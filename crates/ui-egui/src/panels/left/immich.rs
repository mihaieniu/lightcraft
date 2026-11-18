//! Immich: the head of My Photos (CRAFT-9). Shown only when Immich is set up, above the library's own sections, in
//! two groups: Lightroom (the libraries `immich.json` names) and Phone & cloud (the others and the phone uploads),
//! each with its folders ([`crate::immich_load::FolderTree`]) and its albums. A row's menu keeps it offline: its
//! photos' smart previews are fetched and kept for good (a dot marks it). A click on the header lists everything again.

use lightcraft_engine::immich::Group;
use lightcraft_engine::immich_retention::Pin;

use super::rows::{INDENT_STEP, Row, tree_open, tree_toggle};
use super::sections::SECTION_GAP;
use super::{Align2, Icon, LibrarySource, LightcraftApp, Sense, Tokens, pos2, register, vec2};

pub(super) fn section(app: &mut LightcraftApp, ui: &mut egui::Ui) {
    if app.session.immich.is_none() {
        return;
    }
    if app.immich.albums.is_none() && !app.immich.refreshing() {
        crate::immich_load::refresh_albums(app);
    }
    let t = Tokens::get(ui.ctx());
    let (header, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::click());
    ui.painter().text(pos2(header.left() + 18.0, header.center().y), Align2::LEFT_CENTER, "Immich", t.semibold(12.0), t.text_dim);
    let status = if app.immich.refreshing() { "Loading…" } else { "↻" };
    ui.painter().text(pos2(header.right() - 18.0, header.center().y), Align2::RIGHT_CENTER, status, t.font(11.0), t.text_dim);
    register(ui, "button:immichRefresh", header);
    if resp.on_hover_text("List the Immich folders and albums again").clicked() && !app.immich.refreshing() {
        crate::immich_load::refresh_albums(app);
    }
    // the preview server is off or failing: smart previews are built here, slower
    if app.session.immich_previews.as_ref().is_some_and(|(rp, _)| rp.fallback_active()) {
        let r =
            ui.add(egui::Label::new(egui::RichText::new("Building previews on this computer (PC unavailable)").size(11.0).color(t.text_dim)).wrap());
        register(ui, "text:immichFallback", r.rect);
    }
    let albums = match &app.immich.albums {
        Some(Ok(a)) => a.clone(),
        Some(Err(e)) => {
            let e = format!("Immich unreachable: {e}");
            ui.add(egui::Label::new(egui::RichText::new(e).size(11.0).color(t.text_dim)).wrap());
            ui.add_space(SECTION_GAP);
            return;
        }
        None => {
            ui.add_space(SECTION_GAP);
            return;
        }
    };
    let current = app.session.immich_view.as_ref().filter(|_| app.session.source == LibrarySource::Immich).map(|v| v.id.clone());
    let roots = app.immich.folders.roots.clone();
    for (group, label, key) in [(Group::Lightroom, "Lightroom", "lightroom"), (Group::Other, "Phone & cloud", "other")] {
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
        ui.painter().text(pos2(r.left() + 18.0, r.center().y), Align2::LEFT_CENTER, label, t.semibold(11.0), t.text_dim);
        register(ui, format!("text:immichGroup:{key}"), r);
        for (_, root, name) in roots.iter().filter(|(g, _, _)| *g == group) {
            folder_rows(app, ui, root, name, 0.0, current.as_deref());
        }
        for (a, _) in albums.iter().filter(|(_, g)| *g == group) {
            let count = usize::try_from(a.count).ok();
            let pinned = app.immich.pins.iter().any(|p| matches!(p, Pin::Album { id, .. } if *id == a.id));
            let resp = Row::new(&format!("immich:{}", a.id), Icon::Album, &a.name)
                .count(count)
                .selected(current.as_deref() == Some(a.id.as_str()))
                .mark(pinned.then_some(t.accent))
                .show(app, ui);
            pin_menu(app, &resp, Pin::Album { id: a.id.clone(), paths: Vec::new() }, &a.name, pinned);
            if resp.clicked()
                && let Err(e) = crate::immich_load::start(app, &a.id, &a.name)
            {
                app.toast_error(ui.ctx(), e);
            }
        }
    }
    ui.add_space(SECTION_GAP);
    let (line, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(line.left() + 18.0..=(line.right() - 18.0).max(line.left() + 18.0), line.center().y, egui::Stroke::new(1.0, t.divider));
}

/// Folder `path` (as Immich sees it) shown as `name`, and, when open, the folders inside it.
fn folder_rows(app: &mut LightcraftApp, ui: &mut egui::Ui, path: &str, name: &str, indent: f32, current: Option<&str>) {
    let children = app.immich.folders.children.get(path).cloned().unwrap_or_default();
    let open_id = egui::Id::unique(("immich-folder-open", path.to_string()));
    let mut open = tree_open(ui, open_id, false);
    let pinned = app.immich.pins.iter().any(|p| matches!(p, Pin::Folder(f) if f == path));
    let view = format!("folder:{path}");
    let t = Tokens::get(ui.ctx());
    let resp = Row::new(&format!("immich:{view}"), Icon::Folder, name)
        .selected(current == Some(view.as_str()))
        .indent(indent)
        .mark(pinned.then_some(t.accent))
        .show(app, ui);
    let mut toggled = false;
    if !children.is_empty() {
        let tr = tree_toggle(ui, &resp, indent, open_id, &mut open, format!("immichFolderToggle:{path}"), name);
        toggled = tr.clicked();
        pin_menu(app, &tr, Pin::Folder(path.to_string()), name, pinned);
    }
    pin_menu(app, &resp, Pin::Folder(path.to_string()), name, pinned);
    if resp.on_hover_text(path).clicked()
        && !toggled
        && let Err(e) = crate::immich_load::start_folder(app, path, name)
    {
        app.toast_error(ui.ctx(), e);
    }
    if open {
        for child in children {
            let n = child.rsplit('/').next().unwrap_or(&child).to_string();
            folder_rows(app, ui, &child, &n, indent + INDENT_STEP, current);
        }
    }
}

/// A row's menu: keep it offline, or stop.
fn pin_menu(app: &mut LightcraftApp, resp: &egui::Response, pin: Pin, name: &str, pinned: bool) {
    crate::widgets::context_menu(resp, |ui| {
        let label = if pinned { "Don't Keep Offline" } else { "Keep Offline" };
        let r = ui.button(label);
        register(ui, "button:immichPin", r.rect);
        if r.clicked() {
            if pinned {
                crate::immich_load::stop_keeping(app, &pin);
            } else if let Err(e) = crate::immich_load::keep_offline(app, pin.clone(), name) {
                app.toast_error(ui.ctx(), e);
            }
        }
    });
}
