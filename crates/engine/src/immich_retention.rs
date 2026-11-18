//! Keep offline, and the 30-day expiry, for the smart previews of Immich photos (CRAFT-9, a fork addition).
//!
//! A pinned folder or album keeps its photos' smart previews for good. Every other Immich photo's smart preview is
//! deleted once the photo has not been shown for the keep days (30 unless set; 0: never), and is fetched again the
//! next time its folder is opened. The pins, the keep days and when each photo was last shown live in
//! `immich-offline.json` in the smart previews folder, next to the proxies (which the smart preview code ignores).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use lightcraft_catalog::{PhotoId, Source};
use serde::{Deserialize, Serialize};

use crate::Session;

const FILE: &str = "immich-offline.json";
pub const DEFAULT_KEEP_DAYS: u32 = 30;
const DAY: i64 = 86_400;

/// What is kept offline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pin {
    /// A folder (or file), as Immich sees it, and everything inside it.
    Folder(String),
    /// An album: its id, and its photos' paths here when it was pinned.
    Album { id: String, paths: Vec<String> },
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Offline {
    pins: Vec<Pin>,
    keep_days: Option<u32>,
    /// smart preview file name → when its photo was last shown (Unix seconds)
    shown: HashMap<String, i64>,
}

/// Smart previews deleted, and their bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Expired {
    pub removed: usize,
    pub bytes: u64,
}

fn file(s: &Session) -> Option<PathBuf> {
    s.media.smart_dir.as_ref().map(|d| d.join(FILE))
}

fn load(s: &Session) -> Offline {
    file(s).and_then(|f| std::fs::read(f).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(s: &Session, o: &Offline) {
    let Some(f) = file(s) else { return };
    let written = serde_json::to_vec(o)
        .map_err(|e| e.to_string())
        .and_then(|b| f.parent().map_or(Ok(()), |p| std::fs::create_dir_all(p).map_err(|e| e.to_string())).map(|()| b))
        .and_then(|b| lightcraft_catalog::safe_file::write_atomic(&f, &b).map_err(|e| e.to_string()));
    if let Err(e) = written {
        log::warn!("{}: {e}", f.display());
    }
}

/// Now, in Unix seconds.
pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The Immich photos of the catalog: (id, smart preview file name, its path, local path).
fn immich_photos(s: &Session) -> Vec<(PhotoId, String, PathBuf, String)> {
    let Some(dir) = s.media.smart_dir.clone() else { return Vec::new() };
    s.catalog
        .photos()
        .filter_map(|p| {
            let Source::File { path } = &p.source else { return None };
            if !s.media.smart_first.iter().any(|r| crate::immich::is_under(path, r)) {
                return None;
            }
            let sp = crate::smart::existing_file_path(&dir, crate::smart::sharded_file_path(&dir, p));
            Some((p.id, crate::smart::file_name(p), sp, path.clone()))
        })
        .collect()
}

fn pinned(s: &Session, o: &Offline, local: &str) -> bool {
    o.pins.iter().any(|pin| match pin {
        Pin::Folder(f) => s.immich.as_ref().and_then(|c| c.to_server(local)).is_some_and(|server| crate::immich::is_under(&server, f)),
        Pin::Album { paths, .. } => paths.iter().any(|p| p == local),
    })
}

/// Photos `ids` were shown at `at` (Unix seconds).
pub fn touch_at(s: &mut Session, ids: &[PhotoId], at: i64) {
    if s.media.smart_dir.is_none() || ids.is_empty() {
        return;
    }
    let mut o = load(s);
    for id in ids {
        if let Some(p) = s.catalog.photo(*id) {
            o.shown.insert(crate::smart::file_name(p), at);
        }
    }
    save(s, &o);
}

/// Photos `ids` were shown now.
pub fn touch(s: &mut Session, ids: &[PhotoId]) {
    touch_at(s, ids, now());
}

pub fn add_pin(s: &mut Session, pin: Pin) {
    let mut o = load(s);
    o.pins.retain(|p| !same_pin(p, &pin));
    o.pins.push(pin);
    save(s, &o);
}

fn same_pin(a: &Pin, b: &Pin) -> bool {
    match (a, b) {
        (Pin::Folder(x), Pin::Folder(y)) => x == y,
        (Pin::Album { id: x, .. }, Pin::Album { id: y, .. }) => x == y,
        _ => false,
    }
}

pub fn remove_pin(s: &mut Session, pin: &Pin) {
    let mut o = load(s);
    o.pins.retain(|p| !same_pin(p, pin));
    save(s, &o);
}

pub fn pins(s: &Session) -> Vec<Pin> {
    load(s).pins
}

/// Days an unshown photo keeps its smart preview (0: for good).
pub fn set_keep_days(s: &mut Session, days: u32) {
    let mut o = load(s);
    o.keep_days = Some(days);
    save(s, &o);
}

pub fn keep_days(s: &Session) -> u32 {
    load(s).keep_days.unwrap_or(DEFAULT_KEEP_DAYS)
}

/// Delete the smart previews of unpinned Immich photos that `stale` says go (given when the photo was last shown).
fn remove(s: &mut Session, now: i64, stale: impl Fn(Option<i64>) -> bool) -> Expired {
    let mut o = load(s);
    let mut out = Expired::default();
    // a smart preview is named by content, so photos with the same bytes share it: it stays while any of them is
    // outside Immich or pinned
    let roots = &s.media.smart_first;
    let kept: std::collections::HashSet<String> = s
        .catalog
        .photos()
        .filter(|p| match &p.source {
            Source::File { path } => !roots.iter().any(|r| crate::immich::is_under(path, r)) || pinned(s, &o, path),
            _ => false,
        })
        .map(|p| crate::smart::file_name(p))
        .collect();
    for (_, key, sp, local) in immich_photos(s) {
        if kept.contains(&key) || pinned(s, &o, &local) || !sp.is_file() {
            continue;
        }
        let shown = o.shown.get(&key).copied();
        if !stale(shown) {
            // never seen before: its clock starts now
            o.shown.entry(key).or_insert(now);
            continue;
        }
        let bytes = std::fs::metadata(&sp).map_or(0, |m| m.len());
        if std::fs::remove_file(&sp).is_ok() {
            out.removed += 1;
            out.bytes += bytes;
            o.shown.remove(&key);
        }
    }
    save(s, &o);
    out
}

/// Delete the smart previews of unpinned Immich photos not shown for the keep days.
pub fn expire(s: &mut Session, now: i64) -> Expired {
    let days = keep_days(s);
    if days == 0 {
        return Expired::default();
    }
    let limit = i64::from(days) * DAY;
    remove(s, now, |shown| shown.is_some_and(|t| now.saturating_sub(t) > limit))
}

/// Delete the smart previews of every unpinned Immich photo.
pub fn clear_temporary(s: &mut Session) -> Expired {
    remove(s, now(), |_| true)
}

/// (count, bytes) of the kept (pinned) and temporary Immich smart previews.
pub fn stats(s: &Session) -> ((usize, u64), (usize, u64)) {
    let o = load(s);
    let (mut kept, mut temp) = ((0, 0), (0, 0));
    for (_, _, sp, local) in immich_photos(s) {
        let Ok(m) = std::fs::metadata(&sp) else { continue };
        let slot = if pinned(s, &o, &local) { &mut kept } else { &mut temp };
        slot.0 += 1;
        slot.1 += m.len();
    }
    (kept, temp)
}

/// Every photo a pin keeps: a folder and the folders inside it, or an album. Network: a worker thread's job.
pub fn pin_files(
    cfg: &crate::immich::Config,
    pin: &crate::immich_retention::Pin,
    cancel: &AtomicBool,
) -> std::result::Result<crate::immich::AlbumFiles, String> {
    match pin {
        Pin::Album { id, .. } => crate::immich::album_files(cfg, id, cancel),
        Pin::Folder(root) => {
            let client = crate::immich::Client::new(cfg, cancel);
            let mut out = crate::immich::AlbumFiles::default();
            for f in client.folders()?.into_iter().filter(|f| crate::immich::is_under(f, root)) {
                let found = crate::immich::folder_files(cfg, &f, cancel)?;
                out.files.extend(found.files);
                out.assets.extend(found.assets);
                out.unmapped += found.unmapped;
                out.missing += found.missing;
                out.skipped += found.skipped;
            }
            Ok(out)
        }
    }
}
