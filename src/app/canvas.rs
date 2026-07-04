//! The central image area: grid / single / A-B wipe drawing, pan & zoom,
//! ctrl-drag reorder, per-pane header/footer, and the export-region overlay.

use super::*;

impl CimApp {
    pub(super) fn draw_central(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let area = ui.available_rect_before_wrap();
        self.last_area = area;

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

        match self.mode {
            Mode::Single => {
                let idx = self.current.min(self.panes.len() - 1);
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
                for &(idx, cell) in &cells {
                    self.draw_pane(ui, ctx, idx, cell);
                }
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
            let resp = ui.interact(area, Id::new("region_sel"), Sense::drag());
            let pos = ctx.input(|i| i.pointer.interact_pos());
            if resp.drag_started() {
                self.sel_start = pos;
            }
            if resp.dragged() {
                if let (Some(s), Some(c)) = (self.sel_start, pos) {
                    self.sel_rect = Some(Rect::from_two_pos(s, c).intersect(area));
                }
            }
            if resp.drag_stopped() {
                self.selecting_region = false;
                self.sel_start = None;
                // Discard a zero-size accidental click, then map to image space.
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
            Mode::Ab => vec![(
                self.slot_a.min(self.panes.len() - 1),
                Rect::from_min_max(area.min, Pos2::new(area.max.x, area.max.y - FOOTER_H - 2.0)),
            )],
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
        let v = self.view_ref(idx);
        let [w, h] = self.disp_size(idx);
        let a = v.screen_to_img(r.min, img_area);
        let b = v.screen_to_img(r.max, img_area);
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

        let (tex, loading) = self.prepare(ctx, idx);
        let overlay = self.prepare_overlay(ctx, idx);
        let painter = ui.painter_at(img_area);
        painter.rect_filled(img_area, 0.0, Color32::from_gray(24));
        if let Some(id) = tex {
            let v = *self.view_ref(idx);
            let rect = v.image_rect(self.disp_size(idx), img_area);
            painter.image(id, rect, uv(), Color32::WHITE);
            // The mask overlay shares the base image's rect (1:1 in image space).
            if let Some(ov) = overlay {
                painter.image(ov, rect, uv(), Color32::WHITE);
            }
        }
        if loading {
            draw_spinner(&painter, img_area, ctx.input(|i| i.time));
        }
        self.draw_pane_error(ui, idx, img_area);

        // Interaction: zoom / pan / ctrl-drag reorder. Disabled while the user
        // is dragging out an export region (so that drag isn't stolen).
        let sense = if self.selecting_region {
            Sense::hover()
        } else {
            Sense::click_and_drag()
        };
        let resp = ui.interact(img_area, Id::new(("pane", idx)), sense);
        if !self.selecting_region {
            let ctrl = ctx.input(|i| i.modifiers.ctrl);
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
            if resp.drag_started() && ctrl {
                self.drag_src = Some(idx);
            }
            if resp.dragged() && self.drag_src.is_none() {
                let d = resp.drag_delta();
                self.view_mut(idx).pan(d);
            }
            if resp.clicked() {
                self.current = idx;
            }
        }

        self.draw_header(ui, idx, cell);
        self.draw_footer(ui, idx, resp.hover_pos(), img_area, footer_area(cell));

        // No persistent pane border (it doubles up at zero gap, breaking the
        // middle pane). Borders show only during a ctrl-drag reorder: blue on
        // the pane being moved, green on the pane it would swap with.
        if self.drag_src == Some(idx) {
            ui.painter()
                .rect_stroke(cell, 0.0, Stroke::new(2.0, Color32::from_rgb(120, 170, 240)));
        } else if self.drag_src.is_some()
            && ctx
                .input(|i| i.pointer.interact_pos())
                .is_some_and(|p| cell.contains(p))
        {
            ui.painter()
                .rect_stroke(cell, 0.0, Stroke::new(2.0, Color32::from_rgb(120, 210, 120)));
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
        let header = Rect::from_min_size(cell.min, Vec2::new(cell.width(), HEADER_H));
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

        let count = self.panes[idx].media.frame_count();
        let name = self.panes[idx].media.name();
        let title = if count > 1 {
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
            format!(
                "{}  {}   {}/{}  ({} in mem){}",
                idx + 1,
                name,
                self.frame_disp(idx) + 1,
                count_str,
                resident,
                sync
            )
        } else {
            format!("{}  {}", idx + 1, name)
        };
        hp.text(
            header.left_center() + Vec2::new(8.0, 0.0),
            Align2::LEFT_CENTER,
            title,
            FontId::proportional(13.0),
            Color32::from_gray(220),
        );

        let close = Rect::from_min_size(
            Pos2::new(header.max.x - HEADER_H, header.min.y),
            Vec2::splat(HEADER_H),
        );
        let close_resp = ui.interact(close, Id::new(("close", idx)), Sense::click());
        hp.text(
            close.center(),
            Align2::CENTER_CENTER,
            "×",
            FontId::proportional(18.0),
            if close_resp.hovered() {
                Color32::from_rgb(230, 120, 120)
            } else {
                Color32::from_gray(160)
            },
        );
        if close_resp.clicked() {
            self.pending_remove = Some(idx);
        }
    }

    /// Bottom status strip: resolution (h×w), cursor pixel, native value.
    pub(super) fn draw_footer(
        &self,
        ui: &egui::Ui,
        idx: usize,
        hover: Option<Pos2>,
        img_area: Rect,
        footer: Rect,
    ) {
        let fp = ui.painter_at(footer);
        fp.rect_filled(footer, 0.0, Color32::from_gray(28));

        let [w, h] = self.disp_size(idx);
        let mut text = format!("{h}×{w}");

        if let Some(pos) = hover {
            if img_area.contains(pos) {
                let p = self.view_ref(idx).screen_to_img(pos, img_area);
                let (x, y) = (p.x.floor() as i64, p.y.floor() as i64);
                if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                    let (x, y) = (x as usize, y as usize);
                    let f = self.frame_disp(idx);
                    if let Some(frame) = self.panes[idx].media.resident(f) {
                        text = format!("{h}×{w}    x {x}  y {y}    {}", frame.pixel_string(x, y));
                    } else {
                        text = format!("{h}×{w}    x {x}  y {y}");
                    }
                }
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

    // ---- A/B wipe view ---------------------------------------------------

    pub(super) fn draw_ab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, area: Rect) {
        let n = self.panes.len();
        let a = self.slot_a.min(n - 1);
        let b = self.slot_b.min(n - 1);

        // Reserve a footer strip; images live in `img`.
        let img = Rect::from_min_max(
            area.min,
            Pos2::new(area.max.x, area.max.y - FOOTER_H - 2.0),
        );
        let footer = Rect::from_min_max(Pos2::new(area.min.x, area.max.y - FOOTER_H), area.max);

        for &idx in &[a, b] {
            let size = self.panes[idx].media.size();
            let v = self.view_mut(idx);
            if v.needs_fit {
                v.fit(size, img);
            }
        }

        let (ta, la) = self.prepare(ctx, a);
        let (tb, lb) = self.prepare(ctx, b);
        let now = ctx.input(|i| i.time);
        let split_x = img.min.x + self.ab_split.clamp(0.02, 0.98) * img.width();
        let left = Rect::from_min_max(img.min, Pos2::new(split_x, img.max.y));
        let right = Rect::from_min_max(Pos2::new(split_x, img.min.y), img.max);

        self.draw_ab_side(ui, a, ta, la, img, left, true, now);
        self.draw_ab_side(ui, b, tb, lb, img, right, false, now);
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
            if resp.drag_started() {
                self.ab_handle_grabbed = ptr.is_some_and(|p| (p.x - split_x).abs() <= HANDLE_HIT);
            }
            if resp.dragged() {
                let d = resp.drag_delta();
                if self.ab_handle_grabbed {
                    self.ab_split = ((split_x + d.x - img.min.x) / img.width()).clamp(0.02, 0.98);
                } else if let Some(pos) = ptr {
                    let side = if pos.x < split_x { a } else { b };
                    self.view_mut(side).pan(d);
                }
            }
            if resp.drag_stopped() {
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

        // Footer readout for whichever side the cursor is over.
        let hover = resp.hover_pos();
        let side = hover.map(|pos| if pos.x < split_x { a } else { b });
        self.draw_footer(ui, side.unwrap_or(a), hover, img, footer);
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn draw_ab_side(
        &self,
        ui: &egui::Ui,
        idx: usize,
        tex: Option<TextureId>,
        loading: bool,
        area: Rect,
        clip: Rect,
        is_a: bool,
        now: f64,
    ) {
        let painter = ui.painter_at(clip);
        painter.rect_filled(clip, 0.0, Color32::from_gray(18));
        if let Some(id) = tex {
            let rect = self.view_ref(idx).image_rect(self.disp_size(idx), area);
            painter.image(id, rect, uv(), Color32::WHITE);
        }
        if loading {
            draw_spinner(&painter, clip, now);
        }
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
}
