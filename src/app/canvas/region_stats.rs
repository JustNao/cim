//! The right-drag statistics region: selection input, the committed outline,
//! and the per-pane stats panel (mini histogram + mean/std/count + the
//! "compute LUT from region" tone pin). The region lives in image space, so
//! it and its stats replicate across every pane.

use crate::app::*;

impl CimApp {
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
                    .rect_stroke(r, 0.0, Stroke::new(1.5_f32, REGION_COL));
            }
            return;
        }

        let Some(reg) = self.stats_region else { return };

        // The region is stored in the pane's *unrotated* view frame (aligned to
        // the viewer, not the image — see `select_region_bounds`), so it maps
        // back with the plain view as an axis-aligned screen rect, matching the
        // rectangle the user dragged regardless of the pane's rotation.
        let v = self.view_ref(idx);
        let r = Rect::from_two_pos(
            v.img_to_screen(reg.min.to_vec2(), coord_area),
            v.img_to_screen(reg.max.to_vec2(), coord_area),
        )
        .intersect(clip_rect);
        if !r.is_positive() {
            return;
        }
        ui.painter_at(clip_rect)
            .rect_stroke(r, 0.0, Stroke::new(1.5_f32, REGION_COL));

        // The stats panel is collapsible: when hidden, a small button under the
        // region brings it back. The region outline above stays visible.
        if self.show_stats {
            self.ensure_region_stats(idx);
            self.draw_stats_panel(ui, idx, r, clip_rect);
        } else {
            self.draw_stats_collapsed(ui, r, clip_rect);
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
        if self.export.selecting {
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
        // Same viewer-aligned release conversion as the export crop.
        self.set_stats_region(self.select_region_bounds(idx, Rect::from_two_pos(s, n), area));
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
        painter.rect_stroke(panel, 0.0, Stroke::new(1.0_f32, REGION_COL));

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
