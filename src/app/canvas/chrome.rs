//! Per-pane chrome: the header row (Transformations, auto-reload, reload,
//! hide, close), the footer readout (size / format / cursor value), the
//! centred error text, and the shared-cursor dot.

use crate::app::*;

impl CimApp {
    pub(super) fn draw_header(&mut self, ui: &mut egui::Ui, idx: usize, cell: Rect) {
        let header = Rect::from_min_size(cell.min, Vec2::new(cell.width(), header_h_for(cell.width())));
        let hp = ui.painter_at(header);
        let focused = idx == self.current;
        hp.rect_filled(
            header,
            0.0,
            if focused {
                Color32::from_rgb(40, 70, 110)
            } else {
                Color32::from_gray(34)
            },
        );

        // "Transformations" button on the LEFT of row 1 (away from the close ×
        // so it's hard to mis-click), toggling this pane's options popup.
        let modify = Rect::from_min_size(header.min, Vec2::new(MODIFY_W, HEADER_H));
        let mod_resp = ui
            .interact(modify, Id::new(("modify", idx)), Sense::click())
            .on_hover_text(self.hover_for(Action::ToggleVis, ""));
        let open = self.panes[idx].show_opts;
        hp.rect_filled(
            modify,
            0.0,
            if open {
                Color32::from_rgb(70, 110, 160)
            } else if mod_resp.hovered() {
                Color32::from_gray(70)
            } else {
                Color32::from_gray(52)
            },
        );
        hp.text(
            modify.center(),
            Align2::CENTER_CENTER,
            "Transformations",
            FontId::proportional(12.0),
            Color32::from_gray(225),
        );
        if mod_resp.clicked() {
            self.panes[idx].show_opts = !open;
        }

        // "Reload", "Hide" and "Close" buttons at the top-right (matching styles).
        // Reload re-reads from disk; Hide sets visible = false (keeps the pane);
        // Close removes it.
        let close_w = 44.0;
        let hide_w = 34.0;
        // "Reload" as a labelled button; size it to its text so it never clips.
        let reload_w = ui.fonts(|f| {
            f.layout_no_wrap("Reload".to_owned(), FontId::proportional(12.0), Color32::WHITE)
                .rect
                .width()
        }) + 14.0;
        // The Auto-reload (watch) toggle sits left of Reload, but only for panes
        // backed by a file — a Compute pane has its own Auto-refresh instead. It's
        // a labelled "Auto-reload" button; size it to its text so it never clips.
        let watchable = !matches!(self.panes[idx].source, Source::Computed);
        let watch_w = if watchable {
            let w = ui.fonts(|f| {
                f.layout_no_wrap("Auto-reload".to_owned(), FontId::proportional(12.0), Color32::WHITE)
                    .rect
                    .width()
            });
            w + 14.0
        } else {
            0.0
        };

        let count = self.panes[idx].media.frame_count();
        let name = self.panes[idx].media.name();
        // The index number is the one part that must always show; the filename is
        // dropped below if the cell is too narrow for the full title.
        let idx_str = format!("{}", idx + 1);
        let (title_full, title_short) = if count > 1 {
            let resident = self.panes[idx].media.resident_count();
            let sync = match (
                self.panes[idx].sync_spatial,
                self.panes[idx].sync_temporal,
            ) {
                (true, true) => "",
                (false, true) => "  ⊘pos",
                (true, false) => "  ⊘time",
                (false, false) => "  ⊘pos ⊘time",
            };
            // Until the real end is found, show the known count with a "+" so
            // it's clear more frames may still be discovered.
            let count_str = if self.panes[idx].media.at_end() {
                format!("{count}")
            } else {
                format!("{count}+")
            };
            let tail = format!(
                "   {}/{}  ({} in mem){}",
                self.frame_disp(idx) + 1,
                count_str,
                resident,
                sync
            );
            (format!("{idx_str}  {name}{tail}"), format!("{idx_str}{tail}"))
        } else {
            (format!("{idx_str}  {name}"), idx_str.clone())
        };

        // Title after the Transformations button, up to the Hide button. When the
        // full title (with the filename) doesn't fit that span, fall back to the
        // name-less form so the index/frame info stays readable in small cells.
        let title_x = header.min.x + MODIFY_W + 8.0;
        let title_right = header.max.x - close_w - hide_w - reload_w - watch_w - 6.0;
        let font = FontId::proportional(13.0);
        let fits = |ui: &egui::Ui, s: &str| {
            let w = ui.fonts(|f| f.layout_no_wrap(s.to_owned(), font.clone(), Color32::WHITE).rect.width());
            w <= (title_right - title_x)
        };
        let title = if fits(ui, &title_full) {
            title_full
        } else {
            title_short
        };
        hp.text(
            Pos2::new(title_x, header.min.y + HEADER_H / 2.0),
            Align2::LEFT_CENTER,
            title,
            font,
            Color32::from_gray(220),
        );

        // For a multi-file sequence (a numbered run or folder concatenated into
        // one timeline), a hover tooltip on the title reports which underlying
        // file the current frame comes from and its index within that file. The
        // label is selectable so the path can be copied out of the tooltip.
        if let Some((file, local)) = self
            .panes[idx]
            .media
            .local_file(self.frame_disp(idx))
            .map(|(p, i)| (p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(), i))
        {
            let title_rect = Rect::from_min_max(
                Pos2::new(title_x, header.min.y),
                Pos2::new(title_right, header.max.y),
            );
            ui.interact(title_rect, Id::new(("title", idx)), Sense::hover())
                .on_hover_ui(|ui| {
                    ui.add(
                        egui::Label::new(format!("Local file: {file} {local}")).selectable(true),
                    );
                });
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
            .on_hover_text(self.hover_for(Action::ReloadMedia, "Reload this media from disk"));
        if reload_resp.hovered() {
            hp.rect_filled(reload, 0.0, Color32::from_gray(70));
        }
        hp.text(
            reload.center(),
            Align2::CENTER_CENTER,
            "Reload",
            FontId::proportional(12.0),
            if reload_resp.hovered() {
                Color32::from_gray(235)
            } else {
                Color32::from_gray(170)
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
                    "Auto-reload on: reloads when the file changes on disk. Click to stop."
                } else {
                    "Auto-reload: watch the file and reload it when it changes on disk."
                });
            if watching {
                hp.rect_filled(watch, 0.0, Color32::from_rgb(70, 110, 160));
            } else if watch_resp.hovered() {
                hp.rect_filled(watch, 0.0, Color32::from_gray(70));
            }
            hp.text(
                watch.center(),
                Align2::CENTER_CENTER,
                "Auto-reload",
                FontId::proportional(12.0),
                if watching {
                    Color32::from_gray(230)
                } else if watch_resp.hovered() {
                    Color32::from_gray(235)
                } else {
                    Color32::from_gray(170)
                },
            );
            if watch_resp.clicked() {
                let on = !watching;
                self.panes[idx].watch.on = on;
                self.panes[idx].watch.seen = None;
                // Baseline to the current on-disk state when enabling, so turning
                // the watch on never triggers an immediate reload.
                self.panes[idx].watch.loaded = if on {
                    Self::source_file_sig(&self.panes[idx].source)
                } else {
                    None
                };
            }
        }

        let hide_resp = ui
            .interact(hide, Id::new(("hide", idx)), Sense::click())
            .on_hover_text(self.hover_for(Action::HideMedia, ""));
        if hide_resp.hovered() {
            hp.rect_filled(hide, 0.0, Color32::from_gray(70));
        }
        hp.text(
            hide.center(),
            Align2::CENTER_CENTER,
            "Hide",
            FontId::proportional(12.0),
            if hide_resp.hovered() {
                Color32::from_gray(235)
            } else {
                Color32::from_gray(170)
            },
        );
        if hide_resp.clicked() {
            self.panes[idx].visible = false;
            self.reselect_if_hidden();
        }

        let close_resp = ui.interact(close, Id::new(("close", idx)), Sense::click());
        if close_resp.hovered() {
            hp.rect_filled(close, 0.0, Color32::from_gray(70));
        }
        hp.text(
            close.center(),
            Align2::CENTER_CENTER,
            "Close",
            FontId::proportional(12.0),
            // Red-tinted on hover to flag that Close removes the pane.
            if close_resp.hovered() {
                Color32::from_rgb(230, 120, 120)
            } else {
                Color32::from_gray(170)
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

    /// Bottom status strip: resolution (h×w), the shared cursor pixel, and this
    /// pane's native value there.
    pub(super) fn draw_footer(&self, ui: &egui::Ui, idx: usize, footer: Rect) {
        let fp = ui.painter_at(footer);
        fp.rect_filled(footer, 0.0, Color32::from_gray(28));
        // Top border: the footer floats over the image, so a hairline separates
        // it from the image above (matching the header's bottom border).
        fp.hline(
            footer.x_range(),
            footer.min.y + 0.5,
            Stroke::new(1.0_f32, CHROME_BORDER),
        );

        let [w, h] = self.disp_size(idx);
        // Native sample format (uint8 / uint16 / float32), when the frame is
        // resident. Kept next to the resolution so the readout reads "H×W type".
        let kind = self.panes[idx]
            .media
            .resident(self.frame_disp(idx))
            .map(|fr| fr.kind_label());
        let dims = match kind {
            Some(k) => format!("{h}×{w}  {k}"),
            None => format!("{h}×{w}"),
        };
        let mut text = dims.clone();
        if let Some(ci) = self.cursor_img {
            let (x, y) = (ci.x.floor() as i64, ci.y.floor() as i64);
            if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                text = format!("{dims}    x {x}  y {y}    {}", self.value_string(idx, ci));
            }
        }

        fp.text(
            footer.left_center() + Vec2::new(8.0, 0.0),
            Align2::LEFT_CENTER,
            text,
            FontId::monospace(12.0),
            Color32::from_gray(200),
        );
    }

    /// If this sequence failed to decode, paint its message centred over `rect`.
    pub(super) fn draw_pane_error(&self, ui: &egui::Ui, idx: usize, rect: Rect) {
        let Some(msg) = self.panes[idx].error.as_deref() else {
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
    pub(super) fn draw_cursor_dot(&self, painter: &egui::Painter, idx: usize, coord_area: Rect, clip: Rect) {
        if !self.config.cursor_dot || self.cursor_pane == Some(idx) {
            return;
        }
        let Some(ci) = self.cursor_img else { return };
        let sp = self.rot_img_to_screen(idx, ci, coord_area);
        if !clip.contains(sp) {
            return;
        }
        painter.circle_filled(sp, 3.5, Color32::from_rgb(235, 40, 40));
        painter.circle_stroke(sp, 3.5, Stroke::new(1.0_f32, Color32::from_black_alpha(160)));
    }
}
