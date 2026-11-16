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
    let list = crate::immich::Client::new(cfg, &AtomicBool::new(false)).albums().map_err(|e| bad(C, e))?;
    Ok(json!({"albums": list.iter().map(|a| json!({"id": a.id, "name": a.name, "count": a.count})).collect::<Vec<_>>()}))
}

fn browse_album(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "immich.browseAlbum";
    let cfg = config(s, C)?.clone();
    let id = str_param(p, "id").filter(|i| !i.is_empty()).ok_or_else(|| bad(C, "missing `id`"))?;
    let name = str_param(p, "name").unwrap_or("Immich Album");
    let found = crate::immich::album_files(&cfg, id, &AtomicBool::new(false)).map_err(|e| bad(C, e))?;
    // like library.browse: read in place, never bringing deleted photos back
    let opts = ImportOptions { mode: ImportMode::Add, local: true, on_deleted: OnDeleted::Skip, ..Default::default() };
    let report = import_with(s, &found.files, &opts)?;
    let root = crate::immich::common_root(&found.files);
    // the album's folders count as browsed: their untouched photos aren't forgotten while it is in use
    s.note_browsed(&root, &found.files);
    s.show_immich_album(id, name, root);
    s.add_to_immich_view(&found.files);
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

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(query "immich.albums", "Immich Albums", [], None, "{} — the Immich server's albums (needs lightcraft/immich.json) → {albums: [{id, name, count}]}", always, albums),
        cmd!(
            "immich.browseAlbum",
            "Show Immich Album",
            [],
            None,
            "{id, name?} — show an Immich album's photos from the mounted storage without adding them to the library (read in place; edits go to XMP sidecars) → {id, photos, new, failed, unmapped, missing, skipped}",
            always,
            browse_album
        ),
    ]
}
