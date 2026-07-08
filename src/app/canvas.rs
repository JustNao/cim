//! The central image area: grid / single / A-B wipe drawing, pan & zoom,
//! ctrl-drag reorder, per-pane header/footer, and the export-region overlay.

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
    /// image-space crop it covers, clamped to the current image's bounds.
    pub(super) fn screen_rect_to_image(&self, r: Rect, area: Rect) -> Option<Rect> {
        let idx = self.current.min(self.panes.len().checked_sub(1)?);
        let img_area = image_area(area);
        let [w, h] = self.disp_size(idx);
        let a = self.rot_screen_to_img(idx, r.min, img_area);
        let b = self.rot_screen_to_img(idx, r.max, img_area);
        let reg = Rect::from_two_pos(a.to_pos2(), b.to_pos2())
            .intersect(Rect::from_min_max(Pos2::ZERO, Pos2::new(w as f32, h as f32)));
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
                let anchor = ctx
                    .input(|i| i.pointer.hover_pos())
                    .unwrap_or(img_area.center());
                let speed = zoom_speed(ctx);
                self.view_mut(idx).zoom_at((scroll * speed).exp(), anchor, img_area);
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

        // "Hide" and "Close" text buttons at the top-right (matching styles).
        // Hide sets visible = false (keeps the pane); Close removes it.
        let close_w = 44.0;
        let hide_w = 34.0;

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
        let title_right = header.max.x - close_w - hide_w - 6.0;
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
        painter.circle_stroke(sp, 3.5, Stroke::new(1.0, Color32::from_black_alpha(160)));
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
            Stroke::new(2.0, Color32::from_gray(240)),
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
                    if let Some(pos) = ptr {
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

    // ---- right-drag statistics region ------------------------------------

    /// Process the right-drag selection for pane `idx` and draw its result: the
    /// live rubber band while dragging on this pane, otherwise the committed
    /// region outline plus a stats panel. `coord_area` maps screen↔image (the
    /// pane's image area, or the shared A/B image rect); `clip_rect` bounds the
    /// visible side and where a drag may start.
    pub(super) fn region_overlay_for_pane(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        idx: usize,
        coord_area: Rect,
        clip_rect: Rect,
        hovered: bool,
    ) {
        self.region_input(ctx, idx, coord_area, clip_rect, hovered);

        // While actively selecting on this pane, show only the rubber band.
        if self.stats_sel_pane == Some(idx) {
            if let (Some(s), Some(n)) = (self.stats_sel_start, self.stats_sel_now) {
                let r = Rect::from_two_pos(s, n).intersect(clip_rect);
                ui.painter_at(clip_rect)
                    .rect_stroke(r, 0.0, Stroke::new(1.5, REGION_COL));
            }
            return;
        }

        let Some(reg) = self.stats_region else { return };

        // Map the image-space region's four corners onto this pane (rotation-aware),
        // draw them as a closed outline, and take their screen bounding box `r` for
        // clipping / stats-panel placement. On an unrotated pane this is the plain
        // axis-aligned rectangle; on a rotated one it's the tilted region.
        let corners = [
            reg.left_top(),
            reg.right_top(),
            reg.right_bottom(),
            reg.left_bottom(),
        ]
        .map(|c| self.rot_img_to_screen(idx, c.to_vec2(), coord_area));
        let r = corners
            .iter()
            .fold(Rect::NOTHING, |acc, &p| acc.union(Rect::from_min_max(p, p)))
            .intersect(clip_rect);
        if !r.is_positive() {
            return;
        }
        let mut outline: Vec<Pos2> = corners.to_vec();
        outline.push(corners[0]);
        ui.painter_at(clip_rect)
            .add(egui::Shape::line(outline, Stroke::new(1.5, REGION_COL)));

        // The stats panel is collapsible: when hidden, a small button under the
        // region brings it back. The region outline above stays visible.
        if self.show_stats {
            self.ensure_region_stats(idx);
            self.draw_stats_panel(ui, idx, r, clip_rect);
        } else {
            self.draw_stats_collapsed(ui, r, clip_rect);
        }
    }

    // ---- shift+right-drag intensity-profile line -------------------------

    /// Process the shift+right-drag profile line for pane `idx` and draw it (the
    /// amber segment + endpoint handles), mapped onto this pane. `coord_area`
    /// maps screen↔image; `clip_rect` bounds the visible side / where a drag may
    /// start. The line itself lives in image space, so it replicates on every
    /// pane and can be edited from any of them.
    pub(super) fn line_overlay_for_pane(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        idx: usize,
        coord_area: Rect,
        clip_rect: Rect,
        hovered: bool,
    ) {
        self.line_input(ctx, idx, coord_area, clip_rect, hovered);
        self.draw_line_overlay(ui, idx, coord_area, clip_rect);
    }

    /// Draw the profile line and its endpoint handles onto pane `idx`.
    fn draw_line_overlay(&self, ui: &egui::Ui, idx: usize, coord_area: Rect, clip: Rect) {
        let Some(lp) = self.line_profile else { return };
        let sa = self.rot_img_to_screen(idx, lp.a.to_vec2(), coord_area);
        let sb = self.rot_img_to_screen(idx, lp.b.to_vec2(), coord_area);
        let painter = ui.painter_at(clip);
        painter.line_segment([sa, sb], Stroke::new(2.0, LINE_COL));
        for p in [sa, sb] {
            painter.circle_filled(p, 4.0, LINE_COL);
            painter.circle_stroke(p, 4.0, Stroke::new(1.0, Color32::from_black_alpha(180)));
        }
    }

    /// Track the shift+right drag: on press, decide whether it grabs an endpoint,
    /// the body, or draws a new line; follow while held; finalize on release
    /// (a near-zero *new* line is discarded).
    fn line_input(
        &mut self,
        ctx: &egui::Context,
        idx: usize,
        coord_area: Rect,
        hit_rect: Rect,
        hovered: bool,
    ) {
        if self.selecting_region {
            return;
        }
        let down = ctx.input(|i| i.pointer.secondary_down());
        let shift = ctx.input(|i| i.modifiers.shift);
        let pos = ctx.input(|i| i.pointer.interact_pos());
        match self.line_grab {
            None => {
                if !(shift && down && hovered) {
                    return;
                }
                let Some(p) = pos.filter(|p| hit_rect.contains(*p)) else {
                    return;
                };
                let img = self.rot_screen_to_img(idx, p, coord_area).to_pos2();
                // Grab an existing endpoint / body when the press lands on it,
                // otherwise start a fresh line anchored here.
                let grab = match self.line_profile {
                    Some(lp) => {
                        let sa = self.rot_img_to_screen(idx, lp.a.to_vec2(), coord_area);
                        let sb = self.rot_img_to_screen(idx, lp.b.to_vec2(), coord_area);
                        if (p - sa).length() <= LINE_HANDLE {
                            LineGrab::Start
                        } else if (p - sb).length() <= LINE_HANDLE {
                            LineGrab::End
                        } else if dist_to_segment(p, sa, sb) <= LINE_HANDLE {
                            LineGrab::Body
                        } else {
                            LineGrab::New(img)
                        }
                    }
                    None => LineGrab::New(img),
                };
                if let LineGrab::New(anchor) = grab {
                    self.line_profile = Some(LineProfile { a: anchor, b: img });
                }
                self.line_grab = Some(grab);
                self.line_grab_pane = Some(idx);
                self.line_grab_area = coord_area;
                self.line_drag_last = Some(img);
            }
            Some(grab) if self.line_grab_pane == Some(idx) => {
                if !down {
                    self.finalize_line();
                    return;
                }
                let Some(p) = pos else { return };
                let img = self.rot_screen_to_img(idx, p, self.line_grab_area).to_pos2();
                if let Some(lp) = self.line_profile.as_mut() {
                    match grab {
                        LineGrab::Start => lp.a = img,
                        LineGrab::End | LineGrab::New(_) => lp.b = img,
                        LineGrab::Body => {
                            if let Some(last) = self.line_drag_last {
                                let d = img - last;
                                lp.a += d;
                                lp.b += d;
                            }
                        }
                    }
                }
                self.line_drag_last = Some(img);
            }
            _ => {}
        }
    }

    /// End a profile-line drag; a *new* line dragged out to near-zero length is
    /// discarded (so a stray shift+right-click doesn't leave a dot behind).
    fn finalize_line(&mut self) {
        let grab = self.line_grab.take();
        self.line_grab_pane = None;
        self.line_drag_last = None;
        if matches!(grab, Some(LineGrab::New(_))) {
            if let Some(lp) = self.line_profile {
                if (lp.a - lp.b).length() < 2.0 {
                    self.line_profile = None;
                }
            }
        }
    }

    /// Track the right-button drag with simple edge detection on the secondary
    /// button: start on the pane under the cursor, follow while held, finalize
    /// on release. A near-zero drag clears the region instead.
    fn region_input(
        &mut self,
        ctx: &egui::Context,
        idx: usize,
        coord_area: Rect,
        hit_rect: Rect,
        hovered: bool,
    ) {
        if self.selecting_region {
            return; // the export-region drag owns the pointer
        }
        // Shift + right-drag is the intensity-profile line, not a stats region;
        // leave the pointer to `line_input` unless a stats drag is already going.
        if self.stats_sel_pane.is_none() && ctx.input(|i| i.modifiers.shift) {
            return;
        }
        let down = ctx.input(|i| i.pointer.secondary_down());
        let pos = ctx.input(|i| i.pointer.interact_pos());
        match self.stats_sel_pane {
            None => {
                if down && hovered {
                    if let Some(p) = pos {
                        if hit_rect.contains(p) {
                            self.stats_sel_pane = Some(idx);
                            self.stats_sel_start = Some(p);
                            self.stats_sel_now = Some(p);
                            self.stats_sel_area = coord_area;
                        }
                    }
                }
            }
            Some(sel) if sel == idx => {
                if down {
                    if let Some(p) = pos {
                        self.stats_sel_now = Some(p);
                    }
                } else {
                    self.finalize_region(idx);
                }
            }
            _ => {}
        }
    }

    /// Convert the finished right-drag into an image-space region (or clear it
    /// on a near-zero drag), using the pane's view and stored coordinate area.
    fn finalize_region(&mut self, idx: usize) {
        let start = self.stats_sel_start.take();
        let now = self.stats_sel_now.take();
        let area = self.stats_sel_area;
        self.stats_sel_pane = None;
        let (Some(s), Some(n)) = (start, now) else {
            return;
        };
        if (s - n).length() < 4.0 {
            self.set_stats_region(None); // treat a right-click as "clear"
            return;
        }
        let a = self.rot_screen_to_img(idx, s, area);
        let b = self.rot_screen_to_img(idx, n, area);
        let [w, h] = self.disp_size(idx);
        let reg = Rect::from_two_pos(a.to_pos2(), b.to_pos2())
            .intersect(Rect::from_min_max(Pos2::ZERO, Pos2::new(w as f32, h as f32)));
        if reg.width() >= 1.0 && reg.height() >= 1.0 {
            self.set_stats_region(Some(reg));
        } else {
            self.set_stats_region(None);
        }
    }

    /// Draw the stats panel for pane `idx` just below (or above) the on-screen
    /// region rect `r`: a mini histogram with min/max labelled at its ends, a
    /// verbose one-per-row stats list (mean / std / n), and the "compute LUT
    /// from region" toggle that pins every pane's tone to the region.
    fn draw_stats_panel(&mut self, ui: &mut egui::Ui, idx: usize, r: Rect, clip: Rect) {
        let Some(sc) = self.panes[idx].stats.as_ref() else {
            return;
        };
        let data = &sc.data;

        let fmt = |v: f32| -> String {
            if v.fract() == 0.0 {
                format!("{}", v as i64)
            } else {
                format!("{v:.3}")
            }
        };
        let vals = |v: &[f32]| -> String {
            v.iter().map(|x| fmt(*x)).collect::<Vec<_>>().join(" / ")
        };
        // One result per row (mean / std / n), aligned labels.
        let rows = [
            format!("{:<4} = {}", "mean", vals(&data.mean)),
            format!("{:<4} = {}", "std", vals(&data.std)),
            format!("{:<4} = {}", "n", data.count),
        ];

        let pad = 6.0;
        let head_h = 15.0; // top strip holding the collapse button
        let hist_h = 40.0;
        let axis_h = 12.0;
        let line_h = 13.0;
        let btn_h = 20.0;
        let h = pad
            + head_h
            + hist_h
            + 2.0
            + axis_h
            + 4.0
            + rows.len() as f32 * line_h
            + 4.0
            + btn_h
            + pad;
        let w = r.width().max(212.0).min((clip.width() - 2.0).max(120.0));
        // Prefer below the region; fall back to above, then pin inside the clip.
        let top = if r.bottom() + h + 4.0 <= clip.bottom() {
            r.bottom() + 4.0
        } else if r.top() - h - 4.0 >= clip.top() {
            r.top() - h - 4.0
        } else {
            (clip.bottom() - h).max(clip.top())
        };
        let left = r.left().clamp(clip.left(), (clip.right() - w).max(clip.left()));
        let panel = Rect::from_min_size(Pos2::new(left, top), Vec2::new(w, h));

        let painter = ui.painter_at(clip);
        painter.rect_filled(panel, 0.0, Color32::from_black_alpha(205));
        painter.rect_stroke(panel, 0.0, Stroke::new(1.0, REGION_COL));

        let hist_rect = Rect::from_min_size(
            Pos2::new(panel.left() + pad, panel.top() + pad + head_h),
            Vec2::new(w - 2.0 * pad, hist_h),
        );
        let peak_val = draw_hist_curves(&painter, hist_rect, &data.hist);

        // Min / max labelled at the histogram's two ends, and the peak (mode)
        // value centred under it in the tick's amber (mono histograms only).
        let axis_font = FontId::monospace(9.0);
        let axis_col = Color32::from_gray(170);
        painter.text(
            Pos2::new(hist_rect.left(), hist_rect.bottom() + 2.0),
            Align2::LEFT_TOP,
            format!("min = {}", fmt(data.hist.min)),
            axis_font.clone(),
            axis_col,
        );
        if let Some(pv) = peak_val {
            painter.text(
                Pos2::new(hist_rect.center().x, hist_rect.bottom() + 2.0),
                Align2::CENTER_TOP,
                format!("peak = {}", fmt(pv)),
                axis_font.clone(),
                Color32::from_rgb(240, 200, 80),
            );
        }
        painter.text(
            Pos2::new(hist_rect.right(), hist_rect.bottom() + 2.0),
            Align2::RIGHT_TOP,
            format!("max = {}", fmt(data.hist.max)),
            axis_font,
            axis_col,
        );

        // Verbose results, one per row.
        let mut y = hist_rect.bottom() + 2.0 + axis_h + 4.0;
        for row in &rows {
            painter.text(
                Pos2::new(panel.left() + pad, y),
                Align2::LEFT_TOP,
                row,
                FontId::monospace(10.0),
                Color32::from_gray(220),
            );
            y += line_h;
        }

        // Tone toggle at the bottom of the panel. Applies to every pane.
        let btn_rect = Rect::from_min_max(
            Pos2::new(panel.left() + pad, panel.bottom() - btn_h - pad),
            Pos2::new(panel.right() - pad, panel.bottom() - pad),
        );
        let on = self.panes[idx].region_tone;
        let resp = ui.put(
            btn_rect,
            egui::SelectableLabel::new(on, "LUT from region"),
        );
        if resp.clicked() {
            self.apply_region_tone(!on);
        }

        // Collapse button in the top-left corner: hides the panel, leaving the
        // small re-open button under the region (`draw_stats_collapsed`).
        let hide_rect =
            Rect::from_min_size(panel.min + Vec2::splat(3.0), Vec2::new(16.0, head_h - 3.0));
        if ui
            .put(hide_rect, egui::Button::new("–"))
            .on_hover_text("Hide stats")
            .clicked()
        {
            self.show_stats = false;
        }
    }

    // ---- per-pane "Modify" options popup ---------------------------------

    /// The pane options popup (toggled by the header "Transformations" button):
    /// the tone mode, its mode-specific options (`draw_tone_options`), and
    /// Details. Drawn as a foreground `Area` under the header, constrained to the
    /// pane `cell`. When the pane is tone-synced, edits target the *shared*
    /// Transformations (and every synced pane re-renders); otherwise the pane's
    /// own. Nothing is written unless something changed.
    fn draw_options_popup(&mut self, ctx: &egui::Context, idx: usize, cell: Rect) {
        let pane_id = self.panes[idx].id;
        let synced = self.panes[idx].sync_tone;
        // Edit the effective values (shared when synced, else the pane's own).
        let mut contrast = self.contrast_of(idx);
        let mut tone = self.tone_of(idx);
        let mut details = self.details_of(idx);
        // Effective rotation (shared when tone-synced, else the pane's own).
        let mut rotation = self.rotation_of(idx);
        let mut close = false;

        // The proprietary operators (LUT_ALPHA / Details) each need their own
        // loaded library and a single-channel 16-bit frame; gate their controls
        // independently and explain why when disabled.
        let op_input = self.pane_is_op_input(idx);
        let lut_ok = crate::imageproc::lut_alpha_available() && op_input;
        let details_ok = crate::imageproc::details_available() && op_input;
        let lut_hint: &str = if !crate::imageproc::lut_alpha_available() {
            "LUT_ALPHA operator library not found"
        } else {
            "Only available for single-channel 16-bit (uint16) images"
        };
        let details_hint: &str = if !crate::imageproc::details_available() {
            "Details operator library not found"
        } else {
            "Only available for single-channel 16-bit (uint16) images"
        };

        // Overlay (moved here from the Media manager): the single-channel media
        // available to tint over this pane — a boolean mask or a grayscale image /
        // sequence — plus the current selection/colour/alpha. Excludes the pane
        // itself; not offered on a mask pane target.
        let sources: Vec<(u64, String)> = self
            .panes
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != idx && self.overlay_source_size(*i).is_some())
            .map(|(_, p)| (p.id, p.media.name().to_string()))
            .collect();
        let self_is_mask = self.panes[idx].media.is_mask();
        let (mut ov_src, mut ov_color, mut ov_alpha) = match self.overlay_of(idx) {
            Some(o) => (Some(o.src_id), o.color, o.opacity),
            None => (None, Color32::from_rgb(240, 60, 60), 0.5),
        };

        // Histogram of this pane's current frame (folded in from Visualise).
        self.ensure_pane_histogram(idx);
        let f = self.frame_disp(idx);
        let have_hist = self.panes[idx].hist.as_ref().map(|h| h.key) == Some((pane_id, f));

        egui::Area::new(Id::new(("pane_opts", pane_id)))
            .order(egui::Order::Foreground)
            .movable(false)
            .constrain_to(cell)
            .fixed_pos(Pos2::new(
                cell.left() + 4.0,
                cell.top() + header_h_for(cell.width()) + 2.0,
            ))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(230.0);
                    ui.horizontal(|ui| {
                        ui.strong("Transformations");
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.small_button("×").clicked() {
                                    close = true;
                                }
                            },
                        );
                    });
                    if synced {
                        ui.label(
                            egui::RichText::new("Transformations synced")
                                .weak()
                                .small(),
                        );
                    }
                    ui.separator();

                    egui::Grid::new(("opt_grid", pane_id))
                        .num_columns(2)
                        .spacing([8.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("LUT");
                            egui::ComboBox::from_id_salt(("opt_tone", pane_id))
                                .selected_text(contrast.label())
                                .width(130.0)
                                .show_ui(ui, |ui| {
                                    for m in ContrastMode::ORDER {
                                        // LUT_ALPHA needs the library + a 16-bit
                                        // frame; disable it otherwise (unless it's
                                        // already the pane's mode, so it stays
                                        // visible / switchable away).
                                        if m == ContrastMode::LutAlpha
                                            && !lut_ok
                                            && contrast != m
                                        {
                                            ui.add_enabled(
                                                false,
                                                egui::SelectableLabel::new(false, m.label()),
                                            )
                                            .on_disabled_hover_text(lut_hint);
                                        } else {
                                            ui.selectable_value(&mut contrast, m, m.label());
                                        }
                                    }
                                });
                            ui.end_row();

                            draw_tone_options(ui, pane_id, contrast, &mut tone);

                            ui.label("RC");
                            ui.add_enabled(details_ok, egui::Checkbox::without_text(&mut details))
                                .on_hover_text("Rehaussement / sharpening")
                                .on_disabled_hover_text(details_hint);
                            ui.end_row();

                            // Display rotation (about the image centre). Also
                            // editable directly on the pane with Alt + drag.
                            ui.label("Rotate");
                            ui.horizontal(|ui| {
                                // Drag bar (its numeric readout is the text box).
                                ui.add(
                                    egui::Slider::new(&mut rotation, -180.0..=180.0)
                                        .step_by(1.0)
                                        .show_value(false),
                                )
                                .on_hover_text("Rotate the image (Alt + drag on the pane)");

                                // Manual angle entry: a click selects the whole
                                // value so it can be typed straight over; committed
                                // on Enter / focus loss. While this pane's field is
                                // focused, keep the buffer as-typed; otherwise mirror
                                // the live angle.
                                if self.rotation_edit_pane != Some(pane_id) {
                                    self.rotation_edit = fmt_angle(rotation);
                                }
                                let mut out = egui::TextEdit::singleline(&mut self.rotation_edit)
                                    .id(Id::new(("rot_edit", pane_id)))
                                    .desired_width(44.0)
                                    .show(ui);
                                if out.response.gained_focus() {
                                    self.rotation_edit_pane = Some(pane_id);
                                    // Select the whole current value on focus.
                                    let end = self.rotation_edit.chars().count();
                                    out.state.cursor.set_char_range(Some(
                                        egui::text::CCursorRange::two(
                                            egui::text::CCursor::new(0),
                                            egui::text::CCursor::new(end),
                                        ),
                                    ));
                                    out.state.store(ui.ctx(), out.response.id);
                                }
                                if out.response.lost_focus() {
                                    if let Ok(v) = self
                                        .rotation_edit
                                        .trim()
                                        .trim_end_matches('°')
                                        .trim()
                                        .parse::<f32>()
                                    {
                                        rotation = wrap180(v);
                                    }
                                    self.rotation_edit_pane = None;
                                }
                                ui.label("°");
                                if ui.small_button("⟲").on_hover_text("Reset to 0°").clicked() {
                                    rotation = 0.0;
                                    self.rotation_edit_pane = None;
                                }
                            });
                            ui.end_row();
                        });

                    // Overlay picker + colour/alpha (non-mask panes only).
                    if !self_is_mask && !sources.is_empty() {
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label("Overlay");
                            let sel = ov_src
                                .and_then(|id| sources.iter().find(|(m, _)| *m == id))
                                .map(|(_, n)| ellipsize(n, 12))
                                .unwrap_or_else(|| "None".into());
                            egui::ComboBox::from_id_salt(("opt_overlay", pane_id))
                                .selected_text(sel)
                                .width(120.0)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut ov_src, None, "None");
                                    for (mid, mname) in &sources {
                                        ui.selectable_value(
                                            &mut ov_src,
                                            Some(*mid),
                                            ellipsize(mname, 18),
                                        );
                                    }
                                });
                        });
                        if ov_src.is_some() {
                            ui.horizontal(|ui| {
                                ui.color_edit_button_srgba(&mut ov_color);
                                ui.add(
                                    egui::DragValue::new(&mut ov_alpha)
                                        .speed(0.02)
                                        .range(0.0..=1.0)
                                        .fixed_decimals(2)
                                        .prefix("α "),
                                );
                            });
                        }
                    }

                    ui.separator();
                    ui.strong("Histogram");
                    if have_hist {
                        self.draw_histogram(ui, idx);
                    } else {
                        ui.label(egui::RichText::new("frame not loaded").weak().small());
                    }
                });
            });

        // Reconcile the overlay. It rides the tone-sync: when synced, edit the
        // shared overlay and rebuild every synced pane's tinted texture; otherwise
        // just this pane's. A newly *selected* source must match this pane's pixel
        // size — reject a mismatch with an error popup (colour/alpha edits on the
        // same source skip the check).
        let cur = self.overlay_of(idx);
        let new = ov_src.map(|src_id| OverlaySpec {
            src_id,
            color: ov_color,
            opacity: ov_alpha,
        });
        if cur != new {
            let src_changed = new.map(|n| n.src_id) != cur.map(|c| c.src_id);
            let size_ok = match new.filter(|_| src_changed) {
                Some(spec) => match self.panes.iter().position(|p| p.id == spec.src_id) {
                    Some(src) => {
                        let (base, ov) = (self.disp_size(idx), self.disp_size(src));
                        if base == ov {
                            true
                        } else {
                            let sname = self.panes[src].media.name().to_string();
                            self.error_popup = Some(format!(
                                "Overlay size mismatch\n\n\
                                 This image is {}×{} but the overlay “{sname}” is {}×{}.\n\
                                 An overlay must match the image dimensions.",
                                base[0], base[1], ov[0], ov[1],
                            ));
                            false
                        }
                    }
                    None => false, // source vanished; leave the overlay unchanged
                },
                None => true, // clearing, or a colour/alpha edit on the same source
            };
            if size_ok {
                if synced {
                    self.shared_overlay = new;
                    for p in &mut self.panes {
                        if p.sync_tone {
                            p.overlay_tex = None;
                        }
                    }
                } else {
                    self.panes[idx].overlay = new;
                    self.panes[idx].overlay_tex = None;
                }
            }
        }

        // Write the effective tone back (own or shared), invalidating textures.
        if synced {
            if self.shared_contrast != contrast
                || self.shared_tone != tone
                || self.shared_details != details
            {
                self.shared_contrast = contrast;
                self.shared_tone = tone;
                self.shared_details = details;
                self.invalidate_synced_tone();
            }
        } else {
            let p = &mut self.panes[idx];
            if p.contrast != contrast || p.tone != tone || p.details != details {
                p.contrast = contrast;
                p.tone = tone;
                p.details = details;
                p.tex = None; // re-render with the new mapping
            }
        }
        // Rotation is applied at draw time (no texture to invalidate); it rides
        // the Transformations sync, so a synced edit turns every synced pane.
        rotation = wrap180(rotation);
        if self.rotation_of(idx) != rotation {
            self.set_rotation(idx, rotation);
        }
        if close {
            self.panes[idx].show_opts = false;
        }
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
            self.recompute_pane(idx);
        }
        if do_save {
            self.save_computed(idx, &save_name);
        }
    }

    /// The collapsed stats indicator: a small "σ stats" button under the region
    /// `r` that re-opens the panels (replicated, so any pane's button works).
    fn draw_stats_collapsed(&mut self, ui: &mut egui::Ui, r: Rect, clip: Rect) {
        let size = Vec2::new(58.0, 18.0);
        let top = if r.bottom() + size.y + 4.0 <= clip.bottom() {
            r.bottom() + 4.0
        } else {
            (r.top() - size.y - 4.0).max(clip.top())
        };
        let left = r.left().clamp(clip.left(), (clip.right() - size.x).max(clip.left()));
        let btn_rect = Rect::from_min_size(Pos2::new(left, top), size);
        if ui
            .put(btn_rect, egui::Button::new("Stats"))
            .on_hover_text("Show region stats")
            .clicked()
        {
            self.show_stats = true;
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
