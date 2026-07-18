//! The central image area: grid / single / A-B wipe drawing, pan & zoom,
//! ctrl-drag reorder, per-pane header/footer, and the export-region overlay.

mod ab;
mod chrome;
mod compute_ui;
mod line_profile;
mod options_popup;
mod region_stats;
mod transform;

use transform::*;

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
                self.cursor_img = hover.and_then(|p| self.hover_img_pos(idx, area, area, p));
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
                        if let Some(ci) = self.hover_img_pos(idx, cell, cell, p) {
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
        if !self.export.show || self.panes.is_empty() {
            return;
        }

        if self.export.selecting {
            // The crop is dragged with the **right** button, so the left button
            // stays free to pan/zoom around the image while choosing it (the pane
            // interaction handles that). Edge-detect the secondary button like the
            // stats region rather than a competing full-area drag interact.
            let down = ctx.input(|i| i.pointer.secondary_down());
            let pos = ctx.input(|i| i.pointer.interact_pos());
            if down {
                if self.export.sel_start.is_none() {
                    // Begin only if the press starts inside the view.
                    self.export.sel_start = pos.filter(|p| area.contains(*p));
                }
                if let (Some(s), Some(c)) = (self.export.sel_start, pos) {
                    self.export.sel_rect = Some(Rect::from_two_pos(s, c).intersect(area));
                }
            } else if self.export.sel_start.is_some() {
                // Right button released: finalize (a near-zero drag clears it).
                self.export.selecting = false;
                self.export.sel_start = None;
                self.export.region = self
                    .export
                    .sel_rect
                    .take()
                    .filter(|r| r.width() >= 4.0 && r.height() >= 4.0)
                    .and_then(|r| self.screen_rect_to_image(r, area));
                if let Some(m) = self.export.pre_select_mode.take() {
                    self.mode = m;
                }
            }
            if let Some(r) = self.export.sel_rect {
                dim_outside(&ui.painter_at(area), area, r);
            }
            return;
        }

        // Region chosen: show it on every pane it applies to.
        let Some(reg) = self.export.region else { return };
        let panes_areas: Vec<(usize, Rect)> = match self.mode {
            Mode::Single => {
                vec![(self.current.min(self.panes.len() - 1), area)]
            }
            Mode::Grid => self.grid_cells(&self.visible_indices(), area),
            // The wipe shares one image area; both sides are spatially the same
            // place, so pane A's view is representative.
            Mode::Ab => vec![(self.slot_a.min(self.panes.len() - 1), area)],
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
    pub(super) fn grid_cells(&self, vis: &[usize], area: Rect) -> Vec<(usize, Rect)> {
        let n = vis.len();
        let cols = self.config.max_columns.max(1).min(n).max(1);
        let rows = n.div_ceil(cols);
        // Column/row edges snapped to the physical pixel grid, so cells tile with
        // no gap and every shared boundary is exact. A fractional edge would make
        // two adjacent cells' fills anti-alias against each other, leaving a
        // faint seam right under a pane's footer (visible only where a bright
        // image sits in the next row — hence never on the bottom row).
        let ppp = self.ppp.max(0.01);
        let snap = |v: f32| (v * ppp).round() / ppp;
        let x_edge = |c: usize| snap(area.min.x + area.width() * c as f32 / cols as f32);
        let y_edge = |r: usize| snap(area.min.y + area.height() * r as f32 / rows as f32);
        let mut cells = Vec::with_capacity(n);
        for (k, &idx) in vis.iter().enumerate() {
            let r = k / cols;
            let c = k % cols;
            cells.push((
                idx,
                Rect::from_min_max(
                    Pos2::new(x_edge(c), y_edge(r)),
                    Pos2::new(x_edge(c + 1), y_edge(r + 1)),
                ),
            ));
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
        // The image fills the whole cell; the header/footer bars float over its
        // top/bottom strips (when shown), so hiding them never moves the image.
        let img_area = cell;
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
        // The header and footer bars float over the cell's top/bottom strips —
        // pushed clear of any full-width global bar covering that window edge
        // (`chrome_insets`), so a top-row header sits below the toolbar and a
        // bottom-row footer above the frame bar rather than under them.
        let (top_in, bot_in) = self.chrome_insets(cell);
        let header_strip = Rect::from_min_size(
            Pos2::new(cell.min.x, cell.min.y + top_in),
            Vec2::new(cell.width(), HEADER_H),
        );
        let footer_strip = Rect::from_min_max(
            Pos2::new(cell.min.x, cell.max.y - bot_in - FOOTER_H),
            Pos2::new(cell.max.x, cell.max.y - bot_in),
        );
        // While the cursor is over either shown bar, the bar owns the input —
        // don't let the pane zoom/rotate/reorder/focus underneath it. (An
        // in-progress pan started lower down still continues; only new
        // interactions are suppressed here.)
        let over_chrome = self.show_chrome
            && ctx.input(|i| i.pointer.hover_pos()).is_some_and(|p| {
                header_strip.contains(p) || footer_strip.contains(p)
            });
        if resp.hovered() && !over_chrome {
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
        if !self.export.selecting && !over_chrome && resp.drag_started_by(PointerButton::Primary) {
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
        if !self.export.selecting && !over_chrome && resp.clicked() {
            self.current = idx;
        }

        // Right-drag statistics region (selection + outline + stats panel) — not
        // while a crop selection owns the right button.
        if !self.export.selecting {
            self.region_overlay_for_pane(ui, ctx, idx, img_area, img_area, resp.hovered());
            self.line_overlay_for_pane(ui, ctx, idx, img_area, img_area, resp.hovered());
        }

        // Compute-pane controls (source / kind / recompute + inline save) —
        // floated below the header at the cell's top-right so it clears both the
        // header bar and the Transformations popup (which opens at the top-left).
        if self.panes[idx].compute.is_some() {
            self.draw_compute_ui(ctx, idx, img_area, header_strip.max.y);
        }

        // The header and footer bars, floating over the image's top/bottom
        // strips. All chrome hides together (`Action::ToggleChrome`) — including
        // the Transformations popup, whose open state survives the round trip.
        if self.show_chrome {
            self.draw_header(ui, idx, header_strip);
            if self.panes[idx].show_opts {
                self.draw_options_popup(ctx, idx, cell, header_strip.max.y);
            }
            self.draw_footer(ui, idx, footer_strip);
        }

        // The ctrl-drag reorder border is drawn in a separate pass over all
        // cells (`draw_reorder_borders`), after every pane, so it can't be
        // painted over by a later-drawn neighbour.
    }

    /// Reorder feedback borders for the grid, drawn in one pass **after** every
    /// pane so no later-drawn neighbour can cover an earlier pane's outline.
    /// Inset inside the cell so the whole outline stays visible even with no gap
    /// between cells — blue on the pane being moved, green on the pane it would
    /// swap with.
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
            let border = cell.shrink(bw);
            ui.painter().rect_stroke(border, 0.0, Stroke::new(bw, color));
        }
    }
    // ---- compute pane controls -------------------------------------------
}