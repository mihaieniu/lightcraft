//! The building blocks of smart previews built on another computer (CRAFT-9): a proxy's file name from a content key
//! alone, a probe that crosses the wire as JSON, and one photo's proxy bytes equal to what Build Smart Previews writes.

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::Session;

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lc-smartremote-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn png(p: &Path, seed: u8) {
    let img = lightcraft_raster::Rgba8::from_fn(48, 32, |x, y| [(x * 5) as u8, (y * 7) as u8, seed, 255]);
    let b =
        crate::export::encode_image(&img, &crate::export::ExportOptions { format: crate::export::ExportFormat::Png, ..Default::default() }).unwrap();
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, b).unwrap();
}

#[test]
fn file_name_for_key_matches_photo_name() {
    let mut p = lightcraft_catalog::Photo::new(
        lightcraft_catalog::PhotoId(1),
        lightcraft_catalog::Source::File { path: "/x/a.jpg".into() },
        "a.jpg",
        "jpeg",
        30,
        20,
        "2026-01-01T00:00:00",
    );
    p.content_hash = Some("ab".repeat(16));
    assert_eq!(crate::smart::file_name(&p), crate::smart::file_name_for_key(&"ab".repeat(16)));
    let dir = Path::new("/sp");
    assert_eq!(crate::smart::sharded_file_path(dir, &p), crate::smart::sharded_path_for_key(dir, &"ab".repeat(16)));
}

#[test]
fn probe_info_round_trips_json() {
    let mut info = crate::media::ProbeInfo { width: 30, height: 20, format: "jpeg".into(), file_size: 99, ..Default::default() };
    info.content_hash = Some("cd".repeat(16));
    info.xmp = Some("<x/>".into());
    info.orientation = Some(lightcraft_geom::Orientation::Rotate90);
    info.meta.make = "Nikon".into();
    let text = serde_json::to_string(&info).unwrap();
    let back: crate::media::ProbeInfo = serde_json::from_str(&text).unwrap();
    assert_eq!(serde_json::to_value(&back).unwrap(), serde_json::to_value(&info).unwrap());
}

#[test]
fn smart_bytes_equals_built_file() {
    let src = temp_dir("src");
    let lib = temp_dir("lib");
    png(&src.join("a.png"), 9);
    let mut s = Session::new().with_fs();
    s.open_library(&lib, false).unwrap();
    s.execute("library.import", &json!({"paths": [src.join("a.png").to_string_lossy()]})).unwrap();
    let id = s.active().unwrap();
    let r = s.execute("library.smartPreviews", &json!({})).unwrap();
    assert_eq!(r["built"], 1, "{r}");
    let p = (**s.catalog.photo(id).unwrap()).clone();
    let dir = s.media.smart_dir.clone().unwrap();
    let written = std::fs::read(crate::smart::sharded_file_path(&dir, &p)).unwrap();
    assert_eq!(crate::cmd::previews::smart_bytes(&mut s, &p).unwrap(), written);
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&lib);
}
