//! Per-pane chrome: the header row (title, auto-reload, reload, hide, close),
//! the footer readout (size / format / cursor value), the centred error text,
//! and the shared-cursor dot.

use crate::app::*;

impl CimApp {
    pub(super) fn draw_header(&mut self, ui: &mut egui::Ui, idx: usize, header: Rect) {
        let hp = ui.painter_at(header);
        let focused = idx == self.current;
        hp.rect_filled(header, 0.0, if focused { ACCENT } else { BAR_FILL });
        hp.vline(
            header.max.x - 0.5,
            header.y_range(),
            Stroke::new(1.0_f32, CHROME_BORDER),
        );
        // (The Transformations controls now live in the single global panel on the
        // toolbar, so the header no longer carries a per-pane button.)

        // "Reload", "Hide" and "Close" buttons at the top-right (matching styles).
        // Reload re-reads from disk; Hide sets visible = false (keeps the pane);
        // Close removes it.
        let text_w = |ui: &egui::Ui, s: &str| {
            ui.fonts(|f| {
                f.layout_no_wrap(s.to_owned(), FontId::proportional(12.0), Color32::WHITE)
                    .rect
                    .width()
            }) + 14.0
        };
        // "Reload" as a labelled button; size it to its text so it never clips.
        let reload_label = t!("pane.reload").into_owned();
        let watch_label = t!("pane.auto_reload").into_owned();
        let hide_label = t!("pane.hide").into_owned();
        let close_label = t!("pane.close").into_owned();
        let reload_w = ui.fonts(|f| {
            f.layout_no_wrap(
                reload_label.clone(),
                FontId::proportional(12.0),
                Color32::WHITE,
            )
            .rect
            .width()
        }) + 14.0;
        let close_w = text_w(ui, &close_label).max(44.0);
        let hide_w = text_w(ui, &hide_label).max(34.0);
        // The Auto-reload (watch) toggle sits left of Reload, but only for panes
        // backed by a file — a Compute pane has its own Auto-refresh instead. It's
        // a labelled "Auto-reload" button; size it to its text so it never clips.
        let watchable = !matches!(self.panes[idx].source, Source::Computed);
        let watch_w = if watchable {
            let w = ui.fonts(|f| {
                f.layout_no_wrap(
                    watch_label.clone(),
                    FontId::proportional(12.0),
                    Color32::WHITE,
                )
                .rect
                .width()
            });
            w + 14.0
        } else {
            0.0
        };

        let count = self.panes[idx].media.frame_count();
        let name = self.pane_name(idx);
        // The index number is the one part that must always show; the filename is
        // dropped below if the cell is too narrow for the full title.
        let idx_str = format!("{}", idx + 1);
        let (title_full, title_short) = if count > 1 {
            let resident = self.panes[idx].media.resident_count();
            let sync = match (self.panes[idx].sync_spatial, self.panes[idx].sync_temporal) {
                (true, true) => String::new(),
                (false, true) => format!("  ⊘{}", t!("pane.sync_pos")),
                (true, false) => format!("  ⊘{}", t!("pane.sync_time")),
                (false, false) => {
                    format!("  ⊘{} ⊘{}", t!("pane.sync_pos"), t!("pane.sync_time"))
                }
            };
            // Until the real end is found, show the known count with a "+" so
            // it's clear more frames may still be discovered.
            let count_str = if self.panes[idx].media.at_end() {
                format!("{count}")
            } else {
                format!("{count}+")
            };
            let tail = format!(
                "   {}/{}  ({}){}",
                self.frame_disp(idx) + 1,
                count_str,
                t!("pane.in_memory", n = resident),
                sync
            );
            (
                format!("{idx_str}  {name}{tail}"),
                format!("{idx_str}{tail}"),
            )
        } else {
            (format!("{idx_str}  {name}"), idx_str.clone())
        };

        // Title from the left edge, up to the Hide button. When the full title
        // (with the filename) doesn't fit that span, fall back to the name-less
        // form so the index/frame info stays readable in small cells.
        let title_x = header.min.x + 8.0;
        let title_right = header.max.x - close_w - hide_w - reload_w - watch_w - 6.0;
        let font = FontId::proportional(13.0);
        let fits = |ui: &egui::Ui, s: &str| {
            let w = ui.fonts(|f| {
                f.layout_no_wrap(s.to_owned(), font.clone(), Color32::WHITE)
                    .rect
                    .width()
            });
            w <= (title_right - title_x)
        };
        let title = if fits(ui, &title_full) {
            title_full
        } else {
            title_short
        };
        let title_rect = Rect::from_min_max(
            Pos2::new(title_x, header.min.y),
            Pos2::new(title_right, header.max.y),
        );
        let pane_id = self.panes[idx].id;
        if self.renaming.as_ref().is_some_and(|(id, _)| *id == pane_id) {
            // Being renamed: the index stays, the name becomes a text field
            // whose text sits exactly where the title's name is drawn — same
            // font, same left edge (the width of the "N  " before the name),
            // same vertically centred row.
            let (name_x, row_h) = ui.fonts(|f| {
                let w = |s: String| {
                    f.layout_no_wrap(s, font.clone(), Color32::WHITE)
                        .rect
                        .width()
                };
                (
                    w(format!("{idx_str}  x")) - w("x".into()),
                    f.row_height(&font),
                )
            });
            hp.text(
                Pos2::new(title_x, header.min.y + HEADER_H / 2.0),
                Align2::LEFT_CENTER,
                idx_str,
                font.clone(),
                TEXT_DEFAULT,
            );
            let mid = header.min.y + HEADER_H / 2.0;
            let text_rect = Rect::from_min_max(
                Pos2::new(title_x + name_x, mid - row_h / 2.0),
                Pos2::new(title_right.max(title_x + name_x + 60.0), mid + row_h / 2.0),
            );
            self.draw_rename_field(ui, idx, text_rect, header, font);
        } else {
            hp.text(
                Pos2::new(title_x, header.min.y + HEADER_H / 2.0),
                Align2::LEFT_CENTER,
                title,
                font,
                TEXT_DEFAULT,
            );
        }

        // A hover tooltip on the title reports the absolute path of the file the
        // currently shown frame comes from (works for any media type). For a
        // multi-file sequence (a numbered run or folder concatenated into one
        // timeline) it adds the page index within that underlying file. The path
        // label is selectable so it can be copied out of the tooltip.
        let cur_path = self.current_file_path(idx);
        let local_page = self.panes[idx]
            .media
            .local_file(self.frame_disp(idx))
            .map(|(_, i)| i);
        // A double-click on the title renames the pane in place.
        let renaming = self.renaming.as_ref().is_some_and(|(id, _)| *id == pane_id);
        if !renaming {
            let title_resp = ui
                .interact(title_rect, Id::new(("title", idx)), Sense::click())
                .on_hover_ui(|ui| {
                    if let Some(path) = &cur_path {
                        ui.add(egui::Label::new(path.display().to_string()).selectable(true));
                    }
                    if let Some(page) = local_page {
                        ui.label(t!("pane.page_in_file", page = page));
                    }
                    ui.label(egui::RichText::new(t!("pane.rename_hover")).weak());
                });
            if title_resp.double_clicked() {
                self.start_rename(ui.ctx(), idx);
            }
        }

        let close = Rect::from_min_size(
            Pos2::new(header.max.x - close_w, header.min.y),
            Vec2::new(close_w, HEADER_H),
        );
        let hide = Rect::from_min_size(
            Pos2::new(close.min.x - hide_w, header.min.y),
            Vec2::new(hide_w, HEADER_H),
        );
        let reload = Rect::from_min_size(
            Pos2::new(hide.min.x - reload_w, header.min.y),
            Vec2::new(reload_w, HEADER_H),
        );
        let reload_resp = ui
            .interact(reload, Id::new(("reload", idx)), Sense::click())
            .on_hover_text(self.hover_for(Action::ReloadMedia, &t!("pane.reload_hover")));
        if reload_resp.hovered() {
            hp.rect_filled(reload, 0.0, BUTTON_HOVER_FILL);
        }
        hp.text(
            reload.center(),
            Align2::CENTER_CENTER,
            reload_label,
            FontId::proportional(12.0),
            if reload_resp.hovered() {
                TEXT_BUTTON_HOVER
            } else {
                TEXT_BUTTON
            },
        );
        if reload_resp.clicked() {
            self.deferred.push(Deferred::Reload(idx));
        }

        // Auto-reload (watch) toggle, left of Reload. A labelled button that fills
        // blue while watching (matching the Transformations toggle), a plain hover
        // fill otherwise; only shown for file-backed panes.
        if watchable {
            let watch = Rect::from_min_size(
                Pos2::new(reload.min.x - watch_w, header.min.y),
                Vec2::new(watch_w, HEADER_H),
            );
            let watching = self.panes[idx].watch.on;
            let watch_resp = ui
                .interact(watch, Id::new(("watch", idx)), Sense::click())
                .on_hover_text(if watching {
                    t!("pane.auto_reload_on_hover")
                } else {
                    t!("pane.auto_reload_off_hover")
                });
            if watching {
                if focused {
                    hp.rect_filled(watch, 0.0, BUTTON_HOVER_FILL);
                } else {
                    hp.rect_filled(watch, 0.0, ACCENT);
                }
            } else if watch_resp.hovered() {
                hp.rect_filled(watch, 0.0, BUTTON_HOVER_FILL);
            }
            hp.text(
                watch.center(),
                Align2::CENTER_CENTER,
                watch_label,
                FontId::proportional(12.0),
                if watching {
                    TEXT_BUTTON_ACTIVE
                } else if watch_resp.hovered() {
                    TEXT_BUTTON_HOVER
                } else {
                    TEXT_BUTTON
                },
            );
            if watch_resp.clicked() {
                let on = !watching;
                // Baseline to the current on-disk state when enabling, so turning
                // the watch on never triggers an immediate reload. The baseline is
                // the first signature the watcher thread reports back.
                self.rebaseline_watch(idx);
                self.panes[idx].watch.on = on;
            }
        }

        let hide_resp = ui
            .interact(hide, Id::new(("hide", idx)), Sense::click())
            .on_hover_text(self.hover_for(Action::HideMedia, ""));
        if hide_resp.hovered() {
            hp.rect_filled(hide, 0.0, BUTTON_HOVER_FILL);
        }
        hp.text(
            hide.center(),
            Align2::CENTER_CENTER,
            hide_label,
            FontId::proportional(12.0),
            if hide_resp.hovered() {
                TEXT_BUTTON_HOVER
            } else {
                TEXT_BUTTON
            },
        );
        if hide_resp.clicked() {
            self.panes[idx].visible = false;
            self.reselect_if_hidden();
        }

        let close_resp = ui.interact(close, Id::new(("close", idx)), Sense::click());
        if close_resp.hovered() {
            hp.rect_filled(close, 0.0, BUTTON_HOVER_FILL);
        }
        hp.text(
            close.center(),
            Align2::CENTER_CENTER,
            close_label,
            FontId::proportional(12.0),
            // Red-tinted on hover to flag that Close removes the pane.
            if close_resp.hovered() {
                Color32::from_rgb(230, 120, 120)
            } else {
                TEXT_BUTTON
            },
        );
        if close_resp.clicked() {
            self.deferred.push(Deferred::Remove(idx));
        }

        // Bottom border: the header floats over the image (and, in the top row,
        // under the global toolbar), so a hairline separates it from what's below.
        hp.hline(
            header.x_range(),
            header.max.y - 0.5,
            Stroke::new(1.0_f32, CHROME_BORDER),
        );
    }

    /// The pane's name wherever the UI names it — the header, the A/B tags and
    /// pickers, the frame bar, the media manager, the Compute and overlay source
    /// pickers, the line profile and the export labels. The user's name for it
    /// (`custom_name`) exactly as typed — it replaces the folder prefix too — or
    /// else the media's own, prefixed by the last
    /// `config.header_parents` folders of the file (or, for a numbered sequence,
    /// the first frame file) it was opened from. A Compute pane has no file, so
    /// it keeps its bare name — which, until renamed, is built live from its
    /// sources' names, so renaming a source renames the result too.
    pub(in crate::app) fn pane_name(&self, idx: usize) -> String {
        self.pane_name_at(idx, 0)
    }

    /// [`Self::pane_name`], `depth` panes down a Compute chain (bounded by the
    /// pane count, so even a cycle wired in from a stale view command can't
    /// recurse forever).
    fn pane_name_at(&self, idx: usize, depth: usize) -> String {
        let pane = &self.panes[idx];
        match &pane.custom_name {
            // Shown exactly as typed: the folder prefix is part of what the user
            // edited (it's in the field), so deleting it removes it.
            Some(name) => name.clone(),
            None => self.with_header_parents(idx, &self.own_name_at(idx, depth)),
        }
    }

    /// What pane `idx` is called when it isn't renamed, without the folder
    /// prefix: the media's name, or a Compute result's built from its sources.
    fn own_name(&self, idx: usize) -> String {
        self.own_name_at(idx, 0)
    }

    fn own_name_at(&self, idx: usize, depth: usize) -> String {
        let pane = &self.panes[idx];
        let src = |id: Option<u64>| {
            id.and_then(|id| self.panes.iter().position(|p| p.id == id))
                .filter(|_| depth < self.panes.len())
                .map(|i| self.pane_name_at(i, depth + 1))
        };
        match &pane.compute {
            Some(c) if c.computed && c.kind.is_binary() => {
                if let (Some(a), Some(b)) = (src(c.source_id), src(c.source_b)) {
                    return format!("{} · {} {} {}", c.kind.label(), a, c.kind.sign(), b);
                }
            }
            Some(c) if c.computed => {
                if let Some(a) = src(c.source_id) {
                    return format!("{} · {}", c.kind.label(), a);
                }
            }
            _ => {}
        }
        pane.media.name().to_owned()
    }

    /// `name` prefixed by the last `config.header_parents` folders of pane
    /// `idx`'s file; unchanged for a Compute pane or with the setting at 0.
    fn with_header_parents(&self, idx: usize, name: &str) -> String {
        let n = self.config.header_parents;
        let file = match &self.panes[idx].source {
            Source::File(p) => Some(p),
            Source::Sequence { files, .. } => files.first(),
            Source::Computed => None,
        };
        match file {
            Some(p) if n > 0 => with_parent_dirs(&absolute_path(p), name, n),
            _ => name.to_owned(),
        }
    }

    /// Open pane `idx`'s title for renaming, seeded with the name as shown —
    /// folder prefix included — and all of it selected, so typing replaces it.
    fn start_rename(&mut self, ctx: &egui::Context, idx: usize) {
        let text = self.pane_name(idx);
        let id = Id::new(("rename", self.panes[idx].id));
        let mut state = egui::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
        state
            .cursor
            .set_char_range(Some(egui::text_selection::CCursorRange::two(
                egui::text::CCursor::new(0),
                egui::text::CCursor::new(text.chars().count()),
            )));
        state.store(ctx, id);
        self.renaming = Some((self.panes[idx].id, text));
        // Focus is taken by the field when it is first drawn, next frame.
        ctx.memory_mut(|m| m.request_focus(id));
        ctx.request_repaint();
    }

    /// The in-place rename field over pane `idx`'s title: black on a white
    /// box inside the `header`, with no frame, its text in `text_rect` (where
    /// the title's name is drawn). Enter or clicking anywhere else commits (the
    /// field loses focus either way); Escape drops the edit. A blank name — or
    /// the one the pane shows un-renamed — clears the custom name, so the pane
    /// goes back to following its media (and, for a Compute pane, its sources'
    /// names) and the folder prefix setting.
    fn draw_rename_field(
        &mut self,
        ui: &mut egui::Ui,
        idx: usize,
        text_rect: Rect,
        header: Rect,
        font: FontId,
    ) {
        let Some((_, text)) = self.renaming.as_mut() else {
            return;
        };
        let edit_id = Id::new(("rename", self.panes[idx].id));
        let bg = Rect::from_min_max(
            Pos2::new(text_rect.min.x - 4.0, header.min.y + 3.0),
            Pos2::new(text_rect.max.x, header.max.y - 3.0),
        );
        ui.painter().rect_filled(bg, 2.0, Color32::WHITE);
        let resp = ui
            .scope(|ui| {
                // The caret is drawn in the theme's (light) cursor colour, which
                // would vanish on white.
                ui.visuals_mut().text_cursor.stroke.color = Color32::BLACK;
                ui.put(
                    text_rect,
                    egui::TextEdit::singleline(text)
                        .id(edit_id)
                        .font(font)
                        .text_color(Color32::BLACK)
                        .frame(false)
                        .margin(Vec2::ZERO),
                )
            })
            .inner;
        // `start_rename` asked for focus; anything else ends the edit (focus
        // lost to a click elsewhere, Enter, Escape, or never taken at all).
        if resp.has_focus() || ui.memory(|m| m.has_focus(edit_id)) {
            return;
        }
        let Some((_, text)) = self.renaming.take() else {
            return;
        };
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            return;
        }
        let name = text.trim();
        let own = self.with_header_parents(idx, &self.own_name(idx));
        self.panes[idx].custom_name = (!name.is_empty() && name != own).then(|| name.to_owned());
    }

    /// Absolute path of the file backing the currently shown frame, for the
    /// filename hover. A multi-file sequence resolves to the specific file its
    /// current global frame maps to (`local_file`); any other file-backed media
    /// (a still or one multi-page TIFF) resolves to its own source path. `None`
    /// for a computed pane, or a sequence frame not yet mapped to a file.
    pub(super) fn current_file_path(&self, idx: usize) -> Option<PathBuf> {
        let pane = &self.panes[idx];
        if let Some((p, _)) = pane.media.local_file(self.frame_disp(idx)) {
            return Some(absolute_path(p));
        }
        match &pane.source {
            Source::File(p) => Some(absolute_path(p)),
            _ => None,
        }
    }

    /// Bottom status strip: resolution (h×w), the shared cursor pixel, and this
    /// pane's native value there.
    pub(super) fn draw_footer(&self, ui: &egui::Ui, idx: usize, footer: Rect) {
        let fp = ui.painter_at(footer);
        fp.rect_filled(footer, 0.0, BAR_FILL);
        // Top border: the footer floats over the image, so a hairline separates
        // it from the image above (matching the header's bottom border).
        fp.hline(
            footer.x_range(),
            footer.min.y + 0.5,
            Stroke::new(1.0_f32, CHROME_BORDER),
        );
        fp.vline(
            footer.max.x - 0.5,
            footer.y_range(),
            Stroke::new(1.0_f32, CHROME_BORDER),
        );

        let [w, h] = self.disp_size(idx);
        // Native sample format kept next to the resolution: "H×W type".
        let dims = match self.kind_label(idx) {
            Some(k) => format!("{h}×{w}  {k}"),
            None => format!("{h}×{w}"),
        };
        let mut text = dims.clone();
        if let Some(ci) = self.cursor_img {
            let (x, y) = (ci.x.floor() as i64, ci.y.floor() as i64);
            if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                text = format!(
                    "{dims}    {}={y}  {}={x}    {}",
                    t!("pane.row"),
                    t!("pane.col"),
                    self.value_string(idx, ci)
                );
            }
        }

        fp.text(
            footer.left_center() + Vec2::new(8.0, 0.0),
            Align2::LEFT_CENTER,
            text,
            FontId::proportional(12.0),
            TEXT_DEFAULT,
        );

        fp.hline(
            footer.x_range(),
            footer.max.y - 0.5,
            Stroke::new(1.0_f32, CHROME_BORDER),
        );
    }

    /// If this sequence failed to decode, paint its message centred over `rect`.
    pub(super) fn draw_pane_error(&self, ui: &egui::Ui, idx: usize, rect: Rect) {
        let pane = &self.panes[idx];
        let Some(msg) = pane.error.as_deref().or(pane.tex_error.as_deref()) else {
            return;
        };
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_black_alpha(150));
        let col = Color32::from_rgb(240, 130, 130);
        let galley = painter.layout(
            format!("⚠  {msg}"),
            FontId::proportional(15.0),
            col,
            (rect.width() - 32.0).max(48.0),
        );
        let pos = rect.center() - galley.size() / 2.0;
        painter.galley(pos, galley, col);
    }

    /// The native pixel value at the shared image cursor for pane `idx`: the
    /// value string when on a resident pixel, `…` when the frame isn't loaded,
    /// or `—` when the cursor falls outside this pane's image.
    /// Pane `idx`'s native sample format (`uint8` / `uint16` / `float32`), or
    /// `None` while its shown frame isn't resident. Shared by the per-pane footer
    /// and the A/B footer so both report the depth the same way.
    pub(super) fn kind_label(&self, idx: usize) -> Option<&'static str> {
        self.panes[idx]
            .media
            .resident(self.frame_disp(idx))
            .map(|fr| fr.kind_label())
    }

    pub(super) fn value_string(&self, idx: usize, cursor: Vec2) -> String {
        let [w, h] = self.disp_size(idx);
        let (x, y) = (cursor.x.floor() as i64, cursor.y.floor() as i64);
        if x < 0 || y < 0 || x as usize >= w || y as usize >= h {
            return "—".into();
        }
        let f = self.frame_disp(idx);
        match self.panes[idx].media.resident(f) {
            Some(frame) => frame.pixel_string(x as usize, y as usize),
            None => "…".into(),
        }
    }

    /// Paint the shared cursor as a red dot at its image position on pane `idx`.
    /// `coord_area` maps image→screen; `clip` hides it when it maps off the pane.
    /// Skipped when disabled in Settings, and never drawn on the pane the cursor
    /// is actually over (its own OS cursor already marks the spot).
    pub(super) fn draw_cursor_dot(
        &self,
        painter: &egui::Painter,
        idx: usize,
        coord_area: Rect,
        clip: Rect,
    ) {
        if !self.config.cursor_dot || self.cursor_pane == Some(idx) {
            return;
        }
        let Some(ci) = self.cursor_img else { return };
        let sp = self.rot_img_to_screen(idx, ci, coord_area);
        if !clip.contains(sp) {
            return;
        }
        painter.circle_filled(sp, 3.5, Color32::from_rgb(235, 40, 40));
        painter.circle_stroke(
            sp,
            3.5,
            Stroke::new(1.0_f32, Color32::from_black_alpha(160)),
        );
    }
}
