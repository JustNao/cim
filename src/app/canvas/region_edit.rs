//! In-place editing shared by the two image-space regions on the canvas — the
//! **export crop** and the **stats region** — so both behave the same once
//! drawn: drag an edge or corner to resize, left-drag inside to move, and a
//! `W × H px` box at the top-left corner to set the size exactly, with the
//! region's **Global / Local** LUT switch right of it. Both regions live in the
//! pane's *unrotated* view frame (see `select_region_bounds`); the live edit is
//! held in a [`RegionEdit`] and committed on release, so whatever keys on the
//! region (tone bounds, stats) recomputes once rather than every drag frame.

use crate::app::*;

/// Which region an edit applies to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(in crate::app) enum EditTarget {
    Export,
    Stats,
}

impl CimApp {
    fn edit_state(&mut self, t: EditTarget) -> &mut RegionEdit {
        match t {
            EditTarget::Export => &mut self.export.edit,
            EditTarget::Stats => &mut self.stats_edit,
        }
    }

    /// The committed region of `t`.
    fn edit_committed(&self, t: EditTarget) -> Option<Rect> {
        match t {
            EditTarget::Export => self.export.region,
            EditTarget::Stats => self.stats_region,
        }
    }

    /// The region of `t` as it is shown: the live edit, else the committed one.
    pub(in crate::app) fn edit_shown(&self, t: EditTarget) -> Option<Rect> {
        let live = match t {
            EditTarget::Export => self.export.edit.resize,
            EditTarget::Stats => self.stats_edit.resize,
        };
        live.or(self.edit_committed(t))
    }

    fn commit_edit(&mut self, t: EditTarget, r: Rect) {
        match t {
            EditTarget::Export => self.export.region = Some(r),
            EditTarget::Stats => self.set_stats_region(Some(r)),
        }
    }

    /// Whether `t`'s Global / Local switch reads Local (pane `idx`'s, for the
    /// stats region — the same on every pane, see `apply_region_tone`).
    fn edit_local(&self, t: EditTarget, idx: usize) -> bool {
        match t {
            EditTarget::Export => self.export.local,
            EditTarget::Stats => self.panes[idx].region_tone,
        }
    }

    fn set_edit_local(&mut self, t: EditTarget, on: bool) {
        match t {
            EditTarget::Export => self.export.local = on,
            EditTarget::Stats => self.apply_region_tone(on),
        }
    }

    /// The image bounds a region on pane `idx` is held inside: the image on an
    /// unrotated pane, unbounded on a rotated one (where the region may take in
    /// background, as `select_region_bounds` allows).
    fn crop_limits(&self, idx: usize) -> Rect {
        if self.pane_theta(idx) == 0.0 {
            let [w, h] = self.disp_size(idx);
            Rect::from_min_max(Pos2::ZERO, Pos2::new(w as f32, h as f32))
        } else {
            Rect::EVERYTHING
        }
    }

    /// The editing controls for region `t` on pane `idx`: `full` is the region's
    /// screen rect (unclipped), `coord` the area that maps screen↔image and
    /// `clip` the visible part of the pane that the handles are held to (the
    /// same rect but on an A/B side). Draws the size box + switch once per pass.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::app) fn region_edit(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        t: EditTarget,
        idx: usize,
        coord: Rect,
        clip: Rect,
        full: Rect,
    ) {
        self.region_handles(ui, t, idx, coord, clip, full);
        if !self.edit_state(t).box_drawn {
            self.edit_state(t).box_drawn = true;
            self.region_size_box(ctx, t, idx, clip, full);
        }
    }

    /// Drag handles on region `t`'s edges and corners, plus the move area
    /// inside it. A drag moves the grabbed edges (or the whole region) to the
    /// pointer, in whole image pixels, into the live edit; the release commits.
    fn region_handles(
        &mut self,
        ui: &mut egui::Ui,
        t: EditTarget,
        idx: usize,
        coord: Rect,
        clip: Rect,
        full: Rect,
    ) {
        const GRIP: f32 = 5.0;
        use egui::CursorIcon as C;
        let (x0, y0, x1, y1) = (full.min.x, full.min.y, full.max.x, full.max.y);
        let span = egui::Rangef::new;
        let corner = |p: Pos2| Rect::from_center_size(p, Vec2::splat(2.0 * GRIP));
        // The (left, top, right, bottom) edges each handle moves, its hit rect
        // and cursor. Corners come last so they win where they overlap a side.
        let handles: [([bool; 4], Rect, C); 8] = [
            (
                [true, false, false, false],
                Rect::from_x_y_ranges(span(x0 - GRIP, x0 + GRIP), span(y0, y1)),
                C::ResizeHorizontal,
            ),
            (
                [false, false, true, false],
                Rect::from_x_y_ranges(span(x1 - GRIP, x1 + GRIP), span(y0, y1)),
                C::ResizeHorizontal,
            ),
            (
                [false, true, false, false],
                Rect::from_x_y_ranges(span(x0, x1), span(y0 - GRIP, y0 + GRIP)),
                C::ResizeVertical,
            ),
            (
                [false, false, false, true],
                Rect::from_x_y_ranges(span(x0, x1), span(y1 - GRIP, y1 + GRIP)),
                C::ResizeVertical,
            ),
            (
                [true, true, false, false],
                corner(full.left_top()),
                C::ResizeNwSe,
            ),
            (
                [false, false, true, true],
                corner(full.right_bottom()),
                C::ResizeNwSe,
            ),
            (
                [false, true, true, false],
                corner(full.right_top()),
                C::ResizeNeSw,
            ),
            (
                [true, false, false, true],
                corner(full.left_bottom()),
                C::ResizeNeSw,
            ),
        ];
        let limits = self.crop_limits(idx);
        // A left-drag inside the region moves it whole, in whole image pixels,
        // held inside the image. Interacted before the edge handles so they win
        // where the two meet; elsewhere it takes the press from the pane's pan.
        let inside = full.shrink(GRIP).intersect(clip);
        if inside.is_positive() {
            let id = egui::Id::new(("region_move", t, idx));
            let resp = ui.interact(inside, id, egui::Sense::drag());
            let left = egui::PointerButton::Primary;
            let resp = if resp.dragged_by(left) {
                resp.on_hover_cursor(C::Grabbing)
            } else {
                resp.on_hover_cursor(C::Grab)
            };
            let base = self.edit_shown(t);
            if let (Some(base), Some(p)) = (base, resp.interact_pointer_pos()) {
                let q = self.view_ref(idx).screen_to_img(p, coord).to_pos2();
                if resp.drag_started_by(left) {
                    self.edit_state(t).grab = Some(q - base.min);
                }
                if let Some(off) = self.edit_state(t).grab.filter(|_| resp.dragged_by(left)) {
                    let size = base.size();
                    let mut min = (q - off).round();
                    // Keep the whole rect inside the limits (no-op when rotated).
                    min.x = min.x.min(limits.max.x - size.x).max(limits.min.x);
                    min.y = min.y.min(limits.max.y - size.y).max(limits.min.y);
                    self.edit_state(t).resize = Some(Rect::from_min_size(min, size));
                }
            }
            if resp.drag_stopped_by(left) {
                self.edit_state(t).grab = None;
                if let Some(r) = self.edit_state(t).resize.take() {
                    self.commit_edit(t, r);
                }
            }
        }
        for (k, (edges, hit, cursor)) in handles.into_iter().enumerate() {
            let hit = hit.intersect(clip);
            if !hit.is_positive() {
                continue;
            }
            let id = egui::Id::new(("region_edge", t, idx, k));
            let resp = ui
                .interact(hit, id, egui::Sense::drag())
                .on_hover_cursor(cursor);
            let Some(base) = self.edit_shown(t) else {
                return;
            };
            if resp.dragged() {
                if let Some(p) = resp.interact_pointer_pos() {
                    let q = self.view_ref(idx).screen_to_img(p, coord);
                    let (qx, qy) = (q.x.round(), q.y.round());
                    let mut r = base;
                    // Each moved edge follows the pointer but stops one pixel
                    // short of the opposite edge, and inside the image.
                    if edges[0] {
                        r.min.x = qx.max(limits.min.x).min(r.max.x - 1.0);
                    }
                    if edges[1] {
                        r.min.y = qy.max(limits.min.y).min(r.max.y - 1.0);
                    }
                    if edges[2] {
                        r.max.x = qx.min(limits.max.x).max(r.min.x + 1.0);
                    }
                    if edges[3] {
                        r.max.y = qy.min(limits.max.y).max(r.min.y + 1.0);
                    }
                    self.edit_state(t).resize = Some(r);
                }
            }
            if resp.drag_stopped() {
                if let Some(r) = self.edit_state(t).resize.take() {
                    self.commit_edit(t, r);
                }
            }
        }
    }

    /// The `W × H px` box at region `t`'s top-left corner (screen rect `full`
    /// over pane `idx`), and right of it the **Global / Local** LUT switch.
    /// Editing a size moves the right / bottom edge, keeping the top-left corner
    /// where it is. The switch is greyed on a full-range tone (operators, Raw),
    /// which a region never restricts.
    fn region_size_box(
        &mut self,
        ctx: &egui::Context,
        t: EditTarget,
        idx: usize,
        clip: Rect,
        full: Rect,
    ) {
        let Some(reg) = self.edit_committed(t) else {
            return;
        };
        let limits = self.crop_limits(idx);
        // Above the corner when there is room, else just inside it; kept in the
        // pane horizontally.
        const ROOM: f32 = 30.0;
        let x = full.min.x.max(clip.min.x);
        let (pivot, pos) = if full.min.y - ROOM >= clip.min.y {
            (egui::Align2::LEFT_BOTTOM, Pos2::new(x, full.min.y - 4.0))
        } else {
            (
                egui::Align2::LEFT_TOP,
                Pos2::new(x + 4.0, full.min.y.max(clip.min.y) + 4.0),
            )
        };
        let shown = self.edit_shown(t).unwrap_or(reg);
        let mut w = shown.width().round().max(1.0) as u32;
        let mut h = shown.height().round().max(1.0) as u32;
        // The most the size can grow to with the top-left corner held.
        let max_w = (limits.max.x - shown.min.x)
            .floor()
            .clamp(1.0, u32::MAX as f32) as u32;
        let max_h = (limits.max.y - shown.min.y)
            .floor()
            .clamp(1.0, u32::MAX as f32) as u32;
        let scope_ok = !self.visual(idx).contrast.full_range();
        let local = self.edit_local(t, idx);
        let mut pick = local;
        let mut changed = false;
        egui::Area::new(egui::Id::new(("region_size", t)))
            .order(egui::Order::Foreground)
            .pivot(pivot)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let hover = t!("export.region_size_hover");
                        changed |= ui
                            .add(egui::DragValue::new(&mut w).range(1..=max_w))
                            .on_hover_text(hover.as_ref())
                            .changed();
                        ui.label("\u{d7}");
                        changed |= ui
                            .add(egui::DragValue::new(&mut h).range(1..=max_h))
                            .on_hover_text(hover.as_ref())
                            .changed();
                        ui.label("px");
                        ui.separator();
                        ui.add_enabled_ui(scope_ok, |ui| {
                            for (is_local, label, tip) in [
                                (false, t!("stats.lut_global"), t!("stats.lut_global_hover")),
                                (true, t!("stats.lut_local"), t!("stats.lut_local_hover")),
                            ] {
                                ui.selectable_value(&mut pick, is_local, label)
                                    .on_hover_text(tip)
                                    .on_disabled_hover_text(t!("stats.lut_scope_off"));
                            }
                        });
                    });
                });
            });
        if changed && self.edit_state(t).resize.is_none() {
            self.commit_edit(
                t,
                Rect::from_min_size(reg.min, Vec2::new(w as f32, h as f32)),
            );
        }
        if pick != local {
            self.set_edit_local(t, pick);
        }
    }
}
