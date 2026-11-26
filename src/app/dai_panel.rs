//! The **DAI** window: every column of the focused media's DAI
//! ([`crate::dai`]), searchable by fuzzy name, and the selected column over the
//! sequence — as a zoomable graph or a table — where clicking a frame index
//! jumps the timeline there.
//!
//! It tracks `current` like the Transformations panel. The column is kept by
//! **name**, so switching to another media with the same DAI layout keeps the
//! same column on screen, which is the usual way to compare two acquisitions.

use super::profile::{fmt_tick, label_box, nice_ticks};
use super::*;
use crate::dai::{ColType, Dai};
use std::sync::Arc;

/// Pixels of drag below which a right-drag is a click, not a selection.
const MIN_SELECT_PX: f32 = 4.0;
/// The narrowest x span the graph zooms to, in frames.
const MIN_SPAN: f64 = 4.0;

/// The DAI window's state.
#[derive(Default)]
pub(super) struct DaiPanel {
    pub show: bool,
    /// The column-name search field.
    query: String,
    /// The selected column, by name (see the module docs).
    column: Option<String>,
    /// Table instead of graph.
    table: bool,
    /// The graph's visible frame range; `None` = the whole sequence.
    x_range: Option<(f64, f64)>,
    /// Where a range selection (right-drag, or Shift + drag) started, in frames.
    select_from: Option<f64>,
    /// The pane the range above belongs to; another pane resets it.
    pane: Option<u64>,
    /// The selected column as numbers per frame (NaN where it has none), keyed
    /// by what it was built from — rebuilt only when that changes.
    series: Option<(SeriesKey, Vec<f64>)>,
    /// Scroll the table to this row on its next draw.
    scroll_to: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SeriesKey {
    pane: u64,
    dai: usize, // the `Arc<Dai>` address: a reload brings a new one
    column: usize,
    span: usize,
}

impl CimApp {
    /// Whether any pane has DAI — what shows the toolbar's **DAI** button.
    pub(super) fn any_dai(&self) -> bool {
        self.panes.iter().any(|p| p.dai.is_some())
    }

    pub(super) fn draw_dai_panel(&mut self, ctx: &egui::Context) {
        let mut open = self.dai_panel.show;
        let idx = (!self.panes.is_empty()).then(|| self.current.min(self.panes.len() - 1));
        let title = match idx {
            Some(i) => t!("dai.title", name = self.pane_name(i)).into_owned(),
            None => t!("toolbar.dai").into_owned(),
        };
        egui::Window::new(title)
            .id(egui::Id::new("dai_panel"))
            .open(&mut open)
            .resizable(true)
            .default_size(Vec2::new(900.0, 480.0))
            .show(ctx, |ui| {
                let Some(i) = idx else {
                    ui.label(t!("dai.none"));
                    return;
                };
                let Some(dai) = self.panes[i].dai.clone() else {
                    ui.label(t!("dai.none"));
                    return;
                };
                if self.dai_panel.pane != Some(self.panes[i].id) {
                    self.dai_panel.pane = Some(self.panes[i].id);
                    self.dai_panel.x_range = None;
                    self.dai_panel.select_from = None;
                }
                ui.horizontal_top(|ui| {
                    let list_h = ui.available_height();
                    ui.vertical(|ui| {
                        ui.set_width(240.0);
                        ui.set_min_height(list_h);
                        self.draw_dai_columns(ui, &dai);
                    });
                    ui.separator();
                    ui.vertical(|ui| self.draw_dai_column(ui, i, &dai));
                });
            });
        self.dai_panel.show = open;
    }

    /// The searchable column list.
    fn draw_dai_columns(&mut self, ui: &mut egui::Ui, dai: &Dai) {
        ui.add(
            egui::TextEdit::singleline(&mut self.dai_panel.query)
                .hint_text(t!("dai.search_hint"))
                .desired_width(f32::INFINITY),
        );
        let mut hits: Vec<(i32, usize)> = dai
            .columns
            .iter()
            .enumerate()
            .filter_map(|(k, c)| {
                Some((crate::dai::fuzzy_score(&self.dai_panel.query, &c.name)?, k))
            })
            .collect();
        // Best match first; ties keep the file's column order.
        hits.sort_by_key(|&(score, k)| (std::cmp::Reverse(score), k));
        // The count's hover names the DAI file(s) the columns come from.
        let files: Vec<String> = dai
            .tables
            .iter()
            .filter_map(|t| t.path.file_name()?.to_str().map(String::from))
            .collect();
        ui.weak(t!(
            "dai.columns",
            shown = hits.len(),
            total = dai.columns.len()
        ))
        .on_hover_text(files.join("\n"));
        egui::ScrollArea::vertical()
            .id_salt("dai_columns")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (_, k) in hits {
                    let col = &dai.columns[k];
                    let selected = self.dai_panel.column.as_deref() == Some(&col.name);
                    ui.horizontal(|ui| {
                        let r = ui.selectable_label(selected, &col.name);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.weak(type_label(col.ty));
                        });
                        if r.clicked() {
                            self.dai_panel.column = Some(col.name.clone());
                            self.dai_panel.scroll_to = None;
                        }
                    });
                }
            });
    }

    /// The selected column: the view switch and the graph or table.
    fn draw_dai_column(&mut self, ui: &mut egui::Ui, i: usize, dai: &Arc<Dai>) {
        let Some(k) = self
            .dai_panel
            .column
            .as_deref()
            .and_then(|name| dai.columns.iter().position(|c| c.name == name))
        else {
            ui.weak(t!("dai.no_column"));
            return;
        };
        let col = &dai.columns[k];
        let span = dai.span(&self.panes[i].media).max(1);
        let numeric = col.ty.numeric();
        let current = self.frame_disp(i);
        ui.horizontal(|ui| {
            ui.strong(&col.name);
            ui.weak(type_label(col.ty));
            ui.separator();
            ui.add_enabled_ui(numeric, |ui| {
                if ui
                    .selectable_label(numeric && !self.dai_panel.table, t!("dai.graph"))
                    .on_disabled_hover_text(t!("dai.graph_needs_numbers"))
                    .clicked()
                {
                    self.dai_panel.table = false;
                }
            });
            if ui
                .selectable_label(!numeric || self.dai_panel.table, t!("dai.table"))
                .clicked()
            {
                self.dai_panel.table = true;
            }
            ui.separator();
            if ui
                .add_enabled(
                    self.dai_panel.x_range.is_some(),
                    egui::Button::new(t!("dai.whole")),
                )
                .on_hover_text(t!("dai.whole_hover"))
                .clicked()
            {
                self.dai_panel.x_range = None;
            }
            if ui.button(t!("dai.to_current")).clicked() {
                self.dai_panel.scroll_to = Some(current);
                // The graph: bring the playhead into view, keeping the zoom.
                if let Some((lo, hi)) = self.dai_panel.x_range {
                    let c = current as f64;
                    if c < lo || c > hi {
                        let half = (hi - lo) / 2.0;
                        self.dai_panel.x_range = Some(clamp_range(c - half, c + half, span));
                    }
                }
            }
        });
        if numeric && !self.dai_panel.table {
            let key = SeriesKey {
                pane: self.panes[i].id,
                dai: Arc::as_ptr(dai) as usize,
                column: k,
                span,
            };
            if self
                .dai_panel
                .series
                .as_ref()
                .is_none_or(|(k, _)| *k != key)
            {
                let media = &self.panes[i].media;
                let values = (0..span)
                    .map(|f| {
                        dai.value(media, f, k)
                            .and_then(|v| v.as_f64())
                            .unwrap_or(f64::NAN)
                    })
                    .collect();
                self.dai_panel.series = Some((key, values));
            }
            ui.weak(t!("dai.plot_hint"));
            if let Some(target) = self.draw_dai_graph(ui, span, current) {
                self.dai_jump(i, target);
            }
        } else if let Some(target) = self.draw_dai_table(ui, i, dai, k, span, current) {
            self.dai_jump(i, target);
        }
    }

    /// The graph of the cached series over `x_range`. Returns a clicked frame.
    fn draw_dai_graph(&mut self, ui: &mut egui::Ui, span: usize, current: usize) -> Option<usize> {
        let size = Vec2::new(ui.available_width(), ui.available_height().max(160.0));
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_gray(16));
        let plot = Rect::from_min_max(
            Pos2::new(rect.left() + 70.0, rect.top() + 8.0),
            Pos2::new(rect.right() - 10.0, rect.bottom() - 22.0),
        );
        if !plot.is_positive() {
            return None;
        }
        let full = (-0.5, span as f64 - 0.5);
        let (mut lo, mut hi) = self.dai_panel.x_range.unwrap_or(full);
        let px_per = |lo: f64, hi: f64| plot.width() as f64 / (hi - lo);
        let x_at = |sx: f32, lo: f64, hi: f64| lo + (sx - plot.left()) as f64 / px_per(lo, hi);

        // ---- Interaction: wheel zoom, drag pan, right / Shift drag select ----
        let shift = ui.input(|i| i.modifiers.shift);
        let hover = resp.hover_pos().filter(|p| plot.x_range().contains(p.x));
        if let Some(p) = hover {
            let d = ui.input(|i| i.raw_scroll_delta.y + i.raw_scroll_delta.x);
            if d != 0.0 {
                let anchor = x_at(p.x, lo, hi);
                let f = (-d as f64 * 0.0025).exp();
                let (a, b) =
                    clamp_range(anchor - (anchor - lo) * f, anchor + (hi - anchor) * f, span);
                (lo, hi) = (a, b);
                self.dai_panel.x_range = Some((lo, hi));
            }
        }
        let selecting = resp.dragged_by(PointerButton::Secondary)
            || (shift && resp.dragged_by(PointerButton::Primary));
        if (resp.drag_started_by(PointerButton::Secondary)
            || (shift && resp.drag_started_by(PointerButton::Primary)))
            && resp.interact_pointer_pos().is_some()
        {
            let p = resp.interact_pointer_pos().unwrap_or_default();
            self.dai_panel.select_from = Some(x_at(p.x, lo, hi));
        } else if resp.dragged_by(PointerButton::Primary) && self.dai_panel.select_from.is_none() {
            let dx = resp.drag_delta().x as f64 / px_per(lo, hi);
            if dx != 0.0 {
                (lo, hi) = clamp_range(lo - dx, hi - dx, span);
                self.dai_panel.x_range = Some((lo, hi));
            }
        }
        let pointer_x = resp.interact_pointer_pos().map(|p| x_at(p.x, lo, hi));
        if resp.drag_stopped() {
            if let (Some(a), Some(b)) = (self.dai_panel.select_from.take(), pointer_x) {
                if ((b - a) * px_per(lo, hi)).abs() as f32 >= MIN_SELECT_PX {
                    (lo, hi) = clamp_range(a.min(b), a.max(b), span);
                    self.dai_panel.x_range = Some((lo, hi));
                }
            }
        }
        if self
            .dai_panel
            .x_range
            .is_some_and(|r| r == clamp_range(full.0, full.1, span))
        {
            self.dai_panel.x_range = None; // zoomed all the way out: the default
        }
        let mut jump = None;
        if resp.clicked() {
            if let Some(x) = pointer_x {
                jump = Some((x.round().max(0.0) as usize).min(span - 1));
            }
        }

        // ---- Data in view ----
        let Some((_, values)) = &self.dai_panel.series else {
            return jump;
        };
        let first = lo.ceil().max(0.0) as usize;
        let last = (hi.floor().max(0.0) as usize).min(span - 1);
        let (mut ymin, mut ymax) = (f64::INFINITY, f64::NEG_INFINITY);
        for &v in &values[first.min(last)..=last] {
            if v.is_finite() {
                ymin = ymin.min(v);
                ymax = ymax.max(v);
            }
        }
        if !ymin.is_finite() {
            (ymin, ymax) = (0.0, 1.0);
        } else if ymax <= ymin {
            (ymin, ymax) = (ymin - 0.5, ymax + 0.5);
        } else {
            let pad = (ymax - ymin) * 0.05;
            (ymin, ymax) = (ymin - pad, ymax + pad);
        }
        let sx = |x: f64| plot.left() + ((x - lo) * px_per(lo, hi)) as f32;
        let sy = |y: f64| plot.bottom() - ((y - ymin) / (ymax - ymin)) as f32 * plot.height();

        // ---- Axes ----
        let grid = Color32::from_gray(40);
        let axis = Color32::from_gray(150);
        let font = FontId::proportional(10.0);
        for v in nice_ticks(ymin as f32, ymax as f32, 6) {
            let y = sy(v as f64);
            if y < plot.top() - 0.5 || y > plot.bottom() + 0.5 {
                continue;
            }
            painter.line_segment(
                [Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)],
                (1.0, grid),
            );
            painter.text(
                Pos2::new(plot.left() - 4.0, y),
                Align2::RIGHT_CENTER,
                fmt_tick(v),
                font.clone(),
                axis,
            );
        }
        for v in nice_ticks(lo.max(0.0) as f32, hi as f32, 8) {
            if v.fract() != 0.0 || (v as f64) < lo || (v as f64) > hi {
                continue; // frames are whole
            }
            let x = sx(v as f64);
            painter.line_segment(
                [Pos2::new(x, plot.top()), Pos2::new(x, plot.bottom())],
                (1.0, grid),
            );
            painter.text(
                Pos2::new(x, plot.bottom() + 3.0),
                Align2::CENTER_TOP,
                fmt_tick(v),
                font.clone(),
                axis,
            );
        }

        // ---- The series: a polyline when sparse, a min/max envelope per pixel
        // column when there are more frames than pixels ----
        let line = Color32::from_rgb(90, 160, 240);
        let n = last + 1 - first.min(last + 1);
        if n as f32 > plot.width() * 1.5 {
            let cols = plot.width().floor() as usize;
            for c in 0..cols {
                let a = x_at(plot.left() + c as f32, lo, hi)
                    .ceil()
                    .max(first as f64) as usize;
                let b = (x_at(plot.left() + c as f32 + 1.0, lo, hi).ceil() as usize).min(last + 1);
                let (mut mn, mut mx) = (f64::INFINITY, f64::NEG_INFINITY);
                for &v in values.get(a..b).unwrap_or(&[]) {
                    if v.is_finite() {
                        mn = mn.min(v);
                        mx = mx.max(v);
                    }
                }
                if mn.is_finite() {
                    let x = plot.left() + c as f32 + 0.5;
                    painter.line_segment(
                        [Pos2::new(x, sy(mx) - 0.5), Pos2::new(x, sy(mn) + 0.5)],
                        (1.0, line),
                    );
                }
            }
        } else {
            let mut run: Vec<Pos2> = Vec::new();
            let flush = |run: &mut Vec<Pos2>| {
                if run.len() > 1 {
                    painter.add(egui::Shape::line(std::mem::take(run), (1.5, line)));
                } else {
                    run.clear();
                }
            };
            let dots = (n as f32) < plot.width() / 6.0;
            for (f, &v) in values.iter().enumerate().take(last + 1).skip(first) {
                if v.is_finite() {
                    let p = Pos2::new(sx(f as f64), sy(v));
                    if dots {
                        painter.circle_filled(p, 2.5, line);
                    }
                    run.push(p);
                } else {
                    flush(&mut run);
                }
            }
            flush(&mut run);
        }

        // ---- Playhead, selection band, hover readout ----
        let cur_x = sx(current as f64);
        if plot.x_range().contains(cur_x) {
            painter.line_segment(
                [
                    Pos2::new(cur_x, plot.top()),
                    Pos2::new(cur_x, plot.bottom()),
                ],
                (1.5, Color32::from_rgb(240, 180, 60)),
            );
        }
        if let (Some(a), true) = (self.dai_panel.select_from, selecting) {
            if let Some(b) = pointer_x {
                let (x0, x1) = (sx(a.min(b)), sx(a.max(b)));
                let band = Rect::from_x_y_ranges(
                    x0.max(plot.left())..=x1.min(plot.right()),
                    plot.y_range(),
                );
                painter.rect_filled(
                    band,
                    0.0,
                    Color32::from_rgba_unmultiplied(120, 160, 240, 40),
                );
            }
        }
        if let Some(p) = hover.filter(|_| !selecting) {
            let f = (x_at(p.x, lo, hi).round().max(0.0) as usize).min(span - 1);
            let x = sx(f as f64);
            let cross = Color32::from_gray(200);
            painter.line_segment(
                [Pos2::new(x, plot.top()), Pos2::new(x, plot.bottom())],
                (1.0, cross),
            );
            let v = values[f];
            if v.is_finite() {
                painter.circle_filled(Pos2::new(x, sy(v)), 3.5, line);
            }
            let text = format!(
                "#{f}  {}",
                if v.is_finite() {
                    crate::dai::fmt_float(v)
                } else {
                    "—".into()
                }
            );
            let align = if x > plot.center().x {
                Align2::RIGHT_TOP
            } else {
                Align2::LEFT_TOP
            };
            let at = Pos2::new(
                x + if x > plot.center().x { -6.0 } else { 6.0 },
                plot.top() + 4.0,
            );
            label_box(&painter, at, align, text, font, cross);
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        jump
    }

    /// The column as a table of `frame | value`, virtualised (only the visible
    /// rows are laid out), the current frame highlighted. Returns a clicked frame.
    fn draw_dai_table(
        &mut self,
        ui: &mut egui::Ui,
        i: usize,
        dai: &Dai,
        col: usize,
        span: usize,
        current: usize,
    ) -> Option<usize> {
        let row_h = ui.text_style_height(&egui::TextStyle::Body).max(18.0);
        let mut area = egui::ScrollArea::vertical()
            .id_salt("dai_table")
            .auto_shrink([false, false]);
        if let Some(r) = self.dai_panel.scroll_to.take() {
            let step = row_h + ui.spacing().item_spacing.y;
            let offset = (r as f32 * step - ui.available_height() / 2.0).max(0.0);
            area = area.vertical_scroll_offset(offset);
        }
        let mut jump = None;
        let media = &self.panes[i].media;
        area.show_rows(ui, row_h, span, |ui, rows| {
            for f in rows {
                ui.horizontal(|ui| {
                    ui.set_height(row_h);
                    let label = egui::RichText::new(format!("{f:>7}")).monospace();
                    if ui
                        .selectable_label(f == current, label)
                        .on_hover_text(t!("dai.jump_hover"))
                        .clicked()
                    {
                        jump = Some(f);
                    }
                    ui.separator();
                    let text = dai
                        .value(media, f, col)
                        .map_or_else(|| "—".into(), |v| v.display());
                    ui.label(egui::RichText::new(text).monospace());
                });
            }
        });
        jump
    }

    /// Jump pane `i`'s timeline to `target` from the DAI window. A frame past
    /// what has been discovered is reached by **offsets** — a fast jump when the
    /// layout allows, else riding the frontier with header probes — never by
    /// decoding the frames in between.
    fn dai_jump(&mut self, i: usize, target: usize) {
        let clock = self.clock;
        let p = &mut self.panes[i];
        if target >= p.media.frame_count()
            && !p.media.at_end()
            && media::fast_jump(&mut p.media, target).is_ok()
        {
            p.media.touch(target, clock);
        }
        if i == self.transport() && self.transport_own() {
            self.transport_seek(target);
        } else if !self.panes[i].sync_temporal {
            let len = self.panes[i].media.frame_count().max(1);
            self.panes[i].frame = target.min(len - 1);
        } else {
            self.seek_to(target);
        }
    }
}

/// `[lo, hi]` kept inside the sequence and at least `MIN_SPAN` wide (or the
/// whole sequence, when that is shorter), shifted rather than squashed at an end.
fn clamp_range(lo: f64, hi: f64, span: usize) -> (f64, f64) {
    let (min, max) = (-0.5, span as f64 - 0.5);
    let full = max - min;
    let w = (hi - lo).clamp(MIN_SPAN.min(full), full);
    let mid = (lo + hi) / 2.0;
    let lo = (mid - w / 2.0).clamp(min, max - w);
    (lo, lo + w)
}

fn type_label(ty: ColType) -> std::borrow::Cow<'static, str> {
    match ty {
        ColType::Int => t!("dai.type_int"),
        ColType::Float => t!("dai.type_float"),
        ColType::Text => t!("dai.type_text"),
        ColType::Empty => t!("dai.type_empty"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_graph_range_stays_inside_the_sequence() {
        assert_eq!(clamp_range(-50.0, 20.0, 100), (-0.5, 69.5)); // shifted, not squashed
        assert_eq!(clamp_range(90.0, 200.0, 100), (-0.5, 99.5)); // capped at the whole
        assert_eq!(clamp_range(90.0, 110.0, 100), (79.5, 99.5));
        assert_eq!(clamp_range(10.0, 10.5, 100), (8.25, 12.25)); // MIN_SPAN wide
        assert_eq!(clamp_range(-1e9, 1e9, 100), (-0.5, 99.5));
        assert_eq!(clamp_range(0.0, 1.0, 2), (-0.5, 1.5)); // shorter than MIN_SPAN
    }
}
