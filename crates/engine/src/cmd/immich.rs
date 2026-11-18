//! Immich albums ([`crate::immich`]): list them, and show one the way Local shows a folder.

use std::sync::atomic::AtomicBool;

use serde_json::{Value, json};

use super::{CommandSpec, always, bad, cmd, str_param};
use crate::import::{ImportMode, ImportOptions, OnDeleted, import_with};
use crate::{Result, Session};

fn config<'a>(s: &'a Session, c: &str) -> Result<&'a crate::immich::Config> {
    s.immich.as_ref().ok_or_else(|| {
        let file = crate::immich::Config::file().map(|f| f.display().to_string()).unwrap_or_else(|| "lightcraft/immich.json".into());
        bad(c, format!("Immich is not set up: create {file} (see docs/immich.md)"))
    })
}

fn albums(s: &mut Session, _: &Value) -> Result<Value> {
    const C: &str = "immich.albums";
    let cfg = config(s, C)?;
    let list = crate::immich::albums_with_groups(cfg, &AtomicBool::new(false)).map_err(|e| bad(C, e))?;
    Ok(json!({"albums": list.iter().map(|(a, g)| json!({"id": a.id, "name": a.name, "count": a.count, "group": g})).collect::<Vec<_>>()}))
}

fn browse_album(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "immich.browseAlbum";
    let cfg = config(s, C)?.clone();
    let id = str_param(p, "id").filter(|i| !i.is_empty()).ok_or_else(|| bad(C, "missing `id`"))?;
    let name = str_param(p, "name").unwrap_or("Immich Album");
    let found = crate::immich::album_files(&cfg, id, &AtomicBool::new(false)).map_err(|e| bad(C, e))?;
    show_files(s, id, name, found)
}

fn folders(s: &mut Session, _: &Value) -> Result<Value> {
    const C: &str = "immich.folders";
    let cfg = config(s, C)?;
    let f = crate::immich::folder_tree(cfg, &AtomicBool::new(false)).map_err(|e| bad(C, e))?;
    Ok(json!(f))
}

fn browse_folder(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "immich.browseFolder";
    let cfg = config(s, C)?.clone();
    let path = str_param(p, "path").filter(|i| !i.is_empty()).ok_or_else(|| bad(C, "missing `path`"))?;
    let name = str_param(p, "name").map(str::to_string).unwrap_or_else(|| path.rsplit('/').next().unwrap_or(path).to_string());
    let found = crate::immich::folder_files(&cfg, path, &AtomicBool::new(false)).map_err(|e| bad(C, e))?;
    show_files(s, &format!("folder:{path}"), &name, found)
}

/// Show Immich's photos `found` as view `id`, imported in place (the preview server describing them when there is one).
fn show_files(s: &mut Session, id: &str, name: &str, found: crate::immich::AlbumFiles) -> Result<Value> {
    // like library.browse: read in place, never bringing deleted photos back
    let opts = ImportOptions { mode: ImportMode::Add, local: true, on_deleted: OnDeleted::Skip, ..Default::default() };
    s.register_immich_assets(&found.assets);
    let report = import_with(s, &found.files, &opts)?;
    s.fetch_missing_immich_previews(&found.assets);
    s.build_queued_immich_previews();
    let root = crate::immich::common_root(&found.files);
    // the album's folders count as browsed: their untouched photos aren't forgotten while it is in use
    s.note_browsed(&root, &found.files);
    s.show_immich_album(id, name, root);
    s.add_to_immich_view(&found.files);
    // shown now: their smart previews' 30 days start again
    let shown = s.visible().to_vec();
    crate::immich_retention::touch(s, &shown);
    Ok(json!({
        "id": id,
        "photos": s.visible().len(),
        "new": report.imported.len(),
        "failed": report.failed.len(),
        "unmapped": found.unmapped,
        "missing": found.missing,
        "skipped": found.skipped,
    }))
}

/// The pin `p` names (`folder` as Immich sees it, or `album` id).
fn pin_param(c: &str, p: &Value) -> Result<crate::immich_retention::Pin> {
    use crate::immich_retention::Pin;
    match (str_param(p, "folder").filter(|f| !f.is_empty()), str_param(p, "album").filter(|a| !a.is_empty())) {
        (Some(f), None) => Ok(Pin::Folder(f.to_string())),
        (None, Some(a)) => Ok(Pin::Album { id: a.to_string(), paths: Vec::new() }),
        _ => Err(bad(c, "give `folder` or `album`")),
    }
}

/// Import `found` in place without showing it (the preview server describing it when there is one).
pub fn fetch_files(s: &mut Session, found: &crate::immich::AlbumFiles) -> Result<usize> {
    let opts = ImportOptions { mode: ImportMode::Add, local: true, on_deleted: OnDeleted::Skip, ..Default::default() };
    s.register_immich_assets(&found.assets);
    import_with(s, &found.files, &opts)?;
    s.fetch_missing_immich_previews(&found.assets);
    s.build_queued_immich_previews();
    Ok(found.files.len())
}

fn pin(s: &mut Session, p: &Value) -> Result<Value> {
    use crate::immich_retention::Pin;
    const C: &str = "immich.pin";
    let cfg = config(s, C)?.clone();
    let mut pin = pin_param(C, p)?;
    let fetch = p.get("fetch").and_then(Value::as_bool).unwrap_or(true);
    let found = if fetch || matches!(pin, Pin::Album { .. }) {
        Some(crate::immich_retention::pin_files(&cfg, &pin, &AtomicBool::new(false)).map_err(|e| bad(C, e))?)
    } else {
        None
    };
    if let (Pin::Album { paths, .. }, Some(f)) = (&mut pin, &found) {
        paths.clone_from(&f.files);
    }
    crate::immich_retention::add_pin(s, pin);
    let photos = match (&found, fetch) {
        (Some(f), true) => fetch_files(s, f)?,
        (Some(f), false) => f.files.len(),
        _ => 0,
    };
    Ok(json!({"pinned": true, "photos": photos}))
}

fn unpin(s: &mut Session, p: &Value) -> Result<Value> {
    let pin = pin_param("immich.unpin", p)?;
    crate::immich_retention::remove_pin(s, &pin);
    Ok(json!({"pinned": false}))
}

fn cache_stats(s: &mut Session, _: &Value) -> Result<Value> {
    let ((kc, kb), (tc, tb)) = crate::immich_retention::stats(s);
    Ok(json!({
        "kept": {"count": kc, "bytes": kb},
        "temporary": {"count": tc, "bytes": tb},
        "keepDays": crate::immich_retention::keep_days(s),
        "pins": crate::immich_retention::pins(s),
        "pcUnavailable": s.immich_previews.as_ref().is_some_and(|(rp, _)| rp.fallback_active()),
    }))
}

fn ids_param(s: &Session, p: &Value) -> Vec<lightcraft_catalog::PhotoId> {
    p.get("ids")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_u64).map(lightcraft_catalog::PhotoId).filter(|id| s.catalog.photo(*id).is_some()).collect())
        .unwrap_or_default()
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(query "immich.albums", "Immich Albums", [], None, "{} — the Immich server's albums (needs lightcraft/immich.json), each in the group most of its photos are in → {albums: [{id, name, count, group: lightroom|other}]}", always, albums),
        cmd!(
            "immich.browseAlbum",
            "Show Immich Album",
            [],
            None,
            "{id, name?} — show an Immich album's photos from the mounted storage without adding them to the library (read in place; edits go to XMP sidecars) → {id, photos, new, failed, unmapped, missing, skipped}",
            always,
            browse_album
        ),
        cmd!(
            "immich.pin",
            "Keep Offline",
            [],
            None,
            "{folder? (as Immich sees it) | album?, fetch?: bool = true} — keep a folder (and the folders in it) or an album's smart previews for good; with fetch, import its photos now (the preview server builds their smart previews) → {pinned, photos}",
            always,
            pin
        ),
        cmd!(
            "immich.unpin",
            "Don't Keep Offline",
            [],
            None,
            "{folder? | album?} — stop keeping a folder's or album's smart previews for good (they then expire like the others) → {pinned: false}",
            always,
            unpin
        ),
        cmd!(query "immich.cacheStats", "Immich Offline Cache", [], None, "{} — the Immich photos' smart previews kept for good and kept for now, the keep days, the pins, and whether the preview server is unavailable → {kept: {count, bytes}, temporary: {count, bytes}, keepDays, pins, pcUnavailable}", always, cache_stats),
        cmd!(
            "immich.clearTemporary",
            "Clear Temporary Immich Previews",
            [],
            None,
            "{} — delete the smart previews of every Immich photo no pin keeps → {removed, bytes}",
            always,
            |s, _| Ok(json!(crate::immich_retention::clear_temporary(s)))
        ),
        cmd!(
            "immich.expire",
            "Expire Immich Previews",
            [],
            None,
            "{} — delete the smart previews of Immich photos no pin keeps that were not shown for the keep days → {removed, bytes}",
            always,
            |s, _| Ok(json!(crate::immich_retention::expire(s, crate::immich_retention::now())))
        ),
        cmd!(
            "immich.keepDays",
            "Keep Immich Previews For",
            [],
            None,
            "{days} — days an Immich photo not shown keeps its smart preview (0: for good; default 30) → {keepDays}",
            always,
            |s, p| {
                let days = p
                    .get("days")
                    .and_then(Value::as_u64)
                    .and_then(|d| u32::try_from(d).ok())
                    .ok_or_else(|| bad("immich.keepDays", "give `days` (0 or more)"))?;
                crate::immich_retention::set_keep_days(s, days);
                Ok(json!({"keepDays": days}))
            }
        ),
        cmd!(
            "immich.touch",
            "Immich Photos Shown",
            [],
            None,
            "{ids} — these Immich photos were shown now: their smart previews' keep days start again → {}",
            always,
            |s, p| {
                let ids = ids_param(s, p);
                crate::immich_retention::touch(s, &ids);
                Ok(json!({}))
            }
        ),
        cmd!(query "immich.folders", "Immich Folders", [], None, "{} — the Immich server's folders, as it sees them, split into the Lightroom libraries (immich.json lightroom_libraries) and the rest → {lightroom: [path], other: [path]}", always, folders),
        cmd!(
            "immich.browseFolder",
            "Show Immich Folder",
            [],
            None,
            "{path, name?} — show the photos directly in an Immich folder (path as Immich sees it) from the mounted storage, as immich.browseAlbum does → {id, photos, new, failed, unmapped, missing, skipped}",
            always,
            browse_folder
        ),
    ]
}
