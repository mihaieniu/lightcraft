//! Immich in the desktop app: the album list, and an album's photos read on a worker thread (a
//! big album over a network mount takes minutes) and added to the view batch by batch, the way a
//! Local folder is browsed ([`lightcraft_engine::immich`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use lightcraft_engine::activity::{Cancel, TaskGuard};
use lightcraft_engine::immich::{Album, AlbumFiles};
use lightcraft_engine::import::{ImportJob, ImportMode, ImportOptions, OnDeleted, Prepared};

use crate::LightcraftApp;

/// Files read per batch.
const BATCH: usize = 200;

/// The app's Immich state.
#[derive(Default)]
pub struct State {
    /// The server's albums (`None`: not asked yet), or why they couldn't be listed.
    pub albums: Option<Result<Vec<Album>, String>>,
    albums_rx: Option<Receiver<Result<Vec<Album>, String>>>,
    /// The album being loaded.
    pub task: Option<Task>,
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
}

/// Ask the server for its albums on a worker thread (nothing without a config).
pub fn refresh_albums(app: &mut LightcraftApp) {
    let Some(cfg) = app.session.immich.clone() else { return };
    let (tx, rx) = channel();
    let spawned = std::thread::Builder::new().name("immich-albums".into()).spawn(move || {
        let _ = tx.send(lightcraft_engine::immich::Client::new(&cfg, &AtomicBool::new(false)).albums());
    });
    match spawned {
        Ok(_) => app.immich.albums_rx = Some(rx),
        Err(e) => app.immich.albums = Some(Err(e.to_string())),
    }
}

/// Show album `id`: the view switches at once and fills in as the worker reads the files.
pub fn start(app: &mut LightcraftApp, id: &str, name: &str) -> Result<(), String> {
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
    let (c, album) = (cancel.clone(), id.to_string());
    let work = move || {
        let found = match lightcraft_engine::immich::album_files(&cfg, &album, &c) {
            Ok(f) => f,
            Err(e) => {
                let _ = tx.send(Msg::Failed(e));
                return;
            }
        };
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
    app.immich.task = Some(Task { id: id.to_string(), name: name.to_string(), rx, cancel, opts, now, found: AlbumFiles::default(), done: 0, guard });
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
    if let Some(rx) = &app.immich.albums_rx {
        match rx.try_recv() {
            Ok(r) => {
                app.immich.albums = Some(r);
                app.immich.albums_rx = None;
            }
            Err(TryRecvError::Disconnected) => app.immich.albums_rx = None,
            Err(TryRecvError::Empty) => ctx.request_repaint(),
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
