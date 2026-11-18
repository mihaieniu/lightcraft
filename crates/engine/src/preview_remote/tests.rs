use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::*;

const TOKEN: &str = "a-test-token-of-some-length";

fn built(asset: &str) -> Built {
    let probe =
        crate::media::ProbeInfo { width: 4, height: 3, format: "jpeg".into(), content_hash: Some(format!("hash-of-{asset}")), ..Default::default() };
    Built { probe, smart: asset.as_bytes().repeat(100) }
}

/// A server on a free loopback port whose builder fails for "bad".
fn start() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let builder: Builder = Arc::new(|asset: &str| if asset == "bad" { Err("cannot decode bad".into()) } else { Ok(built(asset)) });
    std::thread::spawn(move || serve(l, TOKEN.into(), builder, 8, Arc::new(AtomicBool::new(false))));
    addr
}

/// A hand-written server that answers every request line with `reply`, then `extra` raw bytes.
fn fake(reply: serde_json::Value) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(s) = s else { continue };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut w = s;
            let mut line = String::new();
            while r.read_line(&mut line).unwrap_or(0) > 0 {
                let _ = writeln!(w, "{reply}");
                line.clear();
            }
        }
    });
    addr
}

#[test]
fn hello_reports_versions() {
    let h = Client::new(&start(), TOKEN).hello().unwrap();
    assert_eq!((h.v, h.encode, h.look), (PROTOCOL, crate::smart::ENCODE_VERSION, u64::from(crate::camera_preview::LOOK_VERSION)));
    assert!(h.threads >= 1);
}

#[test]
fn wrong_token_is_refused() {
    let e = Client::new(&start(), "another-token-entirely").hello().unwrap_err();
    assert!(e.contains("token"), "{e}");
}

#[test]
fn hello_refuses_other_encode_version() {
    let addr = fake(serde_json::json!({"ok": true, "v": 1, "encode": 999, "look": crate::camera_preview::LOOK_VERSION, "threads": 1}));
    let e = Client::new(&addr, TOKEN).hello().unwrap_err();
    assert!(e.contains("version"), "{e}");
}

#[test]
fn build_returns_probe_and_bytes() {
    let b = Client::new(&start(), TOKEN).build("a1").unwrap();
    assert_eq!(b.probe.content_hash.as_deref(), Some("hash-of-a1"));
    assert_eq!((b.probe.width, b.probe.height), (4, 3));
    assert_eq!(b.smart, built("a1").smart);
}

#[test]
fn undecodable_asset_reports_error_and_batch_continues() {
    let c = Client::new(&start(), TOKEN);
    let e = c.build("bad").unwrap_err();
    assert!(e.contains("cannot decode bad"), "{e}");
    assert_eq!(c.build("good").unwrap().smart, built("good").smart);
    assert!(c.usable());
}

#[test]
fn oversized_payload_is_refused() {
    let addr = fake(serde_json::json!({"ok": true, "asset": "a", "probe": {}, "bytes": MAX_PAYLOAD + 1}));
    let e = Client::new(&addr, TOKEN).build("a").unwrap_err();
    assert!(e.contains("too large"), "{e}");
}

#[test]
fn three_failures_make_client_unusable() {
    // a port nobody listens on
    let addr = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
    let c = Client::new(&addr, TOKEN);
    for _ in 0..3 {
        assert!(c.usable());
        assert!(c.build("a").is_err());
    }
    assert!(!c.usable());
}

// ---- the Immich builder (Task 3) ----

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("lc-previewremote-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn png_bytes(seed: u8) -> Vec<u8> {
    let img = lightcraft_raster::Rgba8::from_fn(48, 32, |x, y| [(x * 5) as u8, (y * 7) as u8, seed, 255]);
    crate::export::encode_image(&img, &crate::export::ExportOptions { format: crate::export::ExportFormat::Png, ..Default::default() }).unwrap()
}

/// A fake Immich serving asset `a`: its description and its original (binary), 404 for anything else.
fn fake_immich_asset(original: Vec<u8>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(s) = s else { continue };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut first = String::new();
            let _ = r.read_line(&mut first);
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
            }
            let path = first.split_whitespace().nth(1).unwrap_or_default().to_string();
            let (status, body): (u16, Vec<u8>) = match path.as_str() {
                "/api/assets/a" => (200, br#"{"id":"a","originalFileName":"IMG_1.png","originalPath":"/mnt/media/x/IMG_1.png"}"#.to_vec()),
                "/api/assets/a/original" => (200, original.clone()),
                _ => (404, b"{}".to_vec()),
            };
            let mut w = s;
            let _ = write!(w, "HTTP/1.1 {status} X\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n", body.len());
            let _ = w.write_all(&body);
        }
    });
    url
}

fn immich_cfg(url: &str) -> crate::immich::Config {
    crate::immich::Config::from_json(&serde_json::json!({"url": url, "paths": {}}).to_string(), Some("test-key".into()), None).unwrap()
}

#[test]
fn immich_builder_matches_local_build() {
    let original = png_bytes(5);
    let tmp = temp_dir("tmp");
    let builder = immich_builder(immich_cfg(&fake_immich_asset(original.clone())), tmp.clone());
    let b = builder("a").unwrap();
    // the same file imported here
    let here = temp_dir("here");
    let file = here.join("IMG_1.png");
    std::fs::write(&file, &original).unwrap();
    let mut s = crate::Session::new().with_fs();
    let local = (s.media.file_probe.clone().unwrap())(&file.to_string_lossy()).unwrap();
    assert_eq!(b.probe.content_hash, local.content_hash);
    assert!(b.probe.content_hash.is_some());
    assert_eq!((b.probe.width, b.probe.height, b.probe.format.as_str()), (local.width, local.height, local.format.as_str()));
    s.execute("library.import", &serde_json::json!({"paths": [file.to_string_lossy()]})).unwrap();
    let id = s.active().unwrap();
    let p = (**s.catalog.photo(id).unwrap()).clone();
    assert_eq!(b.smart, crate::cmd::previews::smart_bytes(&mut s, &p).unwrap());
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(&here);
}

#[test]
fn builder_cleans_temp_dir() {
    let tmp = temp_dir("clean");
    let builder = immich_builder(immich_cfg(&fake_immich_asset(png_bytes(6))), tmp.clone());
    builder("a").unwrap();
    assert!(builder("missing").is_err());
    assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0, "nothing left in {}", tmp.display());
    let _ = std::fs::remove_dir_all(&tmp);
}

// ---- review fixes ----

#[test]
fn a_bad_asset_id_never_reaches_the_builder() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let called = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = called.clone();
    let builder: Builder = Arc::new(move |a: &str| {
        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(built(a))
    });
    std::thread::spawn(move || serve(l, TOKEN.into(), builder, 8, Arc::new(AtomicBool::new(false))));
    let e = Client::new(&addr, TOKEN).build("../..").unwrap_err();
    assert!(e.contains("asset id"), "{e}");
    assert_eq!(called.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn the_immich_builder_deletes_nothing_outside_its_folder() {
    let base = temp_dir("escape");
    let tmp = base.join("tmp");
    std::fs::create_dir_all(base.join("keep")).unwrap();
    std::fs::create_dir_all(&tmp).unwrap();
    let builder = immich_builder(immich_cfg("http://127.0.0.1:1"), tmp);
    assert!(builder("../keep").is_err());
    assert!(base.join("keep").is_dir(), "a folder beside the temp folder survives");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn asset_errors_dont_turn_the_server_off() {
    let c = Client::new(&start(), TOKEN);
    for _ in 0..5 {
        assert!(c.build("bad").is_err());
    }
    assert!(c.usable(), "a server answering about bad files is still worth asking");
}

#[test]
fn a_failed_server_is_tried_again_after_the_cooldown() {
    let addr = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
    let c = Client::new(&addr, TOKEN).with_cooldown(std::time::Duration::from_millis(50));
    for _ in 0..3 {
        let _ = c.build("a");
    }
    assert!(!c.usable());
    std::thread::sleep(std::time::Duration::from_millis(80));
    assert!(c.usable(), "asked again after the cooldown");
}
