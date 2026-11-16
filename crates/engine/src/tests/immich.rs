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
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let c = config(&format!("http://127.0.0.1:{port}"), &[]);
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
    assert_eq!(r["albums"][0], json!({"id": "a", "name": "Trip", "count": 2}));
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
