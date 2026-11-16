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
            let (status, out) = routes.iter().find(|(k, _, _)| *k == key).map(|(_, st, b)| (*st, b.clone())).unwrap_or((404, "{}".into()));
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
