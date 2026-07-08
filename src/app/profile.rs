//! The **Line profile** tab: samples each media's pixel intensities along the
//! editable amber line (`line_profile`, drawn with shift + right-drag) and plots
//! them — pixel position on the x axis, native intensity on the y axis. One
//! coloured line per media, with a legend underneath and value/position ticks.

use super::*;

/// Distinct per-media line colours, cycled by media index. Chosen to stay legible
/// on the dark plot background and readily tell apart.
const SERIES_PALETTE: &[Color32] = &[
    Color32::from_rgb(90, 160, 240),
    Color32::from_rgb(240, 130, 90),
    Color32::from_rgb(120, 200, 120),
    Color32::from_rgb(220, 180, 70),
    Color32::from_rgb(200, 120, 220),
    Color32::from_rgb(100, 210, 210),
    Color32::from_rgb(240, 150, 180),
    Color32::from_rgb(170, 170, 90),
];

fn series_color(i: usize) -> Color32 {
    SERIES_PALETTE[i % SERIES_PALETTE.len()]
}

impl CimApp {
    /// Sample pane `idx`'s current frame intensity at `npts` evenly spaced points
    /// along the image-space line `lp`. Points that fall outside this pane's frame
    /// (pages can differ in size) — or a frame that isn't decoded yet — read as
    /// `NaN`, so the plotted curve simply breaks there.
    fn line_samples(&self, idx: usize, lp: LineProfile, npts: usize) -> Vec<f32> {
        let f = self.frame_disp(idx);
        let Some(frame) = self.panes[idx].media.resident(f) else {
            return vec![f32::NAN; npts];
        };
        let [w, h] = frame.size;
        (0..npts)
            .map(|k| {
                let t = k as f32 / (npts - 1).max(1) as f32;
                let p = lp.a + (lp.b - lp.a) * t;
                let (x, y) = (p.x.floor() as i64, p.y.floor() as i64);
                if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                    frame.intensity_at(x as usize, y as usize)
                } else {
                    f32::NAN
                }
            })
            .collect()
    }

    /// The **Line profile** window. Appears while a line exists (drawing one with
    /// shift + right-drag opens it, clearing it closes it) and plots the intensity
    /// of every media along the line, with a legend underneath.
    pub(super) fn draw_profile(&mut self, ctx: &egui::Context) {
        let Some(lp) = self.line_profile else { return };
        egui::Window::new("📈 Line profile")
            .resizable(true)
            .default_width(560.0)
            .default_height(420.0)
            .show(ctx, |ui| {
                // Sample every media along the line (shared x axis = the line's
                // image-space length in pixels).
                let length = (lp.b - lp.a).length();
                let npts = (length.round() as usize + 1).clamp(2, 4096);
                // Only the shown panes plot a curve; hidden ones (Hide) are
                // skipped. Colour stays keyed on the pane index so a media keeps
                // its colour regardless of which others are hidden.
                let series: Vec<(String, Color32, Vec<f32>)> = self
                    .visible_indices()
                    .into_iter()
                    .map(|i| {
                        (
                            self.panes[i].media.name().to_string(),
                            series_color(i),
                            self.line_samples(i, lp, npts),
                        )
                    })
                    .collect();

                self.draw_profile_plot(ui, length, &series);
                ui.add_space(6.0);
                ui.separator();
                draw_profile_legend(ui, &series);

                ui.add_space(4.0);
                if ui.button("Clear line").clicked() {
                    self.line_profile = None;
                }
            });
    }

    /// Draw the plot area itself: axes, gridlines with position (x) and value (y)
    /// ticks, and one coloured polyline per media. `length` is the line's pixel
    /// length (x range `0..length`); the y range spans every series' min/max.
    fn draw_profile_plot(
        &self,
        ui: &mut egui::Ui,
        length: f32,
        series: &[(String, Color32, Vec<f32>)],
    ) {
        let height = 300.0_f32.min((ui.available_height() - 90.0).max(140.0));
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_gray(16));

        // Inner plot rect, leaving room for axis labels (left = y, bottom = x).
        let ml = 52.0;
        let mb = 22.0;
        let mt = 6.0;
        let mr = 8.0;
        let plot = Rect::from_min_max(
            Pos2::new(rect.left() + ml, rect.top() + mt),
            Pos2::new(rect.right() - mr, rect.bottom() - mb),
        );
        if !plot.is_positive() {
            return;
        }

        // Value (y) range over every finite sample: the default min/max.
        let (mut ymin, mut ymax) = (f32::INFINITY, f32::NEG_INFINITY);
        for (_, _, vals) in series {
            for &v in vals {
                if v.is_finite() {
                    ymin = ymin.min(v);
                    ymax = ymax.max(v);
                }
            }
        }
        if !(ymax > ymin) {
            // No data, or a perfectly flat line: give the axis a unit span.
            if ymin.is_finite() {
                ymax = ymin + 1.0;
            } else {
                ymin = 0.0;
                ymax = 1.0;
            }
        }
        let xmax = length.max(1e-3);

        let sx = |x: f32| plot.left() + (x / xmax) * plot.width();
        let sy = |y: f32| plot.bottom() - ((y - ymin) / (ymax - ymin)) * plot.height();

        let grid = Color32::from_gray(40);
        let axis_col = Color32::from_gray(150);
        let font = FontId::monospace(9.0);

        // Y ticks (values) — gridlines + right-aligned labels in the left margin.
        for v in nice_ticks(ymin, ymax, 6) {
            if v < ymin - 1e-4 || v > ymax + 1e-4 {
                continue;
            }
            let y = sy(v);
            painter.line_segment(
                [Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)],
                Stroke::new(1.0, grid),
            );
            painter.text(
                Pos2::new(plot.left() - 4.0, y),
                Align2::RIGHT_CENTER,
                fmt_tick(v),
                font.clone(),
                axis_col,
            );
        }

        // X ticks (position along the line) — gridlines + labels under the axis.
        for v in nice_ticks(0.0, xmax, 6) {
            if v < -1e-4 || v > xmax + 1e-4 {
                continue;
            }
            let x = sx(v);
            painter.line_segment(
                [Pos2::new(x, plot.top()), Pos2::new(x, plot.bottom())],
                Stroke::new(1.0, grid),
            );
            painter.text(
                Pos2::new(x, plot.bottom() + 3.0),
                Align2::CENTER_TOP,
                fmt_tick(v),
                font.clone(),
                axis_col,
            );
        }

        // Axis labels.
        painter.text(
            Pos2::new(plot.center().x, rect.bottom() - 1.0),
            Align2::CENTER_BOTTOM,
            "position (px)",
            font.clone(),
            axis_col,
        );
        painter.text(
            Pos2::new(rect.left() + 2.0, rect.top() + 1.0),
            Align2::LEFT_TOP,
            "intensity",
            font.clone(),
            axis_col,
        );

        // Plot border.
        painter.rect_stroke(plot, 0.0, Stroke::new(1.0, Color32::from_gray(70)));

        // Each media's curve, breaking at NaN gaps so out-of-frame stretches
        // don't draw a false connecting segment.
        for (_, color, vals) in series {
            let mut run: Vec<Pos2> = Vec::new();
            let flush = |run: &mut Vec<Pos2>| {
                if run.len() >= 2 {
                    painter.add(egui::Shape::line(run.clone(), Stroke::new(1.3, *color)));
                }
                run.clear();
            };
            for (k, &v) in vals.iter().enumerate() {
                if v.is_finite() {
                    let t = k as f32 / (vals.len() - 1).max(1) as f32;
                    run.push(Pos2::new(sx(t * xmax), sy(v)));
                } else {
                    flush(&mut run);
                }
            }
            flush(&mut run);
        }
    }
}

/// The legend under the plot: a colour swatch + name for each media.
fn draw_profile_legend(ui: &mut egui::Ui, series: &[(String, Color32, Vec<f32>)]) {
    ui.horizontal_wrapped(|ui| {
        for (name, color, _) in series {
            let (swatch, _) = ui.allocate_exact_size(Vec2::new(14.0, 3.0), Sense::hover());
            ui.painter().rect_filled(swatch, 0.0, *color);
            ui.label(ellipsize(name, 24));
            ui.add_space(8.0);
        }
    });
}

/// Format an axis tick value: whole numbers plainly, else a few decimals.
fn fmt_tick(v: f32) -> String {
    if v.fract().abs() < 1e-4 {
        format!("{}", v.round() as i64)
    } else {
        format!("{v:.2}")
    }
}

/// "Nice" evenly spaced tick values spanning `[min, max]` (roughly `target`
/// of them), rounded to 1/2/5 × 10ⁿ so the labels read cleanly.
fn nice_ticks(min: f32, max: f32, target: usize) -> Vec<f32> {
    if !(max > min) {
        return vec![min];
    }
    let range = nice_num(max - min, false);
    let step = nice_num(range / (target.max(2) - 1) as f32, true).max(f32::MIN_POSITIVE);
    let start = (min / step).floor() * step;
    let end = (max / step).ceil() * step;
    let mut ticks = Vec::new();
    let mut v = start;
    // Cap the count so a degenerate step can't loop unboundedly.
    while v <= end + step * 0.5 && ticks.len() < 64 {
        ticks.push(v);
        v += step;
    }
    ticks
}

/// Round `x` to a "nice" number: the nearest (or, when `round` is false, the
/// smallest enclosing) 1/2/5/10 × 10ⁿ.
fn nice_num(x: f32, round: bool) -> f32 {
    if x <= 0.0 {
        return 1.0;
    }
    let exp = x.log10().floor();
    let frac = x / 10f32.powf(exp);
    let nice = if round {
        if frac < 1.5 {
            1.0
        } else if frac < 3.0 {
            2.0
        } else if frac < 7.0 {
            5.0
        } else {
            10.0
        }
    } else if frac <= 1.0 {
        1.0
    } else if frac <= 2.0 {
        2.0
    } else if frac <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * 10f32.powf(exp)
}
