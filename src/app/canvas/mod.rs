//! The central image area: grid / single / A-B wipe drawing, pan & zoom,
//! ctrl-drag reorder, per-pane header/footer, and the export-region overlay.

mod line_profile;
mod options_popup;
mod region_stats;

use super::*;

impl CimApp {
    pub(super) fn draw_central(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let area = ui.available_rect_before_wrap();
        self.last_area = area;
        // Recomputed below from whichever pane the cursor is over; the panes then
        // replicate it (red dot + per-pane pixel value).
        self.cursor_img = None;
        self.cursor_pane = None;

        if self.panes.is_empty() {
            ui.painter().text(
                area.center(),
                Align2::CENTER_CENTER,
                "Open images or drop files here",
                FontId::proportional(18.0),
                Color32::from_gray(140),
            );
            return;
        }

        let hover = ctx.input(|i| i.pointer.hover_pos());
        match self.mode {
            Mode::Single => {
                let idx = self.current.min(self.panes.len() - 1);
                let ia = image_area(area);
                self.cursor_img = hover.and_then(|p| self.hover_img_pos(idx, ia, ia, p));
                self.cursor_pane = self.cursor_img.map(|_| idx);
                self.draw_pane(ui, ctx, idx, area);
            }
            Mode::Grid => {
                let vis = self.visible_indices();
                if vis.is_empty() {
                    ui.painter().text(
                        area.center(),
                        Align2::CENTER_CENTER,
                        "All media hidden — enable some in ☰ Media",
                        FontId::proportional(16.0),
                        Color32::from_gray(140),
                    );
                    return;
                }
                let cells = self.grid_cells(&vis, area);
                // The cursor's image position comes from whichever cell it's over.
                if let Some(p) = hover {
                    for &(idx, cell) in &cells {
                        let ia = image_area(cell);
                        if let Some(ci) = self.hover_img_pos(idx, ia, ia, p) {
                            self.cursor_img = Some(ci);
                            self.cursor_pane = Some(idx);
                            break;
                        }
                    }
                }
                for &(idx, cell) in &cells {
                    self.draw_pane(ui, ctx, idx, cell);
                }
                self.draw_reorder_borders(ui, ctx, &cells);
                self.finish_reorder(ctx, &cells);
            }
            Mode::Ab => self.draw_ab(ui, ctx, area),
        }

        self.region_overlay(ui, ctx, area);
    }

    /// Draw / edit the export region rectangle over the view.
    ///
    /// Selection is done in Single view (forced while `selecting_region`): the
    /// screen drag is converted to IMAGE space on release, so the same crop
    /// applies to every pane of the comparison afterwards.
    pub(super) fn region_overlay(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, area: Rect) {
        if !self.show_export || self.panes.is_empty() {
            return;
        }

        if self.selecting_region {
            // The crop is dragged with the **right** button, so the left button
            // stays free to pan/zoom around the image while choosing it (the pane
            // interaction handles that). Edge-detect the secondary button like the
            // stats region rather than a competing full-area drag interact.
            let down = ctx.input(|i| i.pointer.secondary_down());
            let pos = ctx.input(|i| i.pointer.interact_pos());
            if down {
                if self.sel_start.is_none() {
                    // Begin only if the press starts inside the view.
                    self.sel_start = pos.filter(|p| area.contains(*p));
                }
                if let (Some(s), Some(c)) = (self.sel_start, pos) {
                    self.sel_rect = Some(Rect::from_two_pos(s, c).intersect(area));
                }
            } else if self.sel_start.is_some() {
                // Right button released: finalize (a near-zero drag clears it).
                self.selecting_region = false;
                self.sel_start = None;
                self.export_region = self
                    .sel_rect
                    .take()
                    .filter(|r| r.width() >= 4.0 && r.height() >= 4.0)
                    .and_then(|r| self.screen_rect_to_image(r, area));
                if let Some(m) = self.pre_select_mode.take() {
                    self.mode = m;
                }
            }
            if let Some(r) = self.sel_rect {
                dim_outside(&ui.painter_at(area), area, r);
            }
            return;
        }

        // Region chosen: show it on every pane it applies to.
        let Some(reg) = self.export_region else { return };
        let panes_areas: Vec<(usize, Rect)> = match self.mode {
            Mode::Single => {
                vec![(self.current.min(self.panes.len() - 1), image_area(area))]
            }
            Mode::Grid => self
                .grid_cells(&self.visible_indices(), area)
                .iter()
                .map(|&(idx, cell)| (idx, image_area(cell)))
                .collect(),
            // The wipe shares one image area; both sides are spatially the same
            // place, so pane A's view is representative.
            Mode::Ab => vec![(self.slot_a.min(self.panes.len() - 1), ab_image_rect(area))],
        };
        for (idx, img_area) in panes_areas {
            // The region is stored in the pane's *unrotated* view frame (it is
            // aligned to the viewer, not the image — see `select_region_bounds`),
            // so it maps back with the plain view: an axis-aligned screen rect,
            // exactly the rectangle the user dragged. The pane's rotation is
            // applied downstream by the export's `unrotate`, not here.
            let v = self.view_ref(idx);
            let r = Rect::from_two_pos(
                v.img_to_screen(reg.min.to_vec2(), img_area),
                v.img_to_screen(reg.max.to_vec2(), img_area),
            )
            .intersect(img_area);
            if r.is_positive() {
                dim_outside(&ui.painter_at(img_area), img_area, r);
            }
        }
    }

    /// Convert a screen-space rect (drawn in Single view over `area`) into the
    /// export crop, aligned to the viewer (see [`select_region_bounds`]).
    pub(super) fn screen_rect_to_image(&self, r: Rect, area: Rect) -> Option<Rect> {
        let idx = self.current.min(self.panes.len().checked_sub(1)?);
        self.select_region_bounds(idx, r, image_area(area))
    }

    /// Convert a screen-space selection rect into the image-space region it
    /// covers for pane `idx`, using the pane's view **without its rotation** —
    /// so the region is aligned to the viewer's axis (exactly the rectangle the
    /// user dragged), not the image's. The rotation is re-applied downstream: the
    /// export samples each pixel through `unrotate`, and the overlays draw the
    /// region back with the same plain view. Because the view is a pure
    /// similarity (no rotation), a screen-axis-aligned rect maps to an
    /// axis-aligned image rect, so two opposite corners suffice.
    ///
    /// Clamped to the image bounds only on an **unrotated** pane, so a rotated
    /// crop can include the background outside the image (the export renders it
    /// as transparent); an unrotated crop drops the background exactly as before.
    /// Shared by the export crop and the right-drag stats region so both convert
    /// a release identically.
    pub(super) fn select_region_bounds(&self, idx: usize, r: Rect, area: Rect) -> Option<Rect> {
        let v = self.view_ref(idx);
        let a = v.screen_to_img(r.min, area);
        let b = v.screen_to_img(r.max, area);
        let mut reg = Rect::from_two_pos(a.to_pos2(), b.to_pos2());
        if self.pane_theta(idx) == 0.0 {
            let [w, h] = self.disp_size(idx);
            reg = reg.intersect(Rect::from_min_max(Pos2::ZERO, Pos2::new(w as f32, h as f32)));
        }
        (reg.width() >= 1.0 && reg.height() >= 1.0).then_some(reg)
    }

    pub(super) fn grid_cells(&self, vis: &[usize], area: Rect) -> Vec<(usize, Rect)> {
        let n = vis.len();
        let cols = self.config.max_columns.max(1).min(n).max(1);
        let rows = n.div_ceil(cols);
        let cw = (area.width() - GAP * (cols as f32 - 1.0)) / cols as f32;
        let ch = (area.height() - GAP * (rows as f32 - 1.0)) / rows as f32;
        let mut cells = Vec::with_capacity(n);
        for (k, &idx) in vis.iter().enumerate() {
            let r = k / cols;
            let c = k % cols;
            let min = Pos2::new(
                area.min.x + c as f32 * (cw + GAP),
                area.min.y + r as f32 * (ch + GAP),
            );
            cells.push((idx, Rect::from_min_size(min, Vec2::new(cw, ch))));
        }
        cells
    }

    pub(super) fn finish_reorder(&mut self, ctx: &egui::Context, cells: &[(usize, Rect)]) {
        let Some(src) = self.drag_src else { return };
        if !ctx.input(|i| i.pointer.any_released()) {
            return;
        }
        if let Some(pos) = ctx.input(|i| i.pointer.interact_pos()) {
            if let Some(&(dst, _)) = cells.iter().find(|(_, r)| r.contains(pos)) {
                if dst != src {
                    self.panes.swap(src, dst);
                    remap(&mut self.current, src, dst);
                    remap(&mut self.control, src, dst);
                    remap(&mut self.slot_a, src, dst);
                    remap(&mut self.slot_b, src, dst);
                }
            }
        }
        self.drag_src = None;
    }

    pub(super) fn draw_pane(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, idx: usize, cell: Rect) {
        let img_area = image_area(cell);
        let size = self.panes[idx].media.size();

        // Fit this pane's effective view on first draw / after a reset.
        {
            let v = self.view_mut(idx);
            if v.needs_fit {
                v.fit(size, img_area);
            }
        }

        // Textures were staged/committed by `refresh_textures` before drawing; here
        // we just blit the committed one (falling back to a not-yet-committed frame
        // on the very first paint so a pane isn't blank while its siblings load).
        let tex = self.pane_texture(idx);
        let overlay = self.prepare_overlay(ctx, idx);
        let painter = ui.painter_at(img_area);
        painter.rect_filled(img_area, 0.0, Color32::from_gray(24));
        if let Some(id) = tex {
            let v = *self.view_ref(idx);
            let rect = v.image_rect(self.disp_size(idx), img_area);
            let theta = self.pane_theta(idx);
            paint_rotated(&painter, id, rect, theta);
            // The mask overlay shares the base image's rect (1:1 in image space).
            if let Some(ov) = overlay {
                paint_rotated(&painter, ov, rect, theta);
            }
        }
        // Replicate the shared cursor here (also on the hovered pane, marking the
        // exact pixel under the cursor).
        self.draw_cursor_dot(&painter, idx, img_area, img_area);
        self.draw_pane_error(ui, idx, img_area);

        // Interaction: left-drag pans and the wheel zooms — allowed even while
        // choosing an export crop, so the user can move around the image first
        // (the crop itself is a right-drag, handled in `region_overlay`).
        // Ctrl-drag reorder, click-to-focus and the right-drag stats region are
        // suppressed during crop selection (the right button drives the crop).
        let resp = ui.interact(img_area, Id::new(("pane", idx)), Sense::click_and_drag());
        let ctrl = ctx.input(|i| i.modifiers.ctrl);
        let alt = ctx.input(|i| i.modifiers.alt);
        if resp.hovered() {
            let scroll = wheel_delta(ctx);
            if scroll != 0.0 {
                if ctrl {
                    // Ctrl + wheel scrubs the sequence a frame at a time (up =
                    // next, down = previous) instead of zooming — same stepping as
                    // the next/prev-frame keys (advances the shared timeline).
                    let step = if scroll > 0.0 { Action::NextFrame } else { Action::PrevFrame };
                    self.apply_action(step, ctx);
                } else {
                    let anchor = ctx
                        .input(|i| i.pointer.hover_pos())
                        .unwrap_or(img_area.center());
                    let speed = zoom_speed(ctx);
                    self.view_mut(idx).zoom_at((scroll * speed).exp(), anchor, img_area);
                }
            }
        }
        // Alt + primary drag rotates the pane about its image centre (à la
        // Photoshop): the pane follows the cursor's angle around the pivot.
        if !self.selecting_region && resp.drag_started_by(PointerButton::Primary) {
            if alt {
                let rect = self.view_ref(idx).image_rect(self.disp_size(idx), img_area);
                let pivot = rect.center();
                if let Some(p) = ctx.input(|i| i.pointer.interact_pos()) {
                    let ang = (p - pivot).angle();
                    self.rotate_drag = Some((idx, pivot, ang, self.rotation_of(idx)));
                }
            } else if ctrl {
                self.drag_src = Some(idx);
            }
        }
        if let Some((ridx, pivot, start_ang, start_rot)) = self.rotate_drag {
            if ridx == idx {
                if resp.dragged_by(PointerButton::Primary) {
                    if let Some(p) = ctx.input(|i| i.pointer.interact_pos()) {
                        let delta = ((p - pivot).angle() - start_ang).to_degrees();
                        // Snap to whole degrees; sync across panes when tone-synced.
                        let deg = wrap180((start_rot + delta).round());
                        self.set_rotation(idx, deg);
                    }
                }
                if resp.drag_stopped_by(PointerButton::Primary) {
                    self.rotate_drag = None;
                }
            }
        }
        if resp.dragged_by(PointerButton::Primary)
            && self.drag_src.is_none()
            && self.rotate_drag.is_none()
        {
            let d = resp.drag_delta();
            self.view_mut(idx).pan(d);
        }
        if !self.selecting_region && resp.clicked() {
            self.current = idx;
        }

        // Right-drag statistics region (selection + outline + stats panel) — not
        // while a crop selection owns the right button.
        if !self.selecting_region {
            self.region_overlay_for_pane(ui, ctx, idx, img_area, img_area, resp.hovered());
            self.line_overlay_for_pane(ui, ctx, idx, img_area, img_area, resp.hovered());
        }

        // Compute-pane controls (source / kind / recompute + inline save).
        if self.panes[idx].compute.is_some() {
            self.draw_compute_ui(ctx, idx, img_area);
        }

        self.draw_header(ui, idx, cell);
        if self.panes[idx].show_opts {
            self.draw_options_popup(ctx, idx, cell);
        }
        self.draw_footer(ui, idx, footer_area(cell));

        // The ctrl-drag reorder border is drawn in a separate pass over all
        // cells (`draw_reorder_borders`), after every pane, so it can't be
        // painted over by a later-drawn neighbour.
    }

    /// Reorder feedback borders for the grid, drawn in one pass **after** every
    /// pane so no later-drawn neighbour can cover an earlier pane's outline.
    /// Inset *inside* the image area (excluding the header/footer info bars) so
    /// the whole outline stays visible even with no gap between cells — blue on
    /// the pane being moved, green on the pane it would swap with.
    pub(super) fn draw_reorder_borders(&self, ui: &egui::Ui, ctx: &egui::Context, cells: &[(usize, Rect)]) {
        let Some(src) = self.drag_src else { return };
        let bw = 2.0;
        let ptr = ctx.input(|i| i.pointer.interact_pos());
        for &(idx, cell) in cells {
            let color = if idx == src {
                Color32::from_rgb(120, 170, 240)
            } else if ptr.is_some_and(|p| cell.contains(p)) {
                Color32::from_rgb(120, 210, 120)
            } else {
                continue;
            };
            // Inset by a full stroke width so the outline sits clear of the cell
            // edges (and the screen edge for the outermost panes).
            let border = image_area(cell).shrink(bw);
            ui.painter().rect_stroke(border, 0.0, Stroke::new(bw, color));
        }
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
        let mod_resp = ui.interact(modify, Id::new(("modify", idx)), Sense::click());
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
        let reload_w = 26.0;
        // The auto-reload (watch) toggle sits left of Reload, but only for panes
        // backed by a file — a Compute pane has its own Auto-refresh instead.
        let watchable = !matches!(self.panes[idx].source, Source::Computed);
        let watch_w = if watchable { 26.0 } else { 0.0 };

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
            .on_hover_text("Reload this media from disk");
        if reload_resp.hovered() {
            hp.rect_filled(reload, 0.0, Color32::from_gray(70));
        }
        hp.text(
            reload.center(),
            Align2::CENTER_CENTER,
            "⟳",
            FontId::proportional(14.0),
            if reload_resp.hovered() {
                Color32::from_gray(235)
            } else {
                Color32::from_gray(170)
            },
        );
        if reload_resp.clicked() {
            self.pending_reload = Some(idx);
        }

        // Auto-reload (watch) toggle, left of Reload. Amber ◉ while watching, a
        // dim ○ otherwise; only shown for file-backed panes.
        if watchable {
            let watch = Rect::from_min_size(
                Pos2::new(reload.min.x - watch_w, header.min.y),
                Vec2::new(watch_w, HEADER_H),
            );
            let watching = self.panes[idx].watch;
            let watch_resp = ui
                .interact(watch, Id::new(("watch", idx)), Sense::click())
                .on_hover_text(if watching {
                    "Auto-reload on: reloads when the file changes on disk. Click to stop."
                } else {
                    "Auto-reload: watch the file and reload it when it changes on disk."
                });
            if watch_resp.hovered() {
                hp.rect_filled(watch, 0.0, Color32::from_gray(70));
            }
            let amber = Color32::from_rgb(240, 200, 80);
            hp.text(
                watch.center(),
                Align2::CENTER_CENTER,
                if watching { "◉" } else { "○" },
                FontId::proportional(14.0),
                if watching {
                    amber
                } else if watch_resp.hovered() {
                    Color32::from_gray(235)
                } else {
                    Color32::from_gray(170)
                },
            );
            if watch_resp.clicked() {
                let on = !watching;
                self.panes[idx].watch = on;
                self.panes[idx].watch_seen = None;
                // Baseline to the current on-disk state when enabling, so turning
                // the watch on never triggers an immediate reload.
                self.panes[idx].watch_loaded = if on {
                    Self::source_file_sig(&self.panes[idx].source)
                } else {
                    None
                };
            }
        }

        let hide_resp = ui.interact(hide, Id::new(("hide", idx)), Sense::click());
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
            self.pending_remove = Some(idx);
        }
    }

    /// The image-space position under screen point `pos` for pane `idx`, but only
    /// when it lands on a real pixel of that pane (so the shared cursor tracks an
    /// actual source sample). `coord_area` maps screen↔image; `clip` bounds where
    /// the pointer counts as being over this pane.
    pub(super) fn hover_img_pos(&self, idx: usize, coord_area: Rect, clip: Rect, pos: Pos2) -> Option<Vec2> {
        if !clip.contains(pos) {
            return None;
        }
        let p = self.rot_screen_to_img(idx, pos, coord_area);
        let [w, h] = self.disp_size(idx);
        (p.x >= 0.0 && p.y >= 0.0 && (p.x as usize) < w && (p.y as usize) < h).then_some(p)
    }

    /// Pane `idx`'s effective display rotation in radians (0 when unrotated).
    pub(super) fn pane_theta(&self, idx: usize) -> f32 {
        self.rotation_of(idx).to_radians()
    }

    /// Screen position of image point `p` for pane `idx`, including the pane's
    /// rotation about its image centre. Inverse of [`rot_screen_to_img`]. Because
    /// the view is a similarity (uniform scale + translate, no rotation), rotating
    /// in image space about the image centre is the same as rotating the mapped
    /// screen point about the image-centre's screen position — so the drawn mesh
    /// (which rotates the image rect's corners) and every overlay stay aligned.
    pub(super) fn rot_img_to_screen(&self, idx: usize, p: Vec2, area: Rect) -> Pos2 {
        let v = self.view_ref(idx);
        let s = v.img_to_screen(p, area);
        let theta = self.pane_theta(idx);
        if theta == 0.0 {
            return s;
        }
        let pivot = v.img_to_screen(center_vec(self.disp_size(idx)), area);
        rotate_around(s, pivot, theta)
    }

    /// Which image pixel is under screen point `s` for pane `idx`, undoing the
    /// pane's rotation. Inverse of [`rot_img_to_screen`].
    pub(super) fn rot_screen_to_img(&self, idx: usize, s: Pos2, area: Rect) -> Vec2 {
        let v = self.view_ref(idx);
        let theta = self.pane_theta(idx);
        if theta == 0.0 {
            return v.screen_to_img(s, area);
        }
        let pivot = v.img_to_screen(center_vec(self.disp_size(idx)), area);
        v.screen_to_img(rotate_around(s, pivot, -theta), area)
    }

    /// The native pixel value at the shared image cursor for pane `idx`: the
    /// value string when on a resident pixel, `…` when the frame isn't loaded,
    /// or `—` when the cursor falls outside this pane's image.
    fn value_string(&self, idx: usize, cursor: Vec2) -> String {
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
    fn draw_cursor_dot(&self, painter: &egui::Painter, idx: usize, coord_area: Rect, clip: Rect) {
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

    /// Bottom status strip: resolution (h×w), the shared cursor pixel, and this
    /// pane's native value there.
    pub(super) fn draw_footer(&self, ui: &egui::Ui, idx: usize, footer: Rect) {
        let fp = ui.painter_at(footer);
        fp.rect_filled(footer, 0.0, Color32::from_gray(28));

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

    /// A/B footer: the shared cursor position with **both** sides' native values,
    /// since the single strip stands in for both panes.
    fn draw_ab_footer(&self, ui: &egui::Ui, a: usize, b: usize, footer: Rect) {
        let fp = ui.painter_at(footer);
        fp.rect_filled(footer, 0.0, Color32::from_gray(28));
        let [w, h] = self.disp_size(a);
        let text = match self.cursor_img {
            Some(ci) => format!(
                "{h}×{w}    x {}  y {}    A {}   B {}",
                ci.x.floor() as i64,
                ci.y.floor() as i64,
                self.value_string(a, ci),
                self.value_string(b, ci),
            ),
            None => format!("{h}×{w}"),
        };
        fp.text(
            footer.left_center() + Vec2::new(8.0, 0.0),
            Align2::LEFT_CENTER,
            text,
            FontId::monospace(12.0),
            Color32::from_gray(200),
        );
    }

    // ---- A/B wipe view ---------------------------------------------------

    pub(super) fn draw_ab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, area: Rect) {
        let n = self.panes.len();
        let a = self.slot_a.min(n - 1);
        let b = self.slot_b.min(n - 1);

        // Reserve a footer strip; images live in `img`.
        let img = ab_image_rect(area);
        let footer = Rect::from_min_max(Pos2::new(area.min.x, area.max.y - FOOTER_H), area.max);

        for &idx in &[a, b] {
            let size = self.panes[idx].media.size();
            let v = self.view_mut(idx);
            if v.needs_fit {
                v.fit(size, img);
            }
        }

        // Textures were staged/committed by `refresh_textures` before drawing.
        let ta = self.pane_texture(a);
        let tb = self.pane_texture(b);
        // Mask overlays apply in A/B too (each side over its own image).
        let oa = self.prepare_overlay(ctx, a);
        let ob = self.prepare_overlay(ctx, b);
        let split_x = img.min.x + self.ab_split.clamp(0.02, 0.98) * img.width();
        let left = Rect::from_min_max(img.min, Pos2::new(split_x, img.max.y));
        let right = Rect::from_min_max(Pos2::new(split_x, img.min.y), img.max);

        // Shared cursor from whichever side the pointer is over.
        if let Some(p) = ctx.input(|i| i.pointer.hover_pos()) {
            let side = if p.x < split_x { a } else { b };
            let clip = if p.x < split_x { left } else { right };
            self.cursor_img = self.hover_img_pos(side, img, clip, p);
            self.cursor_pane = self.cursor_img.map(|_| side);
        }

        self.draw_ab_side(ui, a, ta, oa, img, left, true);
        self.draw_ab_side(ui, b, tb, ob, img, right, false);
        self.draw_pane_error(ui, a, left);
        self.draw_pane_error(ui, b, right);

        // Divider line + grab handle.
        let p = ui.painter_at(img);
        p.line_segment(
            [Pos2::new(split_x, img.min.y), Pos2::new(split_x, img.max.y)],
            Stroke::new(2.0_f32, Color32::from_gray(240)),
        );
        let knob = Pos2::new(split_x, img.center().y);
        p.circle_filled(knob, 9.0, Color32::from_gray(240));
        p.text(
            knob,
            Align2::CENTER_CENTER,
            "↔",
            FontId::proportional(12.0),
            Color32::from_gray(30),
        );

        // Interaction: divider drag, else pan/zoom the side under the cursor.
        let sense = if self.selecting_region {
            Sense::hover()
        } else {
            Sense::click_and_drag()
        };
        let resp = ui.interact(img, Id::new("ab_area"), sense);
        let ptr = ctx.input(|i| i.pointer.interact_pos());
        if !self.selecting_region {
            if resp.drag_started_by(PointerButton::Primary) {
                self.ab_handle_grabbed = ptr.is_some_and(|p| (p.x - split_x).abs() <= HANDLE_HIT);
            }
            if resp.dragged_by(PointerButton::Primary) {
                let d = resp.drag_delta();
                if self.ab_handle_grabbed {
                    self.ab_split = ((split_x + d.x - img.min.x) / img.width()).clamp(0.02, 0.98);
                } else if let Some(pos) = ptr {
                    let side = if pos.x < split_x { a } else { b };
                    self.view_mut(side).pan(d);
                }
            }
            if resp.drag_stopped_by(PointerButton::Primary) {
                self.ab_handle_grabbed = false;
            }
            if resp.hovered() {
                let scroll = wheel_delta(ctx);
                if scroll != 0.0 {
                    if ctx.input(|i| i.modifiers.ctrl) {
                        // Ctrl + wheel scrubs the sequence (up = next, down = prev)
                        // instead of zooming — matches the grid/single pane path.
                        let step = if scroll > 0.0 { Action::NextFrame } else { Action::PrevFrame };
                        self.apply_action(step, ctx);
                    } else if let Some(pos) = ptr {
                        let side = if pos.x < split_x { a } else { b };
                        let speed = zoom_speed(ctx);
                        self.view_mut(side).zoom_at((scroll * speed).exp(), pos, img);
                    }
                }
            }
        }

        // Footer: shared cursor position with both sides' native values.
        self.draw_ab_footer(ui, a, b, footer);

        // Right-drag statistics region on each side. Both sides share `img` as
        // the coordinate area (image_rect maps against it); the clip rect limits
        // the visible side and where a drag may start.
        let ab_hover = resp.hovered();
        self.region_overlay_for_pane(ui, ctx, a, img, left, ab_hover);
        self.region_overlay_for_pane(ui, ctx, b, img, right, ab_hover);
        self.line_overlay_for_pane(ui, ctx, a, img, left, ab_hover);
        self.line_overlay_for_pane(ui, ctx, b, img, right, ab_hover);
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn draw_ab_side(
        &self,
        ui: &egui::Ui,
        idx: usize,
        tex: Option<TextureId>,
        overlay: Option<TextureId>,
        area: Rect,
        clip: Rect,
        is_a: bool,
    ) {
        let painter = ui.painter_at(clip);
        painter.rect_filled(clip, 0.0, Color32::from_gray(18));
        if let Some(id) = tex {
            let rect = self.view_ref(idx).image_rect(self.disp_size(idx), area);
            let theta = self.pane_theta(idx);
            paint_rotated(&painter, id, rect, theta);
            // The mask overlay shares the base image's rect (1:1 in image space).
            if let Some(ov) = overlay {
                paint_rotated(&painter, ov, rect, theta);
            }
        }
        // Replicate the shared cursor on this side (clipped to it).
        self.draw_cursor_dot(&painter, idx, area, clip);
        // Corner label.
        let tag = format!(
            "{}  {}",
            if is_a { "A" } else { "B" },
            self.panes[idx].media.name()
        );
        let anchor = if is_a {
            (clip.left_top() + Vec2::new(8.0, 8.0), Align2::LEFT_TOP)
        } else {
            (clip.right_top() + Vec2::new(-8.0, 8.0), Align2::RIGHT_TOP)
        };
        painter.text(
            anchor.0,
            anchor.1,
            tag,
            FontId::proportional(13.0),
            Color32::from_gray(230),
        );
    }

    // ---- compute pane controls -------------------------------------------

    /// Overlay a Compute pane with a top-left foreground `Area`. Two states:
    /// **unconfigured** shows the config form (mode + source combos + a
    /// **Compute** button that runs it); once computed, the result image shows
    /// with the **Refresh** / **Save** / **Auto refresh** controls instead.
    /// Edits are written back and a recompute / save is dispatched after.
    fn draw_compute_ui(&mut self, ctx: &egui::Context, idx: usize, img_area: Rect) {
        let pane_id = self.panes[idx].id;
        let (mut kind, mut source_id, mut source_b, computed, mut auto, mut saving, mut save_name, status) = {
            let c = self.panes[idx].compute.as_ref().unwrap();
            (
                c.kind,
                c.source_id,
                c.source_b,
                c.computed,
                c.auto,
                c.saving,
                c.save_name.clone(),
                c.status.clone(),
            )
        };
        let sources = self.compute_sources(kind);
        let mut recompute = false;
        let mut do_save = false;

        egui::Area::new(Id::new(("compute_ctrl", pane_id)))
            .order(egui::Order::Foreground)
            .movable(false)
            .constrain_to(img_area)
            .fixed_pos(img_area.left_top() + Vec2::splat(6.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(240.0);
                    if !computed {
                        // Config form: pick the mode + source(s), then Compute.
                        ui.label(egui::RichText::new("New compute").strong());
                        compute_config_rows(
                            ui,
                            pane_id,
                            &sources,
                            &mut kind,
                            &mut source_id,
                            &mut source_b,
                        );
                        let ready = source_id.is_some()
                            && (!matches!(kind, media::Reduce::Diff) || source_b.is_some());
                        if ui.add_enabled(ready, egui::Button::new("Compute")).clicked() {
                            recompute = true;
                        }
                    } else {
                        // Result controls (the form is replaced by the output).
                        ui.horizontal(|ui| {
                            if ui.button("Refresh").clicked() {
                                recompute = true;
                            }
                            if !saving && ui.button("Save").clicked() {
                                saving = true;
                            }
                            ui.checkbox(&mut auto, "Auto refresh");
                        });
                        // Inline save: a name field (relative to the working dir).
                        if saving {
                            ui.add(
                                egui::TextEdit::singleline(&mut save_name)
                                    .desired_width(220.0)
                                    .hint_text("name.tif"),
                            );
                            ui.horizontal(|ui| {
                                if ui.button("Save").clicked() {
                                    do_save = true;
                                }
                                if ui.button("Cancel").clicked() {
                                    saving = false;
                                }
                            });
                        }
                    }
                    if !status.is_empty() {
                        ui.label(egui::RichText::new(&status).weak().small());
                    }
                });
            });

        // Write edits back, then dispatch heavier work outside the closures.
        {
            let c = self.panes[idx].compute.as_mut().unwrap();
            c.kind = kind;
            c.source_id = source_id;
            c.source_b = source_b;
            c.auto = auto;
            c.saving = saving;
            c.save_name = save_name.clone();
        }
        if recompute {
            // Defer to the top of the next update (before `refresh_textures`) so
            // the recompute — which nulls this pane's texture — re-renders in the
            // same lock-step commit as the others, never drawing the pane black.
            self.pending_recompute = Some(idx);
        }
        if do_save {
            self.save_computed(idx, &save_name);
        }
    }
}

/// The mode + source combo rows shared by the floating compute draft and a
/// realized Compute pane. `salt` disambiguates widget ids between the two.
/// `sources` is the caller's kind-filtered source list. Returns true if any
/// selection changed (so the caller can recompute). Diff shows an A and a B
/// picker; the reductions show a single Source.
fn compute_config_rows(
    ui: &mut egui::Ui,
    salt: u64,
    sources: &[(u64, String)],
    kind: &mut Reduce,
    source_id: &mut Option<u64>,
    source_b: &mut Option<u64>,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label("Mode");
        egui::ComboBox::from_id_salt(("ckind", salt))
            .selected_text(kind.label())
            .show_ui(ui, |ui| {
                for k in [Reduce::Mean, Reduce::Std, Reduce::Diff] {
                    if ui.selectable_value(kind, k, k.label()).clicked() {
                        changed = true;
                    }
                }
            });
    });
    let diff = matches!(*kind, Reduce::Diff);
    let mut pick = |ui: &mut egui::Ui, label: &str, id: &str, sel: &mut Option<u64>| {
        ui.horizontal(|ui| {
            ui.label(label);
            let cur = sel
                .and_then(|s| sources.iter().find(|(m, _)| *m == s))
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| "—".into());
            egui::ComboBox::from_id_salt((id, salt))
                .selected_text(cur)
                .show_ui(ui, |ui| {
                    for (mid, mname) in sources {
                        if ui
                            .selectable_value(sel, Some(*mid), format!("{} {}", mid, mname))
                            .clicked()
                        {
                            changed = true;
                        }
                    }
                });
        });
    };
    pick(ui, if diff { "A " } else { "Source " }, "csrc", source_id);
    if diff {
        pick(ui, "B ", "csrcb", source_b);
    }
    changed
}
