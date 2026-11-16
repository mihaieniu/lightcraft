//! Immich: the server's albums ([`crate::immich_load`]), below the other sections; shown only when
//! Immich is set up. A click on the header lists the albums again.

use super::rows::Row;
use super::sections::SECTION_GAP;
use super::{Align2, Icon, LibrarySource, LightcraftApp, Sense, Tokens, pos2, register, vec2};

pub(super) fn section(app: &mut LightcraftApp, ui: &mut egui::Ui) {
    if app.session.immich.is_none() {
        return;
    }
    if app.immich.albums.is_none() && !app.immich.refreshing() {
        crate::immich_load::refresh_albums(app);
    }
    ui.add_space(SECTION_GAP);
    let t = Tokens::get(ui.ctx());
    let (header, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::click());
    ui.painter().hline(
        header.left() + 18.0..=(header.right() - 18.0).max(header.left() + 18.0),
        header.top() - SECTION_GAP / 2.0,
        egui::Stroke::new(1.0, t.divider),
    );
    ui.painter().text(pos2(header.left() + 18.0, header.center().y), Align2::LEFT_CENTER, "Immich", t.semibold(12.0), t.text_dim);
    let status = if app.immich.refreshing() { "Loading…" } else { "↻" };
    ui.painter().text(pos2(header.right() - 18.0, header.center().y), Align2::RIGHT_CENTER, status, t.font(11.0), t.text_dim);
    register(ui, "button:immichRefresh", header);
    if resp.on_hover_text("List the Immich albums again").clicked() && !app.immich.refreshing() {
        crate::immich_load::refresh_albums(app);
    }
    let albums = match &app.immich.albums {
        Some(Ok(a)) => a.clone(),
        Some(Err(e)) => {
            let e = format!("Immich unreachable: {e}");
            ui.add(egui::Label::new(egui::RichText::new(e).size(11.0).color(t.text_dim)).wrap());
            return;
        }
        None => return,
    };
    let current = app.session.immich_view.as_ref().filter(|_| app.session.source == LibrarySource::Immich).map(|v| v.id.clone());
    for a in &albums {
        let count = usize::try_from(a.count).ok();
        let resp = Row::new(&format!("immich:{}", a.id), Icon::Album, &a.name)
            .count(count)
            .selected(current.as_deref() == Some(a.id.as_str()))
            .show(app, ui);
        if resp.clicked()
            && let Err(e) = crate::immich_load::start(app, &a.id, &a.name)
        {
            app.toast_error(ui.ctx(), e);
        }
    }
}
