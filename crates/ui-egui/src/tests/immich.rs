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
            let route = routes.iter().find(|(k, _)| k.trim_end_matches("~slow") == key);
            if route.is_some_and(|(k, _)| k.ends_with("~slow")) {
                std::thread::sleep(Duration::from_millis(500));
            }
            let out = route.map(|(_, b)| b.clone()).unwrap_or_else(|| "{}".into());
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

fn temp(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("lc-ui-immich-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config_for(url: &str, dir: &std::path::Path) -> lightcraft_engine::immich::Config {
    let text = json!({"url": url, "paths": {"/srv": dir.to_string_lossy()}}).to_string();
    lightcraft_engine::immich::Config::from_json(&text, Some("k".into()), None).unwrap()
}

fn visible_names(h: &mut Headless) -> Vec<String> {
    let ids = h.app.session.visible().to_vec();
    ids.iter().map(|id| h.app.session.catalog.photo(*id).unwrap().file_name.clone()).collect()
}

#[test]
fn loading_an_album_shows_in_the_activity_stack_and_marks_its_folder_browsed() {
    let dir = temp("row");
    png(&dir.join("a.png"), 1);
    let page = json!({"assets": {"items": [{"originalPath": "/srv/a.png"}], "nextPage": null}}).to_string();
    let url = fake_immich(vec![
        ("GET /api/albums", json!([{"id": "al1", "albumName": "Trip", "assetCount": 1}]).to_string()),
        ("POST /api/search/metadata~slow", page),
    ]);
    let mut h = app(Some(config_for(&url, &dir)));
    assert!(h.step_until(T, |h| h.painted_text().iter().any(|t| t == "Trip")));
    assert_eq!(h.request("ui.clickWidget", json!({"id": "source:immich:al1"}), T)["ok"], true);
    assert!(h.app.session.activity.list().iter().any(|t| t.kind == "immich"), "{:?}", h.app.session.activity.list());
    assert!(h.step_until(T, |h| h.app.immich.task.is_none()));
    assert!(!h.app.session.activity.list().iter().any(|t| t.kind == "immich"), "the row goes when the album is in");
    assert!(h.app.session.catalog.last_browsed(dir.to_str().unwrap()).is_some(), "untouched album photos aren't forgotten while the album is in use");
}

#[test]
fn choosing_another_album_mid_load_shows_only_that_one() {
    let dir = temp("switch");
    png(&dir.join("a.png"), 1);
    png(&dir.join("b.png"), 2);
    let url = fake_immich(vec![
        (
            "GET /api/albums",
            json!([{"id": "A", "albumName": "First", "assetCount": 1}, {"id": "B", "albumName": "Second", "assetCount": 1}]).to_string(),
        ),
        // every search answers slowly with the same page: the album id is in the request body, which this fake ignores
        ("POST /api/search/metadata~slow", json!({"assets": {"items": [{"originalPath": "/srv/a.png"}], "nextPage": null}}).to_string()),
    ]);
    let mut h = app(Some(config_for(&url, &dir)));
    assert!(h.step_until(T, |h| h.painted_text().iter().any(|t| t == "Second")));
    assert_eq!(h.request("ui.clickWidget", json!({"id": "source:immich:A"}), T)["ok"], true);
    assert_eq!(h.request("ui.clickWidget", json!({"id": "source:immich:B"}), T)["ok"], true);
    assert!(h.step_until(T, |h| h.app.immich.task.is_none()));
    assert_eq!(h.app.session.immich_view.as_ref().map(|v| v.id.as_str()), Some("B"), "the view is the album chosen last");
    assert_eq!(h.app.session.activity.list().iter().filter(|t| t.kind == "immich").count(), 0, "the first album's load was stopped");
    assert_eq!(visible_names(&mut h), vec!["a.png"]);
}

#[test]
fn a_failed_rescan_is_shown() {
    let dir = temp("rescan-fail");
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cfg = config_for(&format!("http://127.0.0.1:{port}"), &dir);
    let mut h = app(Some(cfg.clone()));
    h.app.session.immich_rescans = lightcraft_engine::immich::Rescans::with_delay(Duration::ZERO);
    let rescans = h.app.session.immich_rescans.clone();
    rescans.request(&cfg, vec![dir.join("x.png").to_string_lossy().to_string()], &h.app.session.activity);
    rescans.wait_idle();
    assert!(h.step_until(T, |h| h.painted_text().iter().any(|t| t.contains("Immich could not rescan"))), "{:?}", h.painted_text());
}

/// Immich heads My Photos (CRAFT-9): above the library's sections, split into Lightroom and Phone & cloud, each with
/// its folders and albums; a folder's menu keeps it offline.
#[test]
fn immich_section_is_first_with_groups_and_pins() {
    let dir = temp("groups");
    std::fs::create_dir_all(dir.join("lr/2024")).unwrap();
    std::fs::create_dir_all(dir.join("gp/Trip")).unwrap();
    png(&dir.join("lr/2024/a.png"), 1);
    let url = fake_immich(vec![
        (
            "GET /api/albums",
            json!([{"id": "al1", "albumName": "Wedding", "assetCount": 1}, {"id": "al2", "albumName": "Phone Stuff", "assetCount": 1}]).to_string(),
        ),
        (
            "GET /api/libraries",
            json!([
                {"id": "L1", "name": "Lightroom", "importPaths": ["/mnt/media/Lightroom"]},
                {"id": "L2", "name": "GooglePhotos", "importPaths": ["/mnt/media/GooglePhotos"]}
            ])
            .to_string(),
        ),
        (
            "POST /api/search/metadata",
            json!({"assets": {"items": [{"originalPath": "/mnt/media/Lightroom/2024/a.png", "id": "a"}], "nextPage": null}}).to_string(),
        ),
        ("GET /api/view/folder/unique-paths", json!(["/mnt/media/Lightroom/2024", "/mnt/media/GooglePhotos/Trip"]).to_string()),
    ]);
    let text = json!({"url": url, "paths": {"/mnt/media/Lightroom": dir.join("lr").to_string_lossy(), "/mnt/media/GooglePhotos": dir.join("gp").to_string_lossy()}}).to_string();
    let cfg = lightcraft_engine::immich::Config::from_json(&text, Some("k".into()), None).unwrap();
    let mut session = lightcraft_engine::Session::new().with_fs();
    session.open_library(dir.join("library"), false).unwrap();
    session.set_immich(Some(cfg));
    let mut app = LightcraftApp::new(session, crate::Services { png: None, ..Default::default() });
    crate::panels::empty_library::open_on_empty_library(&mut app);
    let mut h = Headless::new(app, [1300.0, 900.0], 1.0);
    assert!(h.step_until(T, |h| h.painted_text().iter().any(|t| t == "Wedding")), "{:?}", h.painted_text());
    h.settle(T);
    let painted = h.painted_text();
    for want in ["Lightroom", "Phone & cloud", "Wedding", "Phone Stuff"] {
        assert!(painted.iter().any(|t| t == want), "{want} in {painted:?}");
    }
    // Immich is drawn above the library's own sections
    let immich = crate::control::widget_rect(&h.app, "button:immichRefresh").expect("the Immich header");
    let albums = crate::control::widget_rect(&h.app, "sidebarSection:albums").expect("the Albums header");
    assert!(immich.top() < albums.top(), "Immich {immich:?} above Albums {albums:?}");
    // the album of Lightroom photos is under Lightroom (the fake answers every album with the same Lightroom page)
    let lightroom_header = crate::control::widget_rect(&h.app, "text:immichGroup:lightroom").unwrap();
    let wedding = crate::control::widget_rect(&h.app, "source:immich:al1").unwrap();
    let phone_header = crate::control::widget_rect(&h.app, "text:immichGroup:other").unwrap();
    assert!(lightroom_header.top() < wedding.top() && wedding.top() < phone_header.top());
    // and the folders under the group of their library
    let lr_root = crate::control::widget_rect(&h.app, "source:immich:folder:/mnt/media/Lightroom").unwrap();
    let gp_root = crate::control::widget_rect(&h.app, "source:immich:folder:/mnt/media/GooglePhotos").unwrap();
    assert!(lr_root.top() < phone_header.top() && phone_header.top() < gp_root.top());
    // Keep Offline from a folder's menu
    let folder = "source:immich:folder:/mnt/media/Lightroom";
    assert_eq!(h.request("ui.clickWidget", json!({"id": folder, "button": "right"}), T)["ok"], true);
    h.settle(T);
    assert_eq!(h.request("ui.clickWidget", json!({"id": "button:immichPin"}), T)["ok"], true);
    assert!(h.step_until(T, |h| h.app.immich.task.is_none()));
    let st = h.app.session.execute("immich.cacheStats", &json!({})).unwrap();
    assert_eq!(st["pins"], json!([{"folder": "/mnt/media/Lightroom"}]), "{st}");
}

/// Albums show without `L_`, under year headings, in the order chosen, each with its cover (CRAFT-11).
#[test]
fn albums_show_covers_names_and_years() {
    let dir = temp("covers");
    let covers = dir.join("covers");
    std::fs::create_dir_all(&covers).unwrap();
    // the cover of "L_Trip", already cached (a real WebP)
    let img = lightcraft_raster::Rgba8::from_fn(8, 8, |x, y| [(x * 30) as u8, (y * 30) as u8, 200, 255]);
    std::fs::write(
        covers.join("c1.webp"),
        lightcraft_codecs::encode_webp_lossless(&lightcraft_codecs::EncodeImage::rgba8(&img), &lightcraft_codecs::EncodeMeta::default()).unwrap(),
    )
    .unwrap();
    let url = fake_immich(vec![
        (
            "GET /api/albums",
            json!([
                {"id": "a1", "albumName": "L_Trip", "assetCount": 1, "startDate": "2026-06-20T00:00:00.000Z"},
                {"id": "a2", "albumName": "L_Apples", "assetCount": 1, "startDate": "2026-01-02T00:00:00.000Z"},
                {"id": "a3", "albumName": "L_Zoo", "assetCount": 1, "startDate": "2024-05-01T00:00:00.000Z"}
            ])
            .to_string(),
        ),
        ("POST /api/search/metadata", json!({"assets": {"items": [{"originalPath": "/srv/a.png", "id": "c1"}], "nextPage": null}}).to_string()),
    ]);
    let text = json!({"url": url, "paths": {"/srv": dir.to_string_lossy()}}).to_string();
    let mut session = lightcraft_engine::Session::new().with_fs();
    session.set_immich(Some(lightcraft_engine::immich::Config::from_json(&text, Some("k".into()), None).unwrap()));
    let mut app = LightcraftApp::new(session, crate::Services { png: None, ..Default::default() });
    app.immich.covers_dir = Some(covers);
    crate::panels::empty_library::open_on_empty_library(&mut app);
    let mut h = Headless::new(app, [1300.0, 900.0], 1.0);
    assert!(h.step_until(T, |h| h.painted_text().iter().any(|t| t == "Trip")), "{:?}", h.painted_text());
    h.settle(T);
    let text = h.painted_text();
    assert!(!text.iter().any(|t| t.starts_with("L_")), "no L_ shown: {text:?}");
    let top = |h: &Headless, id: &str| crate::control::widget_rect(&h.app, id).unwrap_or_else(|| panic!("{id}")).top();
    // by date: 2026 (Trip, then Apples), then 2024 (Zoo)
    assert!(top(&h, "text:immichYear:other:2026") < top(&h, "source:immich:a1"));
    assert!(top(&h, "source:immich:a1") < top(&h, "source:immich:a2"));
    assert!(top(&h, "source:immich:a2") < top(&h, "text:immichYear:other:2024"));
    assert!(top(&h, "text:immichYear:other:2024") < top(&h, "source:immich:a3"));
    assert!(h.step_until(T, |h| crate::control::widget_rect(&h.app, "cover:immich:a1").is_some()), "the cover is drawn");
    // by name: within 2026, Apples before Trip
    assert_eq!(h.request("engine.execute", json!({"command": "view.immichAlbumSort", "params": {"sort": "name"}}), T)["ok"], true);
    assert_eq!(h.app.ui.immich_album_sort, lightcraft_engine::immich::AlbumSort::Name);
    h.settle(T);
    assert!(top(&h, "source:immich:a2") < top(&h, "source:immich:a1"));
}
