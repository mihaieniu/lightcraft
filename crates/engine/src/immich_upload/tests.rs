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
    let items: Vec<_> = found
        .iter()
        .map(|name| json!({"originalPath": format!("/mnt/media/Lightroom/{DAY}/{name}"), "id": format!("asset-{name}"), "type": "IMAGE"}))
        .collect();
    r.push(("GET /api/view/folder", 200, json!(items).to_string()));
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

// ---- review fixes ----

#[test]
fn a_copy_never_recreates_the_servers_folder() {
    let d = temp_dir("root-gone");
    std::fs::write(d.join("a.arw"), "x").unwrap();
    let root = d.join("mount-gone");
    assert!(copy_to_nas(&d.join("a.arw"), &root, Path::new("2026/2026-11-18/a.arw")).is_err());
    assert!(!root.exists(), "the mount's folder is never made here");
}

#[test]
fn a_different_photo_of_the_same_size_is_not_taken_for_ours() {
    let (staging, nas, files) = setup("same-size", &["a.arw"]);
    std::fs::create_dir_all(nas.join(DAY)).unwrap();
    // same length as "raw bytes of a.arw", other bytes
    std::fs::write(nas.join(DAY).join("a.arw"), "RAW BYTES OF A.ARW").unwrap();
    let (url, _) = fake_immich(routes(&["a.arw"]));
    let mut job = Job::new(files, staging.to_string_lossy().to_string(), Target::Album("EX".into()));
    let r = go(&cfg_for(&url, &nas), &mut job).unwrap();
    assert_eq!((r.added, r.failed.len()), (0, 1), "{r:?}");
}

#[test]
fn a_staged_copy_gone_is_one_files_failure() {
    let (staging, nas, files) = setup("source-gone", &["a.arw", "b.arw"]);
    std::fs::remove_file(&files[0]).unwrap();
    let (url, _) = fake_immich(routes(&["b.arw"]));
    let mut job = Job::new(files, staging.to_string_lossy().to_string(), Target::Album("EX".into()));
    let r = go(&cfg_for(&url, &nas), &mut job).unwrap();
    assert_eq!((r.added, r.failed.len()), (1, 1), "{r:?}");
    assert!(job.done());
}

#[test]
fn assets_are_found_by_their_exact_path_in_the_day_folder() {
    let (staging, nas, files) = setup("exact", &["a.arw"]);
    // a path search would also match another library's a.arw; the folder listing has only this one
    let (url, seen) = fake_immich(routes(&["a.arw"]));
    let mut job = Job::new(files, staging.to_string_lossy().to_string(), Target::Album("EX".into()));
    assert_eq!(go(&cfg_for(&url, &nas), &mut job).unwrap().added, 1);
    let seen = seen.lock().unwrap();
    assert!(seen.iter().any(|(k, head)| k == "GET /api/view/folder" && head.contains("2026%2F2026-11-18") || head.contains("2026/2026-11-18")));
    assert!(!seen.iter().any(|(k, _)| k == "POST /api/search/metadata"), "no search per file");
}

#[test]
fn saving_jobs_from_two_threads_loses_none() {
    let d = temp_dir("jobs-race");
    let file = d.join("jobs.json");
    let threads: Vec<_> = (0..4)
        .map(|t| {
            let f = file.clone();
            std::thread::spawn(move || {
                for i in 0..10 {
                    update_jobs(&f, |jobs| jobs.push(Job::new(vec![format!("{t}-{i}")], String::new(), Target::Album("x".into())))).unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(load_jobs(&file).len(), 40);
}
