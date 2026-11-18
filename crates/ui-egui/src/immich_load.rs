//! Immich in the desktop app: the album list, and an album's photos read on a worker thread (a
//! big album over a network mount takes minutes) and added to the view batch by batch, the way a
//! Local folder is browsed ([`lightcraft_engine::immich`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use lightcraft_engine::activity::{Cancel, TaskGuard};
use lightcraft_engine::immich::{Album, AlbumFiles, Config, Folders, Group};
use lightcraft_engine::immich_retention::Pin;
use lightcraft_engine::import::{ImportJob, ImportMode, ImportOptions, OnDeleted, Prepared};

use crate::LightcraftApp;

/// Files read per batch.
const BATCH: usize = 200;

/// The app's Immich state.
#[derive(Default)]
pub struct State {
    /// The server's albums with their group (`None`: not asked yet), or why they couldn't be listed.
    pub albums: Option<Result<Vec<(Album, Group)>, String>>,
    albums_rx: Option<Receiver<Listing>>,
    /// The server's folders as a tree (empty when they couldn't be listed).
    pub folders: FolderTree,
    /// What is kept offline (read when the list arrives and after a change).
    pub pins: Vec<Pin>,
    /// The album or folder being loaded.
    pub task: Option<Task>,
    /// When the unshown photos' smart previews were last expired.
    expired_at: Option<std::time::Instant>,
    /// Album covers by asset id (`None`: being read, or none could be made).
    pub covers: std::collections::HashMap<String, Option<egui::TextureHandle>>,
    covers_rx: Option<Receiver<(String, Option<egui::ColorImage>)>>,
    /// Where covers are cached (`None`: [`lightcraft_engine::immich::covers_dir`]).
    pub covers_dir: Option<std::path::PathBuf>,
}

type Listing = (Result<Vec<(Album, Group)>, String>, Option<Folders>);

/// Immich's folders under the mapped ones: each mapped folder (as Immich sees it) is a root.
#[derive(Clone, Debug, Default)]
pub struct FolderTree {
    /// (group, root, name shown)
    pub roots: Vec<(Group, String, String)>,
    /// folder → the folders directly inside it, sorted
    pub children: std::collections::HashMap<String, Vec<String>>,
}

impl FolderTree {
    /// The tree of `folders` under `cfg`'s mapped folders.
    pub fn build(cfg: &Config, folders: &Folders) -> FolderTree {
        let mut tree = FolderTree::default();
        let mut children: std::collections::HashMap<String, std::collections::BTreeSet<String>> = std::collections::HashMap::new();
        for (server, local) in &cfg.paths {
            let in_lr = folders.lightroom.iter().any(|f| lightcraft_engine::immich::is_under(f, server));
            let group = if in_lr { Group::Lightroom } else { Group::Other };
            let name = std::path::Path::new(local).file_name().map_or_else(|| server.clone(), |n| n.to_string_lossy().to_string());
            tree.roots.push((group, server.clone(), name));
            for f in folders.lightroom.iter().chain(&folders.other).filter(|f| lightcraft_engine::immich::is_under(f, server)) {
                // every folder between the root and `f`
                let mut parent = server.clone();
                for part in f.get(server.len()..).unwrap_or_default().split('/').filter(|p| !p.is_empty()) {
                    let child = format!("{parent}/{part}");
                    children.entry(parent).or_default().insert(child.clone());
                    parent = child;
                }
            }
        }
        tree.roots.sort_by_key(|r| r.2.to_lowercase());
        tree.children = children.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect();
        tree
    }
}

impl State {
    /// The album list is being fetched.
    pub fn refreshing(&self) -> bool {
        self.albums_rx.is_some()
    }
}

enum Msg {
    Found { root: String, found: AlbumFiles },
    Batch(Box<Prepared>, Vec<String>),
    Failed(String),
    Done,
}

/// An album loading.
pub struct Task {
    id: String,
    name: String,
    rx: Receiver<Msg>,
    cancel: Arc<AtomicBool>,
    opts: ImportOptions,
    now: String,
    found: AlbumFiles,
    done: u64,
    guard: TaskGuard,
    /// an album being kept offline: recorded with its photos when they are all read
    pin: Option<Pin>,
}

/// Ask the server for its albums on a worker thread (nothing without a config).
pub fn refresh_albums(app: &mut LightcraftApp) {
    let Some(cfg) = app.session.immich.clone() else { return };
    let (tx, rx) = channel();
    let spawned = std::thread::Builder::new().name("immich-albums".into()).spawn(move || {
        let cancel = AtomicBool::new(false);
        let albums = lightcraft_engine::immich::albums_with_groups(&cfg, &cancel);
        let folders = lightcraft_engine::immich::folder_tree(&cfg, &cancel).ok();
        let _ = tx.send((albums, folders));
    });
    match spawned {
        Ok(_) => app.immich.albums_rx = Some(rx),
        Err(e) => app.immich.albums = Some(Err(e.to_string())),
    }
}

/// Read the albums' covers on a worker thread (from the cache, else from the server), as small pictures.
fn load_covers(app: &mut LightcraftApp) {
    let Some(cfg) = app.session.immich.clone() else { return };
    let Some(dir) = app.immich.covers_dir.clone().or_else(lightcraft_engine::immich::covers_dir) else { return };
    let wanted: Vec<String> = match &app.immich.albums {
        Some(Ok(a)) => a.iter().filter_map(|(a, _)| a.cover.clone()).filter(|c| !app.immich.covers.contains_key(c)).collect(),
        _ => return,
    };
    if wanted.is_empty() {
        return;
    }
    for w in &wanted {
        app.immich.covers.insert(w.clone(), None);
    }
    let (tx, rx) = channel();
    let spawned = std::thread::Builder::new().name("immich-covers".into()).spawn(move || {
        let cancel = AtomicBool::new(false);
        for asset in wanted {
            let img = lightcraft_engine::immich::cached_cover(&cfg, &asset, &dir, &cancel)
                .ok()
                .and_then(|f| std::fs::read(f).ok())
                .and_then(|b| lightcraft_engine::immich::cover_pixels(&b))
                .map(|px| small(&px, 64));
            if tx.send((asset, img)).is_err() {
                return;
            }
        }
    });
    if spawned.is_ok() {
        app.immich.covers_rx = Some(rx);
    }
}

/// `img` shrunk to at most `edge` pixels on its long side (nearest pixel: a cover is tiny).
fn small(img: &lightcraft_raster::Rgba8, edge: usize) -> egui::ColorImage {
    let long = img.width.max(img.height).max(1);
    let scale = (edge as f64 / long as f64).min(1.0);
    let (w, h) = (((img.width as f64 * scale).round() as usize).max(1), ((img.height as f64 * scale).round() as usize).max(1));
    let mut px = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            let sx = (x * img.width / w).min(img.width.saturating_sub(1));
            let sy = (y * img.height / h).min(img.height.saturating_sub(1));
            px.extend_from_slice(&img.data.get(sy * img.width + sx).copied().unwrap_or([0, 0, 0, 255]));
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([w, h], &px)
}

/// Show album `id`: the view switches at once and fills in as the worker reads the files.
pub fn start(app: &mut LightcraftApp, id: &str, name: &str) -> Result<(), String> {
    let album = id.to_string();
    start_with(app, id, name, None, move |cfg, c| lightcraft_engine::immich::album_files(cfg, &album, c))
}

/// Show the photos directly in Immich folder `path` (as Immich sees it).
pub fn start_folder(app: &mut LightcraftApp, path: &str, name: &str) -> Result<(), String> {
    let folder = path.to_string();
    start_with(app, &format!("folder:{path}"), name, None, move |cfg, c| lightcraft_engine::immich::folder_files(cfg, &folder, c))
}

/// Keep a folder (and the folders in it) or an album offline: it is shown, and its photos' smart previews are
/// fetched and kept for good.
pub fn keep_offline(app: &mut LightcraftApp, pin: Pin, name: &str) -> Result<(), String> {
    let view = match &pin {
        Pin::Folder(f) => {
            // a folder pin needs no list of photos: it is recorded now
            lightcraft_engine::immich_retention::add_pin(&mut app.session, pin.clone());
            format!("folder:{f}")
        }
        Pin::Album { id, .. } => id.clone(),
    };
    app.immich.pins = lightcraft_engine::immich_retention::pins(&app.session);
    let p = pin.clone();
    let album_pin = matches!(pin, Pin::Album { .. }).then_some(pin);
    start_with(app, &view, name, album_pin, move |cfg, c| lightcraft_engine::immich_retention::pin_files(cfg, &p, c))
}

/// Stop keeping `pin` offline (its smart previews then expire like the others).
pub fn stop_keeping(app: &mut LightcraftApp, pin: &Pin) {
    lightcraft_engine::immich_retention::remove_pin(&mut app.session, pin);
    app.immich.pins = lightcraft_engine::immich_retention::pins(&app.session);
}

/// Show view `id` (`name`) and fill it, batch by batch, with the photos `files` lists (on a worker thread). An album
/// `pin` is recorded with the album's photos once they are all read.
fn start_with(
    app: &mut LightcraftApp,
    id: &str,
    name: &str,
    pin: Option<Pin>,
    files: impl FnOnce(&Config, &AtomicBool) -> Result<AlbumFiles, String> + Send + 'static,
) -> Result<(), String> {
    let cfg = app.session.immich.clone().ok_or("Immich is not set up")?;
    stop(app);
    // like a Local browse: read in place, never bringing deleted photos back
    let opts = ImportOptions { mode: ImportMode::Add, local: true, on_deleted: OnDeleted::Skip, ..Default::default() };
    let mut job = ImportJob::new(&mut app.session, opts.clone()).map_err(|e| e.to_string())?;
    let now = job.now().to_string();
    let cancel = Arc::new(AtomicBool::new(false));
    let guard = app.session.activity.start("immich", &format!("Loading Immich album {name}"), Cancel::Flag(cancel.clone()));
    app.session.show_immich_album(id, name, String::new());
    let (tx, rx) = channel();
    let c = cancel.clone();
    // the preview server (CRAFT-9) is told which photos the import will ask it about
    let remote = app.session.immich_previews.as_ref().map(|(rp, _)| rp.clone());
    let smart_dir = app.session.media.smart_dir.clone();
    let work = move || {
        let found = match files(&cfg, &c) {
            Ok(f) => f,
            Err(e) => {
                let _ = tx.send(Msg::Failed(e));
                return;
            }
        };
        if let Some(rp) = &remote {
            rp.register(&found.assets, smart_dir);
        }
        let files = found.files.clone();
        let root = lightcraft_engine::immich::common_root(&files);
        if tx.send(Msg::Found { root, found }).is_err() {
            return;
        }
        for chunk in files.chunks(BATCH) {
            if c.load(Ordering::Relaxed) {
                return;
            }
            let prepared = job.prepare_files(chunk.to_vec(), &c);
            if tx.send(Msg::Batch(Box::new(prepared), chunk.to_vec())).is_err() {
                return;
            }
        }
        let _ = tx.send(Msg::Done);
    };
    std::thread::Builder::new().name("immich-album".into()).spawn(work).map_err(|e| e.to_string())?;
    app.immich.task =
        Some(Task { id: id.to_string(), name: name.to_string(), rx, cancel, opts, now, found: AlbumFiles::default(), done: 0, guard, pin });
    Ok(())
}

/// Stop loading an album (another one was chosen, or the view moved on).
pub fn stop(app: &mut LightcraftApp) {
    if let Some(t) = app.immich.task.take() {
        t.cancel.store(true, Ordering::Relaxed);
    }
}

/// Per frame: take the album list and the album's batches the workers sent.
pub fn tick(app: &mut LightcraftApp, ctx: &egui::Context) {
    // an import or export asked Immich to rescan, and that failed: the photos won't show up there
    if let Some(e) = app.session.immich_rescans.take_error() {
        app.toast_error(ctx, format!("Immich could not rescan: {e}"));
    }
    if let Some(rx) = &app.immich.albums_rx {
        match rx.try_recv() {
            Ok((albums, folders)) => {
                app.immich.albums = Some(albums);
                app.immich.folders = match (&app.session.immich, folders) {
                    (Some(cfg), Some(f)) => FolderTree::build(cfg, &f),
                    _ => FolderTree::default(),
                };
                app.immich.pins = lightcraft_engine::immich_retention::pins(&app.session);
                app.immich.albums_rx = None;
                load_covers(app);
            }
            Err(TryRecvError::Disconnected) => app.immich.albums_rx = None,
            Err(TryRecvError::Empty) => ctx.request_repaint(),
        }
    }
    // once at start and then daily: the smart previews of unpinned Immich photos not shown for the keep days go
    // ponytail: runs on the UI thread (a file check per Immich photo); move to a worker if libraries get huge
    if app.session.immich.is_some()
        && app.session.media.smart_dir.is_some()
        && app.immich.expired_at.is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(86_400))
    {
        app.immich.expired_at = Some(std::time::Instant::now());
        let gone = lightcraft_engine::immich_retention::expire(&mut app.session, lightcraft_engine::immich_retention::now());
        if gone.removed > 0 {
            log::info!("Immich: {} smart previews not shown for a while were deleted ({} bytes)", gone.removed, gone.bytes);
        }
    }
    // smart previews the preview server couldn't make (it went away mid-fetch) are built here
    if app.session.immich_previews.as_ref().is_some_and(|(rp, _)| rp.has_queued()) {
        app.session.build_queued_immich_previews();
    }
    if let Some(rx) = &app.immich.covers_rx {
        loop {
            match rx.try_recv() {
                Ok((asset, img)) => {
                    let tex = img.map(|i| ctx.load_texture(format!("immich-cover-{asset}"), i, egui::TextureOptions::LINEAR));
                    app.immich.covers.insert(asset, tex);
                    ctx.request_repaint();
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    app.immich.covers_rx = None;
                    break;
                }
            }
        }
    }
    let Some(mut task) = app.immich.task.take() else { return };
    // the view moved on: nothing more to add
    let shown = app.session.source == lightcraft_engine::LibrarySource::Immich && app.session.immich_view.as_ref().is_some_and(|v| v.id == task.id);
    if !shown || task.guard.is_cancelled() {
        task.cancel.store(true, Ordering::Relaxed);
        return;
    }
    loop {
        match task.rx.try_recv() {
            Ok(Msg::Found { root, found }) => {
                app.session.show_immich_album(&task.id, &task.name, root);
                task.guard.progress(0, found.files.len() as u64);
                task.found = found;
            }
            Ok(Msg::Batch(prepared, files)) => {
                if let Err(e) = lightcraft_engine::import::commit_prepared(&mut app.session, &task.opts, &task.now, *prepared) {
                    app.toast_error(ctx, format!("Immich album {}: {e}", task.name));
                }
                app.session.add_to_immich_view(&files);
                task.done = task.done.saturating_add(files.len() as u64);
                task.guard.progress(task.done, task.found.files.len() as u64);
            }
            Ok(Msg::Failed(e)) => {
                app.toast_error(ctx, e);
                return;
            }
            Ok(Msg::Done) | Err(TryRecvError::Disconnected) => {
                // photos here before the preview server get theirs now; what it didn't do is built here
                app.session.fetch_missing_immich_previews(&task.found.assets);
                app.session.build_queued_immich_previews();
                // shown now: their smart previews' keep days start again
                let shown = app.session.visible().to_vec();
                lightcraft_engine::immich_retention::touch(&mut app.session, &shown);
                // a pinned album keeps whatever it holds now (photos added since it was pinned too)
                if task.pin.is_none() && app.immich.pins.iter().any(|p| matches!(p, Pin::Album { id, .. } if *id == task.id)) {
                    task.pin = Some(Pin::Album { id: task.id.clone(), paths: Vec::new() });
                }
                if let Some(Pin::Album { id, .. }) = task.pin.take() {
                    lightcraft_engine::immich_retention::add_pin(&mut app.session, Pin::Album { id, paths: task.found.files.clone() });
                    app.immich.pins = lightcraft_engine::immich_retention::pins(&app.session);
                }
                // the album's folders count as browsed: their untouched photos aren't forgotten while it is in use
                let root = app.session.immich_view.as_ref().map(|v| v.root.clone()).unwrap_or_default();
                app.session.note_browsed(&root, &task.found.files);
                let left_out = task.found.missing + task.found.unmapped;
                if left_out > 0 {
                    app.toast(ctx, format!("{left_out} photos of {} aren't on the mounted storage", task.name));
                }
                return;
            }
            Err(TryRecvError::Empty) => break,
        }
    }
    ctx.request_repaint();
    app.immich.task = Some(task);
}
