//! The Immich section: hidden without a config; lists the server's albums; a click loads the
//! album's photos on a worker thread (the activity stack shows it) and the view shows exactly them.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use serde_json::json;

use crate::LightcraftApp;
use crate::headless::Headless;

const T: Duration = Duration::from_secs(20);

/// Answers every request with the body of the first route whose `"METHOD /path"` it is.
fn fake_immich(routes: Vec<(&'static str, String)>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut r = BufReader::new(s.try_clone().unwrap());
            let (mut first, mut len) = (String::new(), 0usize);
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if first.is_empty() {
                    first = line.clone();
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            let _ = r.read_exact(&mut body);
            let mut p = first.split_whitespace();
            let key = format!("{} {}", p.next().unwrap_or_default(), p.next().unwrap_or_default());
            let out = routes.iter().find(|(k, _)| *k == key).map(|(_, b)| b.clone()).unwrap_or_else(|| "{}".into());
            let _ = (&s).write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{out}", out.len()).as_bytes());
        }
    });
    url
}

fn png(p: &std::path::Path, seed: u8) {
    let img = lightcraft_raster::Rgba8::from_fn(24, 16, |x, y| [(x * 9) as u8, (y * 12) as u8, seed, 255]);
    let o = lightcraft_engine::export::ExportOptions { format: lightcraft_engine::export::ExportFormat::Png, ..Default::default() };
    std::fs::write(p, lightcraft_engine::export::encode_image(&img, &o).unwrap()).unwrap();
}

fn app(immich: Option<lightcraft_engine::immich::Config>) -> Headless {
    let mut session = lightcraft_engine::Session::new().with_fs();
    session.immich = immich;
    let mut app = LightcraftApp::new(session, crate::Services { png: None, ..Default::default() });
    // as the desktop app opens an empty library: on the grid with the sidebar
    crate::panels::empty_library::open_on_empty_library(&mut app);
    Headless::new(app, [1300.0, 900.0], 1.0)
}

#[test]
fn the_section_is_hidden_without_immich() {
    let mut h = app(None);
    h.settle(T);
    assert!(!h.painted_text().iter().any(|t| t == "Immich"), "no config, no section");
}

#[test]
fn clicking_an_album_shows_exactly_its_photos() {
    let dir = std::env::temp_dir().join(format!("lc-ui-immich-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    png(&dir.join("a.png"), 1);
    png(&dir.join("b.png"), 2);
    let page = json!({"assets": {"items": [{"originalPath": "/srv/a.png"}, {"originalPath": "/srv/gone.png"}], "nextPage": null}}).to_string();
    let url = fake_immich(vec![
        ("GET /api/albums", json!([{"id": "al1", "albumName": "Trip", "assetCount": 2}]).to_string()),
        ("POST /api/search/metadata", page),
    ]);
    let text = json!({"url": url, "paths": {"/srv": dir.to_string_lossy()}}).to_string();
    let cfg = lightcraft_engine::immich::Config::from_json(&text, Some("k".into()), None).unwrap();
    let mut h = app(Some(cfg));
    assert!(h.step_until(T, |h| h.painted_text().iter().any(|t| t == "Trip")), "the album list arrives");
    let r = h.request("ui.clickWidget", json!({"id": "source:immich:al1"}), T);
    assert_eq!(r["ok"], true, "{r}");
    assert!(h.step_until(T, |h| h.app.immich.task.is_none() && h.app.session.source == lightcraft_engine::LibrarySource::Immich));
    let ids = h.app.session.visible().to_vec();
    let names: Vec<String> = ids.iter().map(|id| h.app.session.catalog.photo(*id).unwrap().file_name.clone()).collect();
    assert_eq!(names, vec!["a.png"]);
    h.settle(T);
    assert!(!h.painted_text().iter().any(|t| t == "Your library is empty"), "an album of photos not in the library is not the empty-library screen");
}

#[test]
fn an_album_with_none_of_its_photos_here_says_so() {
    let dir = std::env::temp_dir().join(format!("lc-ui-immich-none-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let page = json!({"assets": {"items": [{"originalPath": "/srv/gone.png"}], "nextPage": null}}).to_string();
    let url = fake_immich(vec![
        ("GET /api/albums", json!([{"id": "al2", "albumName": "Offline", "assetCount": 1}]).to_string()),
        ("POST /api/search/metadata", page),
    ]);
    let text = json!({"url": url, "paths": {"/srv": dir.to_string_lossy()}}).to_string();
    let mut h = app(Some(lightcraft_engine::immich::Config::from_json(&text, Some("k".into()), None).unwrap()));
    assert!(h.step_until(T, |h| h.painted_text().iter().any(|t| t == "Offline")));
    assert_eq!(h.request("ui.clickWidget", json!({"id": "source:immich:al2"}), T)["ok"], true);
    assert!(h.step_until(T, |h| h.app.immich.task.is_none()));
    h.settle(T);
    let text = h.painted_text();
    assert!(!text.iter().any(|t| t == "Your library is empty"), "{text:?}");
    assert!(text.iter().any(|t| t == "None of this album's photos are here"), "{text:?}");
}
