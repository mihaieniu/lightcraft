//! Photos imported into an Immich album, sent to the server in the background (CRAFT-11): the jobs wait in
//! `immich-uploads.json` ([`lightcraft_engine::immich_upload`]) and a worker thread runs them one after another, with
//! a row in the activity stack. A job cut short (the NAS unmounted, the app quit) is picked up again at the next
//! start.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Duration;

use lightcraft_engine::activity::{Cancel, TaskGuard};
use lightcraft_engine::immich_upload::{Job, MAX_ATTEMPTS, Report, load_jobs, run, update_jobs};

use crate::LightcraftApp;

/// How often Immich is asked whether it lists the new photos yet, and for how long.
const POLL: Duration = Duration::from_secs(10);
const TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub(crate) enum Msg {
    Progress(u64, u64, String),
    Finished(Result<Report, String>),
    AllDone,
}

/// The running sender.
pub struct Upload {
    rx: Receiver<Msg>,
    guard: TaskGuard,
}

fn jobs_file(app: &LightcraftApp) -> Option<PathBuf> {
    app.immich.uploads_file.clone().or_else(lightcraft_engine::immich_upload::jobs_file)
}

/// Queue `job` and start sending.
pub fn send(app: &mut LightcraftApp, job: Job) {
    let Some(file) = jobs_file(app) else { return };
    if let Err(e) = update_jobs(&file, |jobs| jobs.push(job)) {
        log::warn!("Immich uploads: {e}");
    }
    start(app, false);
}

/// Run the pending jobs on a worker thread (nothing when one runs, or none are pending). Jobs that failed before
/// are tried again only with `retry` (once per app start).
fn start(app: &mut LightcraftApp, retry: bool) {
    if app.immich.upload.is_some() {
        return;
    }
    let (Some(cfg), Some(file)) = (app.session.immich.clone(), jobs_file(app)) else { return };
    let wanted = move |j: &Job| !j.done() && (retry || j.attempts == 0);
    let pending = load_jobs(&file).iter().filter(|j| wanted(j)).count();
    if pending == 0 {
        return;
    }
    let (tx, rx) = channel();
    let (poll, timeout) = if cfg!(test) { (Duration::from_millis(20), Duration::from_secs(5)) } else { (POLL, TIMEOUT) };
    let spawned = std::thread::Builder::new().name("immich-upload".into()).spawn(move || {
        let cancel = AtomicBool::new(false);
        // each pending job once, in order: one that fails waits for the next start and doesn't hold up the others
        for mut job in load_jobs(&file).into_iter().filter(wanted) {
            let save = |j: &Job| {
                let _ = update_jobs(&file, |all| {
                    if let Some(x) = all.iter_mut().find(|x| x.files == j.files) {
                        *x = j.clone();
                    }
                });
            };
            let progress = |done: u64, total: u64, what: &str| {
                let _ = tx.send(Msg::Progress(done, total, what.to_string()));
            };
            let r = run(&cfg, &mut job, &progress, &save, &cancel, poll, timeout);
            let files = job.files.clone();
            let r = match r {
                // done: off the list
                Ok(rep) => {
                    let _ = update_jobs(&file, |all| all.retain(|x| x.files != files));
                    Ok(rep)
                }
                Err(e) => {
                    job.attempts += 1;
                    let gave_up = job.attempts >= MAX_ATTEMPTS;
                    let _ = update_jobs(&file, |all| {
                        if gave_up {
                            all.retain(|x| x.files != files);
                        } else if let Some(x) = all.iter_mut().find(|x| x.files == files) {
                            x.attempts = job.attempts;
                        }
                    });
                    Err(if gave_up { format!("{e} — given up after {MAX_ATTEMPTS} tries") } else { e })
                }
            };
            let _ = tx.send(Msg::Finished(r));
        }
        let _ = tx.send(Msg::AllDone);
    });
    if spawned.is_ok() {
        let guard = app.session.activity.start("immich", &format!("Sending {pending} import(s) to Immich"), Cancel::No);
        app.immich.upload = Some(Upload { rx, guard });
    }
}

/// Per frame: resume jobs left from before (once), and take what the sender reports.
pub fn tick(app: &mut LightcraftApp, ctx: &egui::Context) {
    if !app.immich.uploads_resumed && app.session.immich.is_some() {
        app.immich.uploads_resumed = true;
        start(app, true);
    }
    let Some(up) = app.immich.upload.take() else { return };
    loop {
        match up.rx.try_recv() {
            Ok(Msg::Progress(done, total, what)) => {
                up.guard.progress(done, total);
                up.guard.detail(&what);
            }
            Ok(Msg::Finished(Ok(r))) => {
                let mut msg = format!("Immich: {} photos added to the album", r.added);
                if !r.missing.is_empty() {
                    msg.push_str(&format!("; {} copied to the server but not listed by Immich (add them to the album there)", r.missing.len()));
                }
                if !r.failed.is_empty() {
                    msg.push_str(&format!("; {} not sent: {}", r.failed.len(), r.failed.iter().map(|(_, why)| why.as_str()).next().unwrap_or("")));
                }
                app.toast(ctx, msg);
                crate::immich_load::refresh_albums(app);
            }
            Ok(Msg::Finished(Err(e))) => app.toast_error(ctx, format!("Sending photos to Immich stopped: {e}")),
            Ok(Msg::AllDone) | Err(TryRecvError::Disconnected) => {
                // a job queued while the sender was finishing is started now (a failed one waits for the next start)
                if let Some(file) = jobs_file(app)
                    && load_jobs(&file).iter().any(|j| !j.done() && j.attempts == 0)
                {
                    start(app, false);
                }
                return;
            }
            Err(TryRecvError::Empty) => break,
        }
    }
    ctx.request_repaint_after(Duration::from_millis(250));
    app.immich.upload = Some(up);
}
