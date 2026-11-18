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
}

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
        }
    }
}

impl Job {
    pub fn new(files: Vec<String>, staging: String, target: Target) -> Self {
        let n = files.len();
        Job { files, staging, target, copied: vec![false; n], assets: vec![None; n], album: None, added: false }
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

pub fn save_jobs(file: &Path, jobs: &[Job]) -> Result<(), String> {
    let b = serde_json::to_vec_pretty(jobs).map_err(|e| e.to_string())?;
    if let Some(p) = file.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    lightcraft_catalog::safe_file::write_atomic(file, &b).map_err(|e| format!("{}: {e}", file.display()))
}

/// Copy `from` to `to` (a new file only: written as `.part`, then renamed). A file already at `to` of the same size
/// counts as copied; one of another size is never overwritten.
fn copy_to_nas(from: &Path, to: &Path) -> Result<(), String> {
    let size = std::fs::metadata(from).map_err(|e| format!("{}: {e}", from.display()))?.len();
    if let Ok(there) = std::fs::metadata(to) {
        return if there.len() == size {
            Ok(())
        } else {
            Err(format!("{} is already on the server and is a different file: left alone", to.display()))
        };
    }
    if let Some(p) = to.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    let part = to.with_extension(format!("{}.part", to.extension().and_then(|e| e.to_str()).unwrap_or("")));
    std::fs::copy(from, &part).map_err(|e| format!("{}: {e}", part.display()))?;
    std::fs::rename(&part, to).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        format!("{}: {e}", to.display())
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
        .find(|l| cfg.lightroom_libraries.iter().any(|n| *n == l.name))
        .ok_or("Immich has no Lightroom library (immich.json lightroom_libraries)")?;
    let server_root = lib.import_paths.first().cloned().ok_or("the Lightroom library has no folder")?;
    let local_root = cfg.to_local(&server_root).ok_or_else(|| format!("{server_root} is not mapped to a folder here (immich.json paths)"))?;
    // the server's folder must be there: never recreated on this computer's disk when the mount is gone
    if !Path::new(&local_root).is_dir() {
        return Err(format!("{local_root} is not there: is the server's storage mounted?"));
    }
    let total = job.files.len() as u64;
    let rel = |f: &str| f.strip_prefix(&job.staging).map(|r| r.trim_start_matches('/').to_string());
    // 1. copy
    let mut skip = vec![false; job.files.len()];
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
            skip[i] = true;
            continue;
        };
        match copy_to_nas(Path::new(&f), &Path::new(&local_root).join(&r)) {
            Ok(()) => {
                if let Some(c) = job.copied.get_mut(i) {
                    *c = true;
                }
            }
            Err(e) if e.contains("different file") => {
                report.failed.push((f, e));
                skip[i] = true;
            }
            // the mount went away, the disk is full…: the job waits for another run
            Err(e) => {
                save(job);
                return Err(e);
            }
        }
        progress(i as u64 + 1, total, "copying to the server");
    }
    report.copied = job.copied.iter().filter(|c| **c).count();
    save(job);
    // 2. scan, 3. wait for Immich to list them
    if job.assets.iter().zip(&job.copied).any(|(a, c)| *c && a.is_none()) {
        client.scan(&lib.id)?;
        let start = Instant::now();
        loop {
            for i in 0..job.files.len() {
                if !job.copied.get(i).copied().unwrap_or(false) || job.assets.get(i).is_some_and(Option::is_some) {
                    continue;
                }
                let Some(r) = job.files.get(i).and_then(|f| rel(f)) else { continue };
                if let Some(a) = client.find_asset(&format!("{}/{r}", server_root.trim_end_matches('/')))?
                    && let Some(slot) = job.assets.get_mut(i)
                {
                    *slot = Some(a);
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
    // 4. the album, 5. add
    let album = match (&job.target, &job.album) {
        (_, Some(a)) => a.clone(),
        (Target::Album(id), None) => id.clone(),
        (Target::New(name), None) => client.create_album(&format!("L_{name}"), "From Lightroom")?,
    };
    job.album = Some(album.clone());
    save(job);
    let ids: Vec<String> = job.assets.iter().flatten().cloned().collect();
    if !ids.is_empty() {
        client.add_to_album(&album, &ids)?;
    }
    report.added = ids.len();
    job.added = true;
    save(job);
    Ok(report)
}

#[cfg(test)]
mod tests;
