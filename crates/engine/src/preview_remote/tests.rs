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
