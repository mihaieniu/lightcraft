//! Immich: config, path map, API client against a local fake server (never the internet).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::immich::{Client, Config, parse_albums};

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lc-immich-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A fake Immich: each route is (`"GET /api/albums"`, status, body); a request matches the first route whose key
/// equals its method and path (query left out). Unknown routes answer 404. Returns the base URL and every request
/// seen as `"METHOD /path"` plus the raw head, for asserting headers.
pub(super) fn fake_immich(routes: Vec<(&'static str, u16, String)>) -> (String, Arc<Mutex<Vec<(String, String)>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(s) = s else { continue };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut head = String::new();
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0u8; len];
            let _ = r.read_exact(&mut body);
            let first = head.lines().next().unwrap_or_default();
            let mut parts = first.split_whitespace();
            let key = format!("{} {}", parts.next().unwrap_or_default(), parts.next().unwrap_or_default().split('?').next().unwrap_or_default());
            log.lock().unwrap().push((key.clone(), format!("{head}{}", String::from_utf8_lossy(&body))));
            let body_s = String::from_utf8_lossy(&body).to_string();
            // a route `"POST /p|text"` also needs `text` in the request body
            let hit = |k: &str| match k.split_once('|') {
                Some((route, text)) => route == key && body_s.contains(text),
                None => k == key,
            };
            let (status, out) = routes.iter().find(|(k, _, _)| hit(k)).map(|(_, st, b)| (*st, b.clone())).unwrap_or((404, "{}".into()));
            let resp = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{out}", out.len());
            let _ = (&s).write_all(resp.as_bytes());
        }
    });
    (url, seen)
}

pub(super) fn config(url: &str, paths: &[(&str, &str)]) -> Config {
    let map: serde_json::Map<String, serde_json::Value> = paths.iter().map(|(a, b)| ((*a).to_string(), json!(b))).collect();
    Config::from_json(&json!({"url": url, "paths": map}).to_string(), Some("test-key".into()), None).unwrap()
}

#[test]
fn config_reads_paths_key_file_and_env() {
    let d = temp_dir("config");
    std::fs::write(d.join("key"), "  file-key\n").unwrap();
    let text = json!({"url": "http://h:2283/", "api_key_file": "~/key", "paths": {"/srv/lib/": "~/photos/lib"}}).to_string();
    let c = Config::from_json(&text, None, Some(&d)).unwrap();
    assert_eq!(c.api_key, "file-key");
    assert_eq!(c.url, "http://h:2283");
    assert_eq!(c.to_local("/srv/lib/a.jpg").as_deref(), Some(d.join("photos/lib/a.jpg").to_str().unwrap()));
    let c = Config::from_json(&text, Some("env-key".into()), Some(&d)).unwrap();
    assert_eq!(c.api_key, "env-key", "IMMICH_API_KEY wins over the file");
}

#[test]
fn config_errors_name_what_is_missing() {
    let d = temp_dir("config-errors");
    let e = Config::from_json(&json!({"paths": {}}).to_string(), Some("k".into()), Some(&d)).unwrap_err();
    assert!(e.contains("url"), "{e}");
    let e = Config::from_json(&json!({"url": "http://h", "api_key_file": "~/nope"}).to_string(), None, Some(&d)).unwrap_err();
    assert!(e.contains(&d.join("nope").to_string_lossy().to_string()), "{e}");
    let e = Config::from_json(&json!({"url": "http://h", "paths": {"/a": 1}}).to_string(), Some("k".into()), Some(&d)).unwrap_err();
    assert!(e.contains("paths"), "{e}");
    assert!(Config::from_json("not json", Some("k".into()), Some(&d)).is_err());
}

#[test]
fn map_path_matches_whole_components_only() {
    let c = config("http://h", &[("/mnt/media/Lib", "/l/lib"), ("/mnt/media/Library", "/l/library"), ("/up", "/l/up")]);
    assert_eq!(c.to_local("/mnt/media/Library/a.jpg").as_deref(), Some("/l/library/a.jpg"));
    assert_eq!(c.to_local("/mnt/media/Lib/a.jpg").as_deref(), Some("/l/lib/a.jpg"));
    assert_eq!(c.to_local("/mnt/media/Libx/a.jpg"), None);
    assert_eq!(c.to_local("/elsewhere/a.jpg"), None);
    assert_eq!(c.to_server("/l/up/x/y.jpg").as_deref(), Some("/up/x/y.jpg"));
    assert_eq!(c.to_server("/l/upper/y.jpg"), None);
}

#[test]
fn parse_rejects_malformed_and_wrong_shapes() {
    assert!(parse_albums(&json!({})).is_err());
    assert!(parse_albums(&json!([{"id": 1}])).is_err());
    let a = parse_albums(&json!([{"id": "a", "albumName": "Trip", "assetCount": 3}, {"id": "b", "albumName": "RAW", "assetCount": 0}])).unwrap();
    assert_eq!(a.len(), 2);
    assert_eq!((a[0].id.as_str(), a[0].name.as_str(), a[0].count), ("a", "Trip", 3));
}

#[test]
fn client_lists_albums_and_pages() {
    let page1 = json!({"assets": {"items": [{"originalPath": "/srv/a.jpg"}, {"originalPath": "/srv/b.jpg"}], "nextPage": "2"}}).to_string();
    let (url, seen) = fake_immich(vec![
        ("GET /api/albums", 200, json!([{"id": "a", "albumName": "Trip", "assetCount": 2}]).to_string()),
        ("POST /api/search/metadata", 200, page1),
        ("GET /api/libraries", 200, json!([{"id": "L1", "importPaths": ["/srv"]}]).to_string()),
        ("POST /api/libraries/L1/scan", 204, String::new()),
    ]);
    let c = config(&url, &[]);
    let cancel = AtomicBool::new(false);
    let cl = Client::new(&c, &cancel);
    assert_eq!(cl.albums().unwrap()[0].name, "Trip");
    let (paths, next) = cl.album_page("a", 1).unwrap();
    assert_eq!(paths, vec!["/srv/a.jpg", "/srv/b.jpg"]);
    assert_eq!(next, Some(2));
    assert_eq!(cl.libraries().unwrap()[0].import_paths, vec!["/srv"]);
    cl.scan("L1").unwrap();
    let seen = seen.lock().unwrap();
    assert!(seen.iter().all(|(_, head)| head.contains("x-api-key: test-key\r\n")), "every request carries the key");
    let search = &seen.iter().find(|(k, _)| k == "POST /api/search/metadata").unwrap().1;
    assert!(search.contains(r#""albumIds":["a"]"#) && search.contains(r#""type":"IMAGE""#) && search.contains(r#""page":1"#), "{search}");
}

#[test]
fn client_reports_refused_key_and_connection_refused() {
    let (url, _) = fake_immich(vec![("GET /api/albums", 401, "{}".into())]);
    let c = config(&url, &[]);
    let cancel = AtomicBool::new(false);
    let e = Client::new(&c, &cancel).albums().unwrap_err();
    assert!(e.contains("API key"), "{e}");
    // port 1: a freed ephemeral port could be taken by another test's server meanwhile
    let c = config("http://127.0.0.1:1", &[]);
    let e = Client::new(&c, &cancel).albums().unwrap_err();
    assert!(e.contains("unreachable"), "{e}");
}

#[test]
fn client_caps_body_size() {
    let big = format!("[{}]", "0,".repeat(9 * 1024 * 1024) + "0");
    let (url, _) = fake_immich(vec![("GET /api/albums", 200, big)]);
    let c = config(&url, &[]);
    let cancel = AtomicBool::new(false);
    let e = Client::new(&c, &cancel).albums().unwrap_err();
    assert!(e.contains("too large"), "{e}");
}

fn png(p: &std::path::Path, seed: u8) {
    let img = lightcraft_raster::Rgba8::from_fn(16, 12, |x, y| [(x * 9) as u8, (y * 11) as u8, seed, 255]);
    let b =
        crate::export::encode_image(&img, &crate::export::ExportOptions { format: crate::export::ExportFormat::Png, ..Default::default() }).unwrap();
    std::fs::write(p, b).unwrap();
}

fn page(paths: &[&str], next: Option<&str>) -> String {
    let items: Vec<_> = paths.iter().map(|p| json!({"originalPath": p, "type": "IMAGE"})).collect();
    json!({"assets": {"items": items, "nextPage": next}}).to_string()
}

/// A folder `/srv/lib` (mapped to a temp dir holding a.png, b.png, sub/c.png, v.txt) and an album "a"
/// listing a, sub/c, a path outside the map, a file not on disk and the text file.
fn album_session(tag: &str) -> (crate::Session, PathBuf) {
    let d = temp_dir(tag);
    png(&d.join("a.png"), 1);
    png(&d.join("b.png"), 2);
    std::fs::create_dir_all(d.join("sub")).unwrap();
    png(&d.join("sub/c.png"), 3);
    std::fs::write(d.join("v.txt"), "x").unwrap();
    let body = page(&["/srv/lib/a.png", "/srv/lib/sub/c.png", "/srv/other/x.jpg", "/srv/lib/gone.png", "/srv/lib/v.txt"], None);
    let (url, _) = fake_immich(vec![("POST /api/search/metadata", 200, body)]);
    let mut s = crate::Session::new().with_fs();
    s.immich = Some(config(&url, &[("/srv/lib", d.to_str().unwrap())]));
    (s, d)
}

fn visible_names(s: &mut crate::Session) -> Vec<String> {
    let ids = s.visible().to_vec();
    let mut v: Vec<String> = ids.iter().map(|id| s.catalog.photo(*id).unwrap().file_name.clone()).collect();
    v.sort();
    v
}

#[test]
fn browse_album_shows_only_album_photos() {
    let (mut s, _d) = album_session("only");
    let r = s.execute("immich.browseAlbum", &json!({"id": "a", "name": "Trip"})).unwrap();
    assert_eq!(r["photos"], 2, "{r}");
    assert_eq!(s.source, crate::LibrarySource::Immich);
    assert_eq!(visible_names(&mut s), vec!["a.png", "c.png"]);
    s.source = crate::LibrarySource::All;
    assert!(visible_names(&mut s).is_empty(), "album photos stay out of All Photos (Local)");
}

#[test]
fn browse_album_counts_unmapped_missing_and_skipped() {
    let (mut s, _d) = album_session("counts");
    let r = s.execute("immich.browseAlbum", &json!({"id": "a"})).unwrap();
    assert_eq!((r["unmapped"].as_u64(), r["missing"].as_u64(), r["skipped"].as_u64()), (Some(1), Some(1), Some(1)), "{r}");
}

#[test]
fn browse_album_pages_until_next_page_is_null() {
    let d = temp_dir("pages");
    png(&d.join("a.png"), 1);
    png(&d.join("b.png"), 2);
    let (url, seen) = fake_immich(vec![
        (r#"POST /api/search/metadata|"page":1"#, 200, page(&["/srv/a.png"], Some("2"))),
        (r#"POST /api/search/metadata|"page":2"#, 200, page(&["/srv/b.png"], None)),
    ]);
    let mut s = crate::Session::new().with_fs();
    s.immich = Some(config(&url, &[("/srv", d.to_str().unwrap())]));
    s.execute("immich.browseAlbum", &json!({"id": "a"})).unwrap();
    assert_eq!(visible_names(&mut s), vec!["a.png", "b.png"]);
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[test]
fn an_empty_album_shows_nothing() {
    let d = temp_dir("empty");
    png(&d.join("a.png"), 1);
    let (url, _) = fake_immich(vec![("POST /api/search/metadata", 200, page(&[], None))]);
    let mut s = crate::Session::new().with_fs();
    s.immich = Some(config(&url, &[("/srv", d.to_str().unwrap())]));
    s.execute("library.browse", &json!({"path": d.to_string_lossy()})).unwrap();
    s.execute("immich.browseAlbum", &json!({"id": "a"})).unwrap();
    assert!(visible_names(&mut s).is_empty());
}

#[test]
fn immich_commands_error_when_not_set_up() {
    let mut s = crate::Session::new().with_fs();
    s.immich = None;
    for c in ["immich.albums", "immich.browseAlbum"] {
        let e = s.execute(c, &json!({"id": "a"})).unwrap_err().to_string();
        assert!(e.contains("Immich is not set up") && e.contains("immich.json"), "{c}: {e}");
    }
}

#[test]
fn immich_albums_lists_albums() {
    let (url, _) = fake_immich(vec![("GET /api/albums", 200, json!([{"id": "a", "albumName": "Trip", "assetCount": 2}]).to_string())]);
    let mut s = crate::Session::new().with_fs();
    s.immich = Some(config(&url, &[]));
    let r = s.execute("immich.albums", &json!({})).unwrap();
    assert_eq!(r["albums"][0], json!({"id": "a", "name": "Trip", "count": 2, "group": "other"}));
}

#[test]
fn common_root_of_files() {
    use crate::immich::common_root;
    assert_eq!(common_root(&["/a/b/c.jpg".into(), "/a/d/e.jpg".into()]), "/a");
    assert_eq!(common_root(&["/a/b/c.jpg".into()]), "/a/b");
    assert_eq!(common_root(&["/a/bc/x.jpg".into(), "/a/b/y.jpg".into()]), "/a");
    assert_eq!(common_root(&[]), "");
}

fn scans_seen(seen: &Arc<Mutex<Vec<(String, String)>>>) -> Vec<String> {
    seen.lock().unwrap().iter().map(|(k, _)| k.clone()).filter(|k| k.ends_with("/scan")).collect()
}

/// A session whose Immich has libraries L1 (`/srv/lib`, mapped to a temp dir) and L2 (`/srv/other`).
fn rescan_session(tag: &str) -> (crate::Session, PathBuf, Arc<Mutex<Vec<(String, String)>>>) {
    let d = temp_dir(tag);
    let (url, seen) = fake_immich(vec![
        ("GET /api/libraries", 200, json!([{"id": "L1", "importPaths": ["/srv/lib"]}, {"id": "L2", "importPaths": ["/srv/other"]}]).to_string()),
        ("POST /api/libraries/L1/scan", 204, String::new()),
        ("POST /api/libraries/L2/scan", 204, String::new()),
    ]);
    let mut s = crate::Session::new().with_fs();
    s.immich = Some(config(&url, &[("/srv/lib", d.to_str().unwrap())]));
    s.immich_rescans = crate::immich::Rescans::with_delay(std::time::Duration::ZERO);
    (s, d, seen)
}

#[test]
fn import_into_mapped_folder_requests_scan_of_owning_library() {
    let (mut s, d, seen) = rescan_session("import");
    png(&d.join("new.png"), 4);
    s.execute("library.import", &json!({"paths": [d.join("new.png").to_string_lossy()]})).unwrap();
    s.immich_rescans.wait_idle();
    assert_eq!(scans_seen(&seen), vec!["POST /api/libraries/L1/scan"]);
}

#[test]
fn local_browse_sends_no_rescan() {
    let (mut s, d, seen) = rescan_session("browse");
    png(&d.join("a.png"), 1);
    s.execute("library.browse", &json!({"path": d.to_string_lossy()})).unwrap();
    s.immich_rescans.wait_idle();
    assert!(seen.lock().unwrap().is_empty(), "browsing never talks to Immich");
}

#[test]
fn import_outside_mapped_folders_sends_nothing() {
    let (mut s, _d, seen) = rescan_session("outside");
    let other = temp_dir("outside-other");
    png(&other.join("x.png"), 5);
    s.execute("library.import", &json!({"paths": [other.join("x.png").to_string_lossy()]})).unwrap();
    s.immich_rescans.wait_idle();
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn export_into_mapped_folder_requests_scan() {
    let (s, d, seen) = rescan_session("export");
    let after = s.after_export(&crate::export::ExportOptions::default()).unwrap();
    let out = d.join("Edits/x.jpg").to_string_lossy().to_string();
    let _ = after.run(&[json!({"path": out})], false);
    s.immich_rescans.wait_idle();
    assert_eq!(scans_seen(&seen), vec!["POST /api/libraries/L1/scan"]);
}

#[test]
fn rescan_failure_does_not_fail_import() {
    let d = temp_dir("fail");
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut s = crate::Session::new().with_fs();
    s.immich = Some(config(&format!("http://127.0.0.1:{port}"), &[("/srv/lib", d.to_str().unwrap())]));
    s.immich_rescans = crate::immich::Rescans::with_delay(std::time::Duration::ZERO);
    png(&d.join("n.png"), 6);
    let r = s.execute("library.import", &json!({"paths": [d.join("n.png").to_string_lossy()]})).unwrap();
    assert_eq!(r["imported"].as_array().map(Vec::len), Some(1));
    s.immich_rescans.wait_idle();
}

#[test]
fn a_windowless_exit_waits_for_the_rescan() {
    let (mut s, d, seen) = rescan_session("exit");
    s.immich_rescans = crate::immich::Rescans::with_delay(std::time::Duration::from_millis(300));
    png(&d.join("late.png"), 7);
    s.execute("library.import", &json!({"paths": [d.join("late.png").to_string_lossy()]})).unwrap();
    s.wait_for_photos_imports();
    assert_eq!(scans_seen(&seen), vec!["POST /api/libraries/L1/scan"], "the CLI / MCP exit path sent the rescan before returning");
}

#[test]
fn requests_arriving_steadily_make_one_scan_after_the_last() {
    let (s, d, seen) = rescan_session("steady");
    let rescans = crate::immich::Rescans::with_delay(std::time::Duration::from_millis(300));
    let cfg = s.immich.clone().unwrap();
    for i in 0..3 {
        rescans.request(&cfg, vec![d.join(format!("{i}.png")).to_string_lossy().to_string()], &s.activity);
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    rescans.wait_idle();
    assert_eq!(scans_seen(&seen), vec!["POST /api/libraries/L1/scan"], "one import's batches make one scan");
}

#[test]
fn a_failed_rescan_is_kept_for_the_front_end_to_show() {
    let d = temp_dir("fail-shown");
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cfg = config(&format!("http://127.0.0.1:{port}"), &[("/srv/lib", d.to_str().unwrap())]);
    let rescans = crate::immich::Rescans::with_delay(std::time::Duration::ZERO);
    rescans.request(&cfg, vec![d.join("a.png").to_string_lossy().to_string()], &crate::activity::Activity::default());
    rescans.wait_idle();
    let e = rescans.take_error().expect("the failure is kept");
    assert!(e.contains("unreachable"), "{e}");
    assert!(rescans.take_error().is_none(), "shown once");
}

#[test]
fn a_waiting_rescan_shows_in_the_activity_stack() {
    let (s, d, _seen) = rescan_session("row");
    let rescans = crate::immich::Rescans::with_delay(std::time::Duration::from_millis(300));
    rescans.request(s.immich.as_ref().unwrap(), vec![d.join("a.png").to_string_lossy().to_string()], &s.activity);
    assert!(s.activity.list().iter().any(|t| t.kind == "immich"), "{:?}", s.activity.list());
    rescans.wait_idle();
    assert!(!s.activity.list().iter().any(|t| t.kind == "immich"), "the row goes when the rescan is done");
}

#[test]
fn browsing_an_album_records_its_folders_as_browsed() {
    let (mut s, d) = album_session("stamp");
    s.execute("immich.browseAlbum", &json!({"id": "a"})).unwrap();
    assert!(s.catalog.last_browsed(d.to_str().unwrap()).is_some(), "untouched album photos aren't forgotten while the album is in use");
}

// ---- smart previews from a worker (CRAFT-9) ----

#[test]
fn config_reads_worker_and_token_file() {
    let d = temp_dir("worker-config");
    std::fs::write(d.join("tok"), "  a-long-shared-token-123\n").unwrap();
    let text = json!({"url": "http://h", "worker": {"addr": "10.0.0.5:7991", "token_file": "~/tok"}}).to_string();
    let c = Config::from_json(&text, Some("k".into()), Some(&d)).unwrap();
    let w = c.worker.unwrap();
    assert_eq!((w.addr.as_str(), w.token.as_str()), ("10.0.0.5:7991", "a-long-shared-token-123"));
    assert_eq!(c.lightroom_libraries, vec!["Lightroom".to_string()]);
    assert!(Config::from_json(&json!({"url": "http://h"}).to_string(), Some("k".into()), Some(&d)).unwrap().worker.is_none());
}

/// A page whose items carry ids (`"id-<file name>"`).
fn page_with_ids(paths: &[&str]) -> String {
    let items: Vec<_> = paths
        .iter()
        .map(|p| json!({"originalPath": p, "id": format!("id-{}", p.rsplit('/').next().unwrap().replace('.', "-")), "type": "IMAGE"}))
        .collect();
    json!({"assets": {"items": items, "nextPage": null}}).to_string()
}

/// A real proxy for a small image, as a worker would send.
fn proxy_bytes(seed: u8) -> Vec<u8> {
    let mut img = lightcraft_raster::Rgb32f::new(16, 12);
    img.data.iter_mut().for_each(|px| *px = [0.2, 0.3, f32::from(seed) / 255.0]);
    crate::smart::encode(&img, None).unwrap()
}

/// A worker whose builder answers `answers` builds, then stops listening (the PC went away).
fn worker(answers: usize) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use crate::preview_remote::{Built, serve};
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let built = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let b = built.clone();
    let builder: crate::preview_remote::Builder = Arc::new(move |asset: &str| {
        let n = b.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n >= answers {
            return Err("gone".into());
        }
        let probe = crate::media::ProbeInfo {
            width: 16,
            height: 12,
            format: "png".into(),
            kind: lightcraft_catalog::MediaKind::Image,
            content_hash: Some(format!("{:032x}", n + 1)),
            ..Default::default()
        };
        Ok(Built { probe, smart: proxy_bytes(asset.len() as u8) })
    });
    std::thread::spawn(move || serve(l, "a-long-shared-token-123".into(), builder, 8, Arc::new(AtomicBool::new(false))));
    (addr, built)
}

/// Album "a" of three photos in a mapped temp folder, a library on disk, a native probe that counts its calls,
/// and (with `worker_addr`) the worker set up.
fn worker_session(tag: &str, worker_addr: Option<&str>) -> (crate::Session, Arc<std::sync::atomic::AtomicUsize>, PathBuf) {
    let d = temp_dir(tag);
    for (i, n) in ["a.png", "b.png", "c.png"].iter().enumerate() {
        png(&d.join(n), i as u8);
    }
    let body = page_with_ids(&["/srv/lib/a.png", "/srv/lib/b.png", "/srv/lib/c.png"]);
    let (url, _) = fake_immich(vec![("POST /api/search/metadata", 200, body)]);
    let mut s = crate::Session::new().with_fs();
    s.open_library(d.join("library"), false).unwrap();
    let native = s.media.file_probe.clone().unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = calls.clone();
    s.media.file_probe = Some(Arc::new(move |p: &str| {
        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        native(p)
    }));
    let mut cfg = config(&url, &[("/srv/lib", d.to_str().unwrap())]);
    cfg.worker = worker_addr.map(|a| crate::immich::WorkerConfig { addr: a.into(), token: "a-long-shared-token-123".into() });
    s.set_immich(Some(cfg));
    (s, calls, d)
}

fn smart_count(s: &mut crate::Session) -> usize {
    let ids = s.visible().to_vec();
    ids.iter().filter(|id| crate::cmd::previews::has_smart_preview(s, **id)).count()
}

#[test]
fn browse_with_worker_reads_no_original() {
    let (addr, _) = worker(usize::MAX);
    let (mut s, calls, _d) = worker_session("worker-all", Some(&addr));
    let r = s.execute("immich.browseAlbum", &json!({"id": "a", "name": "Trip"})).unwrap();
    assert_eq!(r["photos"], 3, "{r}");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0, "no original was probed here");
    assert_eq!(smart_count(&mut s), 3, "every photo has the worker's smart preview");
}

#[test]
fn server_dropping_mid_batch_falls_back() {
    let (addr, _) = worker(1);
    let (mut s, calls, _d) = worker_session("worker-drop", Some(&addr));
    let r = s.execute("immich.browseAlbum", &json!({"id": "a", "name": "Trip"})).unwrap();
    assert_eq!(r["photos"], 3, "{r}");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2, "the two the worker didn't answer were probed here");
    // (built in the background)
    for _ in 0..500 {
        if s.execute("library.previewProgress", &json!({})).unwrap()["running"] != true {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(smart_count(&mut s), 3, "and their smart previews were built here");
}

#[test]
fn no_worker_config_keeps_todays_path() {
    let (mut s, calls, _d) = worker_session("worker-none", None);
    s.execute("immich.browseAlbum", &json!({"id": "a", "name": "Trip"})).unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(smart_count(&mut s), 0, "no smart previews are made without a worker");
}

// ---- Immich photos preview from their smart preview; folders; Lightroom grouping (CRAFT-9) ----

#[test]
fn immich_photos_preview_from_smart_preview_when_online() {
    let (mut s, _calls, _d) = worker_session("smart-first", None);
    s.execute("immich.browseAlbum", &json!({"id": "a", "name": "Trip"})).unwrap();
    let id = s.visible().to_vec()[0];
    let p = (**s.catalog.photo(id).unwrap()).clone();
    // a magenta proxy: the original (a dim gradient) is online, yet the preview shows the proxy
    let mut img = lightcraft_raster::Rgb32f::new(16, 12);
    img.data.iter_mut().for_each(|px| *px = [1.0, 0.0, 1.0]);
    let sp = crate::smart::sharded_file_path(s.media.smart_dir.as_ref().unwrap(), &p);
    std::fs::create_dir_all(sp.parent().unwrap()).unwrap();
    std::fs::write(&sp, crate::smart::encode(&img, None).unwrap()).unwrap();
    s.media.forget(id);
    let r = s.render_now(id, 16, 12).unwrap().image;
    let n = r.data.len() as f64;
    // (the original's red is a ramp averaging about 67; the proxy's is full)
    let red = r.data.iter().map(|px| f64::from(px[0])).sum::<f64>() / n;
    assert!(red > 150.0, "the smart preview is shown: mean red {red}");
}

fn folder_session(tag: &str) -> (crate::Session, PathBuf) {
    let d = temp_dir(tag);
    std::fs::create_dir_all(d.join("sub")).unwrap();
    png(&d.join("sub/c.png"), 3);
    png(&d.join("a.png"), 1);
    let (url, _) = fake_immich(vec![
        (
            "GET /api/libraries",
            200,
            json!([
                {"id": "L1", "name": "Lightroom", "importPaths": ["/mnt/media/Lightroom"]},
                {"id": "L2", "name": "GooglePhotos", "importPaths": ["/mnt/media/GooglePhotos"]}
            ])
            .to_string(),
        ),
        (
            "GET /api/view/folder/unique-paths",
            200,
            json!(["/mnt/media/Lightroom/2024", "/mnt/media/GooglePhotos/Trip", "/srv/lib/sub", "/usr/src/app/upload/x"]).to_string(),
        ),
        ("GET /api/view/folder", 200, json!([{"originalPath": "/srv/lib/sub/c.png", "id": "c", "type": "IMAGE"}]).to_string()),
    ]);
    let mut s = crate::Session::new().with_fs();
    s.set_immich(Some(config(&url, &[("/srv/lib", d.to_str().unwrap())])));
    (s, d)
}

#[test]
fn folders_split_by_lightroom_library() {
    let (mut s, _d) = folder_session("folders");
    let r = s.execute("immich.folders", &json!({})).unwrap();
    assert_eq!(r["lightroom"], json!(["/mnt/media/Lightroom/2024"]), "{r}");
    assert_eq!(r["other"], json!(["/mnt/media/GooglePhotos/Trip", "/srv/lib/sub", "/usr/src/app/upload/x"]), "{r}");
}

#[test]
fn album_group_by_majority() {
    use crate::immich::{Group, Library, group_by_majority};
    let c = config("http://h", &[]);
    let libs = vec![
        Library { id: "L1".into(), name: "Lightroom".into(), import_paths: vec!["/mnt/media/Lightroom".into()] },
        Library { id: "L2".into(), name: "GooglePhotos".into(), import_paths: vec!["/mnt/media/GooglePhotos".into()] },
    ];
    let lr = ["/mnt/media/Lightroom/a.nef", "/mnt/media/Lightroom/b.nef", "/mnt/media/GooglePhotos/c.jpg"].map(String::from);
    assert_eq!(group_by_majority(&c, &libs, &lr), Group::Lightroom);
    let other = ["/mnt/media/Lightroom/a.nef", "/usr/src/app/upload/b.jpg", "/mnt/media/GooglePhotos/c.jpg"].map(String::from);
    assert_eq!(group_by_majority(&c, &libs, &other), Group::Other);
    assert_eq!(group_by_majority(&c, &libs, &[]), Group::Other);
}

#[test]
fn browse_folder_shows_its_photos() {
    let (mut s, _d) = folder_session("browse-folder");
    let r = s.execute("immich.browseFolder", &json!({"path": "/srv/lib/sub"})).unwrap();
    assert_eq!(r["photos"], 1, "{r}");
    assert_eq!(s.source, crate::LibrarySource::Immich);
    assert_eq!(visible_names(&mut s), vec!["c.png"]);
}

#[test]
fn albums_carry_their_group() {
    let (url, _) = fake_immich(vec![
        ("GET /api/albums", 200, json!([{"id": "a", "albumName": "Wedding", "assetCount": 2}]).to_string()),
        ("GET /api/libraries", 200, json!([{"id": "L1", "name": "Lightroom", "importPaths": ["/mnt/media/Lightroom"]}]).to_string()),
        ("POST /api/search/metadata", 200, page(&["/mnt/media/Lightroom/a.nef", "/mnt/media/Lightroom/b.nef"], None)),
    ]);
    let mut s = crate::Session::new().with_fs();
    s.set_immich(Some(config(&url, &[])));
    let r = s.execute("immich.albums", &json!({})).unwrap();
    assert_eq!(r["albums"][0]["group"], "lightroom", "{r}");
}

// ---- keep offline and the 30-day expiry (CRAFT-9) ----

const DAY: i64 = 86_400;

/// The worker album (a, b, c, each with a smart preview from the worker), all last shown at `shown`.
fn kept_session(tag: &str, shown: i64) -> (crate::Session, Vec<lightcraft_catalog::PhotoId>, PathBuf) {
    let (addr, _) = worker(usize::MAX);
    let (mut s, _, d) = worker_session(tag, Some(&addr));
    s.execute("immich.browseAlbum", &json!({"id": "a", "name": "Trip"})).unwrap();
    let mut ids = s.visible().to_vec();
    ids.sort_by_key(|id| s.catalog.photo(*id).unwrap().file_name.clone());
    crate::immich_retention::touch_at(&mut s, &ids, shown);
    assert_eq!(smart_count(&mut s), 3);
    (s, ids, d)
}

fn with_proxy(s: &mut crate::Session, ids: &[lightcraft_catalog::PhotoId]) -> Vec<bool> {
    ids.iter().map(|id| crate::cmd::previews::has_smart_preview(s, *id)).collect()
}

#[test]
fn unshown_31_days_is_expired_30_is_kept() {
    let now = 1_800_000_000;
    let (mut s, ids, _d) = kept_session("expire-31", now - 31 * DAY);
    crate::immich_retention::touch_at(&mut s, &ids[..1], now - 30 * DAY);
    let e = crate::immich_retention::expire(&mut s, now);
    assert_eq!(e.removed, 2);
    assert!(e.bytes > 0);
    assert_eq!(with_proxy(&mut s, &ids), vec![true, false, false]);
}

#[test]
fn photo_in_any_pin_survives_expiry() {
    let now = 1_800_000_000;
    let (mut s, ids, d) = kept_session("expire-pins", now - 90 * DAY);
    // a folder pin covers a.png (as Immich sees it); an album pin covers b.png
    crate::immich_retention::add_pin(&mut s, crate::immich_retention::Pin::Folder("/srv/lib/a.png".into()));
    let b = d.join("b.png").to_string_lossy().to_string();
    crate::immich_retention::add_pin(&mut s, crate::immich_retention::Pin::Album { id: "x".into(), paths: vec![b] });
    assert_eq!(crate::immich_retention::expire(&mut s, now).removed, 1);
    assert_eq!(with_proxy(&mut s, &ids), vec![true, true, false]);
}

#[test]
fn keep_days_zero_never_expires() {
    let now = 1_800_000_000;
    let (mut s, ids, _d) = kept_session("expire-never", now - 900 * DAY);
    crate::immich_retention::set_keep_days(&mut s, 0);
    assert_eq!(crate::immich_retention::expire(&mut s, now).removed, 0);
    assert_eq!(with_proxy(&mut s, &ids), vec![true, true, true]);
}

#[test]
fn clear_temporary_keeps_pins() {
    let (mut s, ids, _d) = kept_session("clear-temp", 1_800_000_000);
    s.execute("immich.pin", &json!({"folder": "/srv/lib/c.png", "fetch": false})).unwrap();
    let st = s.execute("immich.cacheStats", &json!({})).unwrap();
    assert_eq!((st["kept"]["count"].as_u64(), st["temporary"]["count"].as_u64()), (Some(1), Some(2)), "{st}");
    let r = s.execute("immich.clearTemporary", &json!({})).unwrap();
    assert_eq!(r["removed"], 2, "{r}");
    assert_eq!(with_proxy(&mut s, &ids), vec![false, false, true]);
}

#[test]
fn pin_fetches_whole_folder() {
    let (addr, _) = worker(usize::MAX);
    let (mut s, d) = folder_session("pin-fetch");
    s.open_library(d.join("library"), false).unwrap();
    let mut cfg = s.immich.clone().unwrap();
    cfg.worker = Some(crate::immich::WorkerConfig { addr, token: "a-long-shared-token-123".into() });
    s.set_immich(Some(cfg));
    // the fake lists sub/c.png for any folder asked about; /srv/lib holds /srv/lib/sub
    let r = s.execute("immich.pin", &json!({"folder": "/srv/lib"})).unwrap();
    assert_eq!(r["photos"], 1, "{r}");
    let c = s.catalog.photos().find(|p| p.file_name == "c.png").map(|p| p.id).expect("c.png fetched");
    assert!(crate::cmd::previews::has_smart_preview(&s, c));
    let st = s.execute("immich.cacheStats", &json!({})).unwrap();
    assert_eq!(st["kept"]["count"], 1, "{st}");
}

#[test]
fn photos_already_here_get_their_smart_previews_from_the_worker() {
    // browsed once without a worker: in the catalog, no smart previews
    let (mut s, _calls, _d) = worker_session("already-here", None);
    s.execute("immich.browseAlbum", &json!({"id": "a", "name": "Trip"})).unwrap();
    assert_eq!(smart_count(&mut s), 0);
    // the worker is set up: opening the album again fetches what is missing (in the background)
    let (addr, built) = worker(usize::MAX);
    let mut cfg = s.immich.clone().unwrap();
    cfg.worker = Some(crate::immich::WorkerConfig { addr, token: "a-long-shared-token-123".into() });
    s.set_immich(Some(cfg));
    s.execute("immich.browseAlbum", &json!({"id": "a", "name": "Trip"})).unwrap();
    for _ in 0..500 {
        if smart_count(&mut s) == 3 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(smart_count(&mut s), 3);
    assert_eq!(built.load(std::sync::atomic::Ordering::SeqCst), 3);
}

// ---- review fixes ----

/// A server that greets with another smart-preview encoding, and counts the builds it is asked for.
fn other_revision_worker() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let builds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let b = builds.clone();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(s) = s else { continue };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut w = s;
            let mut line = String::new();
            while r.read_line(&mut line).unwrap_or(0) > 0 {
                if line.contains("\"build\"") {
                    b.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = writeln!(w, "{}", json!({"ok": false, "error": "not expected"}));
                } else {
                    let _ = writeln!(w, "{}", json!({"ok": true, "v": 1, "encode": 999, "look": 1, "threads": 1}));
                }
                line.clear();
            }
        }
    });
    (addr, builds)
}

#[test]
fn a_server_of_another_revision_is_never_asked_to_build() {
    let (addr, builds) = other_revision_worker();
    let (mut s, calls, _d) = worker_session("other-revision", Some(&addr));
    s.execute("immich.browseAlbum", &json!({"id": "a", "name": "Trip"})).unwrap();
    assert_eq!(builds.load(std::sync::atomic::Ordering::SeqCst), 0, "refused at its greeting");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3, "every photo was probed here");
    assert_eq!(s.execute("immich.cacheStats", &json!({})).unwrap()["pcUnavailable"], true);
}

#[test]
fn when_the_pc_dies_during_the_fetch_the_rest_is_built_here() {
    let (mut s, _calls, _d) = worker_session("fetch-dies", None);
    s.execute("immich.browseAlbum", &json!({"id": "a", "name": "Trip"})).unwrap();
    let (addr, _) = worker(1);
    let mut cfg = s.immich.clone().unwrap();
    cfg.worker = Some(crate::immich::WorkerConfig { addr, token: "a-long-shared-token-123".into() });
    s.set_immich(Some(cfg));
    s.execute("immich.browseAlbum", &json!({"id": "a", "name": "Trip"})).unwrap();
    // the fetch threads finish; what failed is queued and built here on the next pass
    for _ in 0..500 {
        s.build_queued_immich_previews();
        if smart_count(&mut s) == 3 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(smart_count(&mut s), 3);
}

#[test]
fn the_servers_probe_keeps_the_local_files_date() {
    let (addr, _) = worker(usize::MAX);
    let (s, _calls, d) = worker_session("file-date", Some(&addr));
    let a = d.join("a.png").to_string_lossy().to_string();
    s.register_immich_assets(&[(a.clone(), "id-a-png".into())]);
    let probe = (s.media.file_probe.clone().unwrap())(&a).unwrap();
    let meta = std::fs::metadata(&a).unwrap();
    assert_eq!(probe.file_size, meta.len());
    assert!(probe.file_modified.is_some(), "the local file's date, not the server's download time");
    let local = crate::files::modified_iso(&meta);
    assert_eq!(probe.file_modified, local);
}

// ---- album names, dates and year groups (CRAFT-11) ----

fn album(id: &str, name: &str, start: Option<&str>) -> (crate::immich::Album, crate::immich::Group) {
    (
        crate::immich::Album { id: id.into(), name: name.into(), count: 1, start: start.map(String::from), cover: None },
        crate::immich::Group::Lightroom,
    )
}

fn names(albums: &[(crate::immich::Album, crate::immich::Group)], groups: &[(String, Vec<usize>)]) -> Vec<(String, Vec<String>)> {
    groups.iter().map(|(y, ix)| (y.clone(), ix.iter().map(|i| crate::immich::display_name(&albums[*i].0.name).to_string()).collect())).collect()
}

#[test]
fn display_name_and_years_edge_cases() {
    use crate::immich::{AlbumSort, display_name, year_groups};
    assert_eq!(display_name("L_Cununie 25.8.25"), "Cununie 25.8.25");
    assert_eq!(display_name("L_"), "L_", "nothing left: the name as it is");
    assert_eq!(display_name("Lake"), "Lake");
    assert_eq!(display_name("L_L_x"), "L_x", "one prefix only");
    assert!(year_groups(&[], AlbumSort::Date).is_empty());
    let a = vec![album("1", "L_", None), album("2", "x", Some("not a date"))];
    assert_eq!(names(&a, &year_groups(&a, AlbumSort::Date)), vec![("Undated".to_string(), vec!["L_".to_string(), "x".to_string()])]);
}

#[test]
fn albums_sort_by_date_within_years() {
    use crate::immich::{AlbumSort, year_groups};
    let a = vec![
        album("1", "L_Old", Some("2024-05-01T00:00:00.000Z")),
        album("2", "L_Beta", Some("2026-01-02T00:00:00.000Z")),
        album("3", "L_Alpha", Some("2026-06-20T00:00:00.000Z")),
        album("4", "Nodate", None),
    ];
    assert_eq!(
        names(&a, &year_groups(&a, AlbumSort::Date)),
        vec![
            ("2026".to_string(), vec!["Alpha".to_string(), "Beta".to_string()]),
            ("2024".to_string(), vec!["Old".to_string()]),
            ("Undated".to_string(), vec!["Nodate".to_string()]),
        ]
    );
}

#[test]
fn albums_sort_by_name_within_years() {
    use crate::immich::{AlbumSort, year_groups};
    let a = vec![
        album("1", "L_beta", Some("2026-06-20T00:00:00Z")),
        album("2", "Alpha", Some("2026-01-02T00:00:00Z")),
        album("3", "L_Gamma", Some("2026-03-01T00:00:00Z")),
    ];
    assert_eq!(
        names(&a, &year_groups(&a, AlbumSort::Name)),
        vec![("2026".to_string(), vec!["Alpha".to_string(), "beta".to_string(), "Gamma".to_string()])]
    );
}

#[test]
fn parse_albums_reads_start_date() {
    let v = json!([{"id": "a", "albumName": "L_X", "assetCount": 3, "startDate": "2026-06-20T00:00:00.000Z"}, {"id": "b", "albumName": "Y", "assetCount": 0}]);
    let a = parse_albums(&v).unwrap();
    assert_eq!(a[0].start.as_deref(), Some("2026-06-20T00:00:00.000Z"));
    assert_eq!(a[1].start, None);
}

// ---- album covers (CRAFT-11) ----

fn one(id: &str) -> String {
    json!({"assets": {"items": [{"originalPath": "/x.jpg", "id": id}], "nextPage": null}}).to_string()
}

#[test]
fn cover_prefers_favourite_then_five_stars_then_first() {
    let none = json!({"assets": {"items": [], "nextPage": null}}).to_string();
    let (url, _) = fake_immich(vec![
        ("POST /api/search/metadata|isFavorite", 200, one("fav")),
        ("POST /api/search/metadata|rating", 200, one("five")),
        ("POST /api/search/metadata", 200, one("first")),
    ]);
    let c = config(&url, &[]);
    let cancel = AtomicBool::new(false);
    assert_eq!(Client::new(&c, &cancel).album_cover("a").unwrap().as_deref(), Some("fav"));
    let (url, _) = fake_immich(vec![
        ("POST /api/search/metadata|isFavorite", 200, none.clone()),
        ("POST /api/search/metadata|rating", 200, one("five")),
        ("POST /api/search/metadata", 200, one("first")),
    ]);
    let c = config(&url, &[]);
    assert_eq!(Client::new(&c, &cancel).album_cover("a").unwrap().as_deref(), Some("five"));
    let (url, _) = fake_immich(vec![
        ("POST /api/search/metadata|isFavorite", 200, none.clone()),
        ("POST /api/search/metadata|rating", 200, none.clone()),
        ("POST /api/search/metadata", 200, one("first")),
    ]);
    let c = config(&url, &[]);
    assert_eq!(Client::new(&c, &cancel).album_cover("a").unwrap().as_deref(), Some("first"));
    let (url, _) = fake_immich(vec![("POST /api/search/metadata", 200, none)]);
    let c = config(&url, &[]);
    assert_eq!(Client::new(&c, &cancel).album_cover("a").unwrap(), None, "an empty album has no cover");
}

#[test]
fn cover_bytes_are_capped() {
    let d = temp_dir("covers");
    let (url, _) = fake_immich(vec![
        ("GET /api/assets/small/thumbnail", 200, "RIFFxxxxWEBP".to_string()),
        ("GET /api/assets/huge/thumbnail", 200, "x".repeat(crate::immich::MAX_COVER + 1)),
    ]);
    let c = config(&url, &[]);
    let cancel = AtomicBool::new(false);
    let f = crate::immich::cached_cover(&c, "small", &d, &cancel).unwrap();
    assert_eq!(std::fs::read(&f).unwrap(), b"RIFFxxxxWEBP");
    assert!(crate::immich::cached_cover(&c, "huge", &d, &cancel).is_err());
    assert!(!d.join("huge.webp").exists(), "nothing written for a refused cover");
    assert!(crate::immich::cached_cover(&c, "../x", &d, &cancel).is_err(), "never a path");
}
