//! Photos imported into an Immich album (CRAFT-11, a fork addition): after an import copied them to this computer,
//! they are copied to the Immich server's Lightroom library folder (keeping their `YYYY/YYYY-MM-DD/` place), Immich is
//! asked to scan it, and once it lists them they are added to an album (made first when it is new).
//!
//! A [`Job`] remembers what is done, so one cut short (the NAS unmounted, the app quit) carries on where it stopped:
//! the app keeps pending jobs in [`jobs_file`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::immich::{Client, Config};

/// Which album the photos go to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    /// An existing album, by id.
    Album(String),
    /// A new album with this name (made as `L_<name>`, described "From Lightroom", like the albums from Lightroom).
    New(String),
}

/// Photos on their way to Immich.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Job {
    /// The copies on this computer.
    pub files: Vec<String>,
    /// The folder they were imported into: their path under it is their path in the Lightroom library.
    pub staging: String,
    pub target: Target,
    /// Per file: on the server.
    pub copied: Vec<bool>,
    /// Per file: its Immich asset, once Immich lists it.
    pub assets: Vec<Option<String>>,
    /// The album (made, for a new one).
    pub album: Option<String>,
    /// Added to the album.
    pub added: bool,
    /// Runs that stopped with an error: after [`MAX_ATTEMPTS`] the job is given up.
    pub attempts: u32,
}

/// Runs a job gets before it is given up (each app start tries the jobs left once).
pub const MAX_ATTEMPTS: u32 = 5;

impl Default for Job {
    fn default() -> Self {
        Job {
            files: Vec::new(),
            staging: String::new(),
            target: Target::New(String::new()),
            copied: Vec::new(),
            assets: Vec::new(),
            album: None,
            added: false,
            attempts: 0,
        }
    }
}

impl Job {
    pub fn new(files: Vec<String>, staging: String, target: Target) -> Self {
        let n = files.len();
        Job { files, staging, target, copied: vec![false; n], assets: vec![None; n], album: None, added: false, attempts: 0 }
    }

    pub fn done(&self) -> bool {
        self.added
    }
}

/// What a run did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    pub copied: usize,
    pub found: usize,
    pub added: usize,
    /// Copied, but Immich didn't list them in time.
    pub missing: Vec<String>,
    /// Not copied: (file, why).
    pub failed: Vec<(String, String)>,
}

/// Where pending jobs are kept.
pub fn jobs_file() -> Option<PathBuf> {
    crate::config::config_dir().map(|d| d.join("immich-uploads.json"))
}

pub fn load_jobs(file: &Path) -> Vec<Job> {
    std::fs::read(file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Every change to the jobs file goes through here, one at a time (the UI queues jobs while a worker records its
/// progress: two load-change-save rounds at once would lose one's change).
pub fn update_jobs(file: &Path, change: impl FnOnce(&mut Vec<Job>)) -> Result<(), String> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _held = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut jobs = load_jobs(file);
    change(&mut jobs);
    save_jobs(file, &jobs)
}

pub fn save_jobs(file: &Path, jobs: &[Job]) -> Result<(), String> {
    let b = serde_json::to_vec_pretty(jobs).map_err(|e| e.to_string())?;
    if let Some(p) = file.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    lightcraft_catalog::safe_file::write_atomic(file, &b).map_err(|e| format!("{}: {e}", file.display()))
}

/// Why a file wasn't copied: this file only (the run goes on), or the server's folder (the run stops and is tried
/// again later).
enum CopyError {
    File(String),
    Stop(String),
}

/// Whether two files hold the same bytes.
fn same_bytes(a: &Path, b: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    let (mut x, mut y) = (std::io::BufReader::new(std::fs::File::open(a)?), std::io::BufReader::new(std::fs::File::open(b)?));
    let (mut p, mut q) = (vec![0u8; 256 * 1024], vec![0u8; 256 * 1024]);
    loop {
        let n = x.read(&mut p)?;
        let mut m = 0;
        while m < n {
            let k = y.read(q.get_mut(m..n).unwrap_or_default())?;
            if k == 0 {
                return Ok(false);
            }
            m += k;
        }
        if n == 0 {
            return Ok(y.read(&mut q)? == 0);
        }
        if p.get(..n) != q.get(..n) {
            return Ok(false);
        }
    }
}

/// Copy `from` to `rel` (plain names only) under the server's folder `root`, as a new file: written as `.part`, then
/// renamed. `root` must be there (the mount): it and anything above it is never made here, only the folders below
/// it. A file already there with the same bytes counts as copied; any other is never overwritten.
fn copy_to_nas(from: &Path, root: &Path, rel: &Path) -> Result<(), CopyError> {
    use std::path::Component;
    if !rel.components().all(|c| matches!(c, Component::Normal(_))) || rel.as_os_str().is_empty() {
        return Err(CopyError::File(format!("{}: not a plain path below the import folder", rel.display())));
    }
    if !root.is_dir() {
        return Err(CopyError::Stop(format!("{} is not there: is the server's storage mounted?", root.display())));
    }
    let size = std::fs::metadata(from).map_err(|e| CopyError::File(format!("{}: {e}", from.display())))?.len();
    let to = root.join(rel);
    if let Ok(there) = std::fs::metadata(&to) {
        return match there.len() == size && same_bytes(from, &to).map_err(|e| CopyError::Stop(format!("{}: {e}", to.display())))? {
            true => Ok(()),
            false => Err(CopyError::File(format!("{} is already on the server and is a different file: left alone", to.display()))),
        };
    }
    // the folders below the root, one by one
    let mut dir = root.to_path_buf();
    if let Some(parent) = rel.parent() {
        for c in parent.components() {
            dir.push(c);
            if !dir.is_dir() {
                std::fs::create_dir(&dir).map_err(|e| CopyError::Stop(format!("{}: {e}", dir.display())))?;
            }
        }
    }
    let part = to.with_extension(format!("{}.part", to.extension().and_then(|e| e.to_str()).unwrap_or("")));
    std::fs::copy(from, &part).map_err(|e| CopyError::Stop(format!("{}: {e}", part.display())))?;
    std::fs::rename(&part, &to).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        CopyError::Stop(format!("{}: {e}", to.display()))
    })
}

/// Run `job`: copy, scan, wait for Immich (every `poll`, at most `timeout`), make the album, add. `progress` gets
/// (done, total, what), `save` the job after each step. An error (the server or its folder unreachable) leaves the
/// job to be run again.
pub fn run(
    cfg: &Config,
    job: &mut Job,
    progress: &dyn Fn(u64, u64, &str),
    save: &dyn Fn(&Job),
    cancel: &AtomicBool,
    poll: Duration,
    timeout: Duration,
) -> Result<Report, String> {
    let client = Client::new(cfg, cancel);
    let mut report = Report::default();
    let lib = client
        .libraries()?
        .into_iter()
        .find(|l| cfg.lightroom_libraries.contains(&l.name))
        .ok_or("Immich has no Lightroom library (immich.json lightroom_libraries)")?;
    let server_root = lib.import_paths.first().cloned().ok_or("the Lightroom library has no folder")?;
    let local_root = cfg.to_local(&server_root).ok_or_else(|| format!("{server_root} is not mapped to a folder here (immich.json paths)"))?;
    let root = PathBuf::from(&local_root);
    let total = job.files.len() as u64;
    let staging = PathBuf::from(&job.staging);
    let rel = |f: &str| Path::new(f).strip_prefix(&staging).ok().map(Path::to_path_buf);
    // 1. copy
    for i in 0..job.files.len() {
        if cancel.load(Ordering::Relaxed) {
            return Err("stopped".into());
        }
        if job.copied.get(i).copied().unwrap_or(false) {
            continue;
        }
        let f = job.files.get(i).cloned().unwrap_or_default();
        let Some(r) = rel(&f) else {
            report.failed.push((f, "not in the import folder".into()));
            continue;
        };
        match copy_to_nas(Path::new(&f), &root, &r) {
            Ok(()) => {
                if let Some(c) = job.copied.get_mut(i) {
                    *c = true;
                }
            }
            Err(CopyError::File(e)) => report.failed.push((f, e)),
            // the mount went away, the disk is full…: the job waits for another run
            Err(CopyError::Stop(e)) => {
                save(job);
                return Err(e);
            }
        }
        progress(i as u64 + 1, total, "copying to the server");
    }
    report.copied = job.copied.iter().filter(|c| **c).count();
    save(job);
    // 2. scan, 3. wait for Immich to list them: one listing per day folder, matched by exact path
    let server = |r: &Path| format!("{}/{}", server_root.trim_end_matches('/'), r.to_string_lossy());
    if job.assets.iter().zip(&job.copied).any(|(a, c)| *c && a.is_none()) {
        client.scan(&lib.id)?;
        let start = Instant::now();
        loop {
            let wanted: Vec<(usize, String)> = (0..job.files.len())
                .filter(|i| job.copied.get(*i).copied().unwrap_or(false) && job.assets.get(*i).is_some_and(Option::is_none))
                .filter_map(|i| job.files.get(i).and_then(|f| rel(f)).map(|r| (i, server(&r))))
                .collect();
            let mut folders: Vec<String> = wanted.iter().filter_map(|(_, p)| p.rsplit_once('/').map(|(d, _)| d.to_string())).collect();
            folders.sort();
            folders.dedup();
            let mut listed = std::collections::HashMap::new();
            for d in folders {
                listed.extend(client.folder_assets(&d)?);
            }
            for (i, p) in wanted {
                if let (Some(id), Some(slot)) = (listed.get(&p).filter(|id| !id.is_empty()), job.assets.get_mut(i)) {
                    *slot = Some(id.clone());
                }
            }
            let found = job.assets.iter().filter(|a| a.is_some()).count();
            progress(found as u64, report.copied as u64, "waiting for Immich");
            if found >= report.copied || start.elapsed() >= timeout || cancel.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(poll);
        }
        save(job);
    }
    report.found = job.assets.iter().filter(|a| a.is_some()).count();
    report.missing =
        job.files.iter().zip(job.copied.iter().zip(&job.assets)).filter(|(_, (c, a))| **c && a.is_none()).map(|(f, _)| f.clone()).collect();
    let ids: Vec<String> = job.assets.iter().flatten().cloned().collect();
    // nothing reached Immich: no album is made for it
    if ids.is_empty() {
        job.added = true;
        save(job);
        return Ok(report);
    }
    // 4. the album, 5. add
    let album = match (&job.target, &job.album) {
        (_, Some(a)) => a.clone(),
        (Target::Album(id), None) => id.clone(),
        (Target::New(name), None) => client.create_album(&format!("L_{name}"), "From Lightroom")?,
    };
    job.album = Some(album.clone());
    save(job);
    client.add_to_album(&album, &ids)?;
    report.added = ids.len();
    job.added = true;
    save(job);
    Ok(report)
}

#[cfg(test)]
mod tests;
