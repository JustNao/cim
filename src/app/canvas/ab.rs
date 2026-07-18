//! The A/B wipe view: two panes split at a draggable divider, pan/zoom acting
//! on the side under the cursor, with a single shared footer reading both
//! panes' values at the shared cursor.

use crate::app::*;

use super::transform::*;

impl CimApp {
    pub(super) fn draw_ab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, area: Rect) {
        let n = self.panes.len();
        let a = self.slot_a.min(n - 1);
        let b = self.slot_b.min(n - 1);

        // Images fill the whole area; the shared footer floats over the bottom
        // strip (when chrome is shown), so hiding it never moves the images. It's
        // pushed clear of the global frame bar (`chrome_insets`), and the A/B
        // corner labels clear of the toolbar, so neither is painted over.
        let img = area;
        let (top_in, bot_in) = self.chrome_insets(area);
        let footer = Rect::from_min_max(
            Pos2::new(area.min.x, area.max.y - bot_in - FOOTER_H),
            Pos2::new(area.max.x, area.max.y - bot_in),
        );

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

        self.draw_ab_side(ui, a, ta, oa, img, left, true, top_in);
        self.draw_ab_side(ui, b, tb, ob, img, right, false, top_in);
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
        let sense = if self.export.selecting {
            Sense::hover()
        } else {
            Sense::click_and_drag()
        };
        let resp = ui.interact(img, Id::new("ab_area"), sense);
        let ptr = ctx.input(|i| i.pointer.interact_pos());
        if !self.export.selecting {
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
                        let step = if scroll > 0.0 {
                            Action::NextFrame
                        } else {
                            Action::PrevFrame
                        };
                        self.apply_action(step, ctx);
                    } else if let Some(pos) = ptr {
                        let side = if pos.x < split_x { a } else { b };
                        let speed = zoom_speed(ctx);
                        self.view_mut(side)
                            .zoom_at((scroll * speed).exp(), pos, img);
                    }
                }
            }
        }

        // Footer: shared cursor position with both sides' native values. Chrome
        // — hidden in the image-only view along with the corner labels; the
        // divider stays (the wipe itself is content, not UI).
        if self.show_chrome {
            self.draw_ab_footer(ui, a, b, footer);
        }

        // Right-drag statistics region on each side. Both sides share `img` as
        // the coordinate area (image_rect maps against it); the clip rect limits
        // the visible side and where a drag may start.
        let ab_hover = resp.hovered();
        self.region_overlay_for_pane(ui, ctx, a, img, left, ab_hover);
        self.region_overlay_for_pane(ui, ctx, b, img, right, ab_hover);
        self.line_overlay_for_pane(ui, ctx, a, img, left, ab_hover);
        self.line_overlay_for_pane(ui, ctx, b, img, right, ab_hover);
    }

    // ---- A/B wipe view ---------------------------------------------------
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
        top_inset: f32,
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
        // Corner label — chrome, hidden in the image-only view.
        if !self.show_chrome {
            return;
        }
        let tag = format!(
            "{}  {}",
            if is_a { "A" } else { "B" },
            self.panes[idx].media.name()
        );
        let anchor = if is_a {
            (
                clip.left_top() + Vec2::new(8.0, 8.0 + top_inset),
                Align2::LEFT_TOP,
            )
        } else {
            (
                clip.right_top() + Vec2::new(-8.0, 8.0 + top_inset),
                Align2::RIGHT_TOP,
            )
        };
        painter.text(
            anchor.0,
            anchor.1,
            tag,
            FontId::proportional(13.0),
            Color32::from_gray(230),
        );
    }

    /// A/B footer: the shared cursor position with **both** sides' native values,
    /// since the single strip stands in for both panes.
    fn draw_ab_footer(&self, ui: &egui::Ui, a: usize, b: usize, footer: Rect) {
        let fp = ui.painter_at(footer);
        fp.rect_filled(footer, 0.0, Color32::from_gray(28));
        // Top border, matching the per-pane footer (it floats over the image).
        fp.hline(
            footer.x_range(),
            footer.min.y + 0.5,
            Stroke::new(1.0_f32, CHROME_BORDER),
        );
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
}
