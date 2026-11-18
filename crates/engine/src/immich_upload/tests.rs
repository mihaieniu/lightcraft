use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::tests::immich::{config, fake_immich};

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lc-upload-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

const DAY: &str = "2026/2026-11-18";

/// A staging folder with `names` under `DAY`, and an empty NAS folder mapped as the Lightroom library.
fn setup(tag: &str, names: &[&str]) -> (PathBuf, PathBuf, Vec<String>) {
    let d = temp_dir(tag);
    let (staging, nas) = (d.join("staging"), d.join("nas"));
    std::fs::create_dir_all(staging.join(DAY)).unwrap();
    std::fs::create_dir_all(&nas).unwrap();
    let files = names
        .iter()
        .map(|n| {
            let f = staging.join(DAY).join(n);
            std::fs::write(&f, format!("raw bytes of {n}")).unwrap();
            f.to_string_lossy().to_string()
        })
        .collect();
    (staging, nas, files)
}

fn routes(found: &[&str]) -> Vec<(&'static str, u16, String)> {
    let mut r: Vec<(&'static str, u16, String)> = vec![
        ("GET /api/libraries", 200, json!([{"id": "L1", "name": "Lightroom", "importPaths": ["/mnt/media/Lightroom"]}]).to_string()),
        ("POST /api/libraries/L1/scan", 204, String::new()),
        ("POST /api/albums", 201, json!({"id": "NEW", "albumName": "L_Shoot"}).to_string()),
        ("PUT /api/albums/NEW/assets", 200, "[]".into()),
        ("PUT /api/albums/EX/assets", 200, "[]".into()),
    ];
    for name in found {
        let key: &'static str = Box::leak(format!("POST /api/search/metadata|/mnt/media/Lightroom/{DAY}/{name}").into_boxed_str());
        r.push((key, 200, json!({"assets": {"items": [{"originalPath": format!("/mnt/media/Lightroom/{DAY}/{name}"), "id": format!("asset-{name}")}], "nextPage": null}}).to_string()));
    }
    r.push(("POST /api/search/metadata", 200, json!({"assets": {"items": [], "nextPage": null}}).to_string()));
    r
}

fn go(cfg: &crate::immich::Config, job: &mut Job) -> Result<Report, String> {
    run(cfg, job, &|_, _, _| {}, &|_| {}, &AtomicBool::new(false), Duration::from_millis(10), Duration::from_millis(200))
}

fn cfg_for(url: &str, nas: &Path) -> crate::immich::Config {
    config(url, &[("/mnt/media/Lightroom", nas.to_str().unwrap())])
}

#[test]
fn upload_copies_scans_finds_and_adds() {
    let (staging, nas, files) = setup("all", &["a.arw", "b.arw"]);
    let (url, seen) = fake_immich(routes(&["a.arw", "b.arw"]));
    let mut job = Job::new(files, staging.to_string_lossy().to_string(), Target::Album("EX".into()));
    let r = go(&cfg_for(&url, &nas), &mut job).unwrap();
    assert_eq!((r.copied, r.found, r.added), (2, 2, 2), "{r:?}");
    assert_eq!(std::fs::read(nas.join(DAY).join("a.arw")).unwrap(), b"raw bytes of a.arw");
    let seen = seen.lock().unwrap();
    assert!(seen.iter().any(|(k, _)| k == "POST /api/libraries/L1/scan"), "Immich was asked to scan");
    let put = &seen.iter().find(|(k, _)| k == "PUT /api/albums/EX/assets").unwrap().1;
    assert!(put.contains("asset-a.arw") && put.contains("asset-b.arw"), "{put}");
    assert!(job.done());
}

#[test]
fn new_album_is_created_with_prefix_and_description() {
    let (staging, nas, files) = setup("new", &["a.arw"]);
    let (url, seen) = fake_immich(routes(&["a.arw"]));
    let mut job = Job::new(files, staging.to_string_lossy().to_string(), Target::New("Shoot".into()));
    let r = go(&cfg_for(&url, &nas), &mut job).unwrap();
    assert_eq!(r.added, 1);
    let seen = seen.lock().unwrap();
    let create = &seen.iter().find(|(k, _)| k == "POST /api/albums").unwrap().1;
    assert!(create.contains(r#""albumName":"L_Shoot""#) && create.contains(r#""description":"From Lightroom""#), "{create}");
    assert_eq!(job.album.as_deref(), Some("NEW"));
}

#[test]
fn different_file_on_the_nas_is_left_alone() {
    let (staging, nas, files) = setup("different", &["a.arw", "b.arw"]);
    std::fs::create_dir_all(nas.join(DAY)).unwrap();
    std::fs::write(nas.join(DAY).join("b.arw"), "another photo entirely").unwrap();
    let (url, _) = fake_immich(routes(&["a.arw", "b.arw"]));
    let mut job = Job::new(files, staging.to_string_lossy().to_string(), Target::Album("EX".into()));
    let r = go(&cfg_for(&url, &nas), &mut job).unwrap();
    assert_eq!(r.added, 1, "{r:?}");
    assert_eq!(r.failed.len(), 1);
    assert!(r.failed[0].0.ends_with("b.arw"));
    assert_eq!(std::fs::read(nas.join(DAY).join("b.arw")).unwrap(), b"another photo entirely", "never overwritten");
}

#[test]
fn assets_not_found_are_reported() {
    let (staging, nas, files) = setup("missing", &["a.arw", "b.arw"]);
    let (url, _) = fake_immich(routes(&["a.arw"]));
    let mut job = Job::new(files, staging.to_string_lossy().to_string(), Target::Album("EX".into()));
    let r = go(&cfg_for(&url, &nas), &mut job).unwrap();
    assert_eq!(r.added, 1);
    assert_eq!(r.missing.len(), 1);
    assert!(r.missing[0].ends_with("b.arw"));
}

#[test]
fn upload_resumes_after_a_failure() {
    let (staging, nas, files) = setup("resume", &["a.arw"]);
    let (url, _) = fake_immich(routes(&["a.arw"]));
    // the NAS isn't mounted: nothing there to copy into
    std::fs::remove_dir_all(&nas).unwrap();
    let mut job = Job::new(files, staging.to_string_lossy().to_string(), Target::Album("EX".into()));
    assert!(go(&cfg_for(&url, &nas), &mut job).is_err());
    assert!(!job.done());
    // saved and loaded again, the NAS back: it finishes
    let store = staging.join("jobs.json");
    save_jobs(&store, std::slice::from_ref(&job)).unwrap();
    let mut again = load_jobs(&store).pop().unwrap();
    std::fs::create_dir_all(&nas).unwrap();
    let r = go(&cfg_for(&url, &nas), &mut again).unwrap();
    assert_eq!(r.added, 1);
    assert!(again.done());
}
