//! Find Subject on a real photo: the people in focus and what they hold are selected, the
//! background isn't.
//!
//! `LIGHTCRAFT_SAM3_DIR=<model dir> LIGHTCRAFT_SUBJECT_PHOTO=<photo.ppm (P6)>
//! LIGHTCRAFT_SUBJECT_CHECKS='{"in": [[x,y],…], "out": [[x,y],…]}' cargo test -p lightcraft-segment
//! --release subject -- --nocapture`. Points are normalized. `LIGHTCRAFT_SUBJECT_OUT=<file.pgm>`
//! writes the mask to look at. Skipped (passes) without them.

use std::path::PathBuf;

use lightcraft_segment::{HELD_CONCEPTS, MASK_SIDE, SUBJECT_CONCEPTS, Sam3};

/// A binary PPM (P6, 8-bit): width, height, RGB.
fn read_ppm(path: &PathBuf) -> (usize, usize, Vec<u8>) {
    let data = std::fs::read(path).unwrap();
    let mut fields = Vec::new();
    let mut i = 0;
    while fields.len() < 4 {
        while data[i].is_ascii_whitespace() {
            i += 1;
        }
        if data[i] == b'#' {
            while data[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        while !data[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(String::from_utf8_lossy(&data[start..i]).to_string());
    }
    assert_eq!(fields[0], "P6", "a binary PPM");
    let (w, h): (usize, usize) = (fields[1].parse().unwrap(), fields[2].parse().unwrap());
    (w, h, data[i + 1..i + 1 + w * h * 3].to_vec())
}

fn points(v: &serde_json::Value, key: &str) -> Vec<(f32, f32)> {
    v[key].as_array().into_iter().flatten().map(|p| (p[0].as_f64().unwrap() as f32, p[1].as_f64().unwrap() as f32)).collect()
}

#[test]
fn real_subject_covers_people_and_what_they_hold() {
    let (Some(dir), Some(photo), Some(checks)) =
        (std::env::var_os("LIGHTCRAFT_SAM3_DIR"), std::env::var_os("LIGHTCRAFT_SUBJECT_PHOTO"), std::env::var("LIGHTCRAFT_SUBJECT_CHECKS").ok())
    else {
        lightcraft_testkit::skip!(Model, "set LIGHTCRAFT_SAM3_DIR, LIGHTCRAFT_SUBJECT_PHOTO and LIGHTCRAFT_SUBJECT_CHECKS");
        return;
    };
    let checks: serde_json::Value = serde_json::from_str(&checks).unwrap();
    let (w, h, rgb) = read_ppm(&PathBuf::from(photo));
    let mut model = Sam3::load(&PathBuf::from(dir)).unwrap();
    let t = std::time::Instant::now();
    let mut enc = model.encode(&rgb, w, h).unwrap();
    eprintln!("encoded {w}×{h} in {:?}", t.elapsed());
    for c in SUBJECT_CONCEPTS.iter().chain(HELD_CONCEPTS) {
        let t = std::time::Instant::now();
        let out = model.detect(&mut enc, c).unwrap();
        let p = 1.0 / (1.0 + (-out.presence).exp());
        let found = out.logits.iter().filter(|l| p / (1.0 + (-**l).exp()) > 0.5).count();
        eprintln!("{c:>8}: {found} found (presence {p:.2}) in {:?}", t.elapsed());
    }
    let t = std::time::Instant::now();
    let s = model.segment_subject(&mut enc, None, 0.5).unwrap().expect("a subject");
    eprintln!("subject in {:?}", t.elapsed());
    let side = MASK_SIDE;
    let at = |(x, y): (f32, f32)| {
        let c = |v: f32| ((v * side as f32) as usize).min(side - 1);
        s.logits[c(y) * side + c(x)]
    };
    if let Some(out) = std::env::var_os("LIGHTCRAFT_SUBJECT_OUT") {
        let mut pgm = format!("P5 {side} {side} 255\n").into_bytes();
        pgm.extend(s.logits.iter().map(|l| if *l > 0.0 { 255u8 } else { 0 }));
        std::fs::write(out, pgm).unwrap();
    }
    let missed: Vec<_> = points(&checks, "in").into_iter().filter(|p| at(*p) <= 0.0).collect();
    let leaked: Vec<_> = points(&checks, "out").into_iter().filter(|p| at(*p) > 0.0).collect();
    assert!(missed.is_empty() && leaked.is_empty(), "missed {missed:?}, leaked into the background {leaked:?}");
}
