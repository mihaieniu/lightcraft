//! The preview server's builder: an Immich asset's original, downloaded into a temporary folder, probed and proxied
//! by a headless session exactly as the app would, then deleted.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::json;

use super::{Builder, Built};

/// Build Immich assets of the server `cfg` names, downloading each original into a folder of its own under `tmp`.
pub fn immich_builder(cfg: crate::immich::Config, tmp: PathBuf) -> Builder {
    Arc::new(move |asset: &str| {
        let dir = tmp.join(asset);
        let r = build_one(&cfg, asset, &dir);
        // the original is never kept: the server's disk holds no photos
        let _ = std::fs::remove_dir_all(&dir);
        r
    })
}

fn build_one(cfg: &crate::immich::Config, asset: &str, dir: &std::path::Path) -> Result<Built, String> {
    let cancel = AtomicBool::new(false);
    let client = crate::immich::Client::new(cfg, &cancel);
    let name = client.asset_file_name(asset)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let file = dir.join(name);
    client.download_original(asset, &file, crate::read_file::MAX_MEDIA_BYTES)?;
    let path = file.to_string_lossy().to_string();
    let mut s = crate::Session::new().with_fs();
    let probe_fn = s.media.file_probe.clone().ok_or("no file probe")?;
    let probe = probe_fn(&path)?;
    s.execute("library.import", &json!({"paths": [path]})).map_err(|e| e.to_string())?;
    let id = s.active().ok_or_else(|| format!("asset {asset}: the import added no photo"))?;
    let photo = s.catalog.photo(id).map(|p| (**p).clone()).ok_or_else(|| format!("asset {asset}: the imported photo is gone"))?;
    let smart = crate::cmd::previews::smart_bytes(&mut s, &photo)?;
    Ok(Built { probe, smart })
}
