//! Immich photos described and proxied by a preview server (CRAFT-9, a fork addition): while Immich is set up with a
//! `worker`, the session's file probe answers for the Immich photos about to be imported from the server
//! ([`crate::preview_remote`]) and writes the smart preview it sends, so the import never reads the original. A photo
//! the server can't do (it is off, or fails [`crate::preview_remote::MAX_FAILURES`] times in a row) is probed here as
//! before and queued: [`crate::Session::build_queued_immich_previews`] then builds its smart preview here.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use crate::media::FileProbe;
use crate::preview_remote::Client;

/// The server, the photos it may be asked about, and where their smart previews go.
#[derive(Debug)]
pub struct RemotePreviews {
    client: Client,
    /// local path → Immich asset id
    ids: Mutex<HashMap<String, String>>,
    smart_dir: Mutex<Option<PathBuf>>,
    /// Immich photos probed here instead: their smart previews are built here.
    local_queue: Mutex<HashSet<String>>,
}

impl RemotePreviews {
    pub fn new(addr: &str, token: &str) -> Self {
        RemotePreviews { client: Client::new(addr, token), ids: Mutex::default(), smart_dir: Mutex::default(), local_queue: Mutex::default() }
    }

    /// Photos about to be imported (local path, asset id), and the smart previews folder of the open library
    /// (`None`: no library on disk, nothing is asked).
    pub fn register(&self, assets: &[(String, String)], smart_dir: Option<PathBuf>) {
        self.ids.lock().unwrap_or_else(PoisonError::into_inner).extend(assets.iter().cloned());
        *self.smart_dir.lock().unwrap_or_else(PoisonError::into_inner) = smart_dir;
    }

    /// Whether the server stopped being asked (it is off or keeps failing): the laptop builds instead.
    pub fn fallback_active(&self) -> bool {
        !self.client.usable()
    }

    /// The Immich photos probed here since the last call.
    pub fn take_local_queue(&self) -> Vec<String> {
        self.local_queue.lock().unwrap_or_else(PoisonError::into_inner).drain().collect()
    }

    /// Ask the server about `path`: its probe, once its smart preview is written. `None`: not an Immich photo this
    /// was told about, no library on disk, or the server failed.
    fn remote(&self, path: &str) -> Option<crate::media::ProbeInfo> {
        let asset = self.ids.lock().unwrap_or_else(PoisonError::into_inner).get(path).cloned()?;
        let dir = self.smart_dir.lock().unwrap_or_else(PoisonError::into_inner).clone()?;
        if !self.client.usable() {
            return None;
        }
        let built = match self.client.build(&asset) {
            Ok(b) => b,
            Err(e) => {
                log::info!("preview server: {path}: {e}");
                return None;
            }
        };
        let key = built.probe.content_hash.clone()?;
        let to = crate::smart::sharded_path_for_key(&dir, &key);
        let written = to
            .parent()
            .ok_or_else(|| "no parent folder".to_string())
            .and_then(|p| std::fs::create_dir_all(p).map_err(|e| e.to_string()))
            .and_then(|()| lightcraft_catalog::safe_file::write_atomic(&to, &built.smart).map_err(|e| e.to_string()));
        if let Err(e) = written {
            log::warn!("smart preview {}: {e}", to.display());
            return None;
        }
        Some(built.probe)
    }

    fn queue_local(&self, path: &str) {
        if self.ids.lock().unwrap_or_else(PoisonError::into_inner).contains_key(path) {
            self.local_queue.lock().unwrap_or_else(PoisonError::into_inner).insert(path.to_string());
        }
    }
}

/// `native`, except that Immich photos `rp` knows about are answered by the server.
pub fn wrap_probe(native: FileProbe, rp: Arc<RemotePreviews>) -> FileProbe {
    Arc::new(move |path: &str| {
        if let Some(probe) = rp.remote(path) {
            return Ok(probe);
        }
        rp.queue_local(path);
        native(path)
    })
}
