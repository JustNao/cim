//! The toolbar and the floating tool windows: media manager, and settings
//! (layout + keybindings).

use super::*;

impl CimApp {
    pub(super) fn draw_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui.button("Open").clicked() {
                self.open_dialog();
            }
            ui.separator();
            for (mode, label) in [
                (Mode::Grid, "Grid"),
                (Mode::Single, "Single"),
                (Mode::Ab, "A/B"),
            ] {
                if ui.selectable_label(self.mode == mode, label).clicked() {
                    self.mode = mode;
                }
            }
            ui.separator();
            if ui.button("Fit").clicked() {
                self.apply_action_local(Action::ResetView);
            }
            if ui.button("100%").clicked() && !self.panes.is_empty() {
                let size = self.panes[self.current].media.size();
                self.view_mut(self.current).actual_size(size);
            }
            ui.label(format!("{:.0}%", self.view_zoom_label() * 100.0));
            // Playback (Play/speed/Load all) and the scrubber live in the
            // full-width bottom frame bar, shown when the media is a sequence.

            ui.separator();
            if ui.selectable_label(self.show_manager, "Media").clicked() {
                self.show_manager = !self.show_manager;
            }
            if ui.selectable_label(self.show_export, "Export").clicked() {
                self.toggle_export();
            }
            if ui.selectable_label(self.show_viewcmd, "View cmd").clicked() {
                self.show_viewcmd = !self.show_viewcmd;
            }
            if ui.selectable_label(self.show_settings, "Settings").clicked() {
                self.show_settings = !self.show_settings;
            }
        });

        // A/B operand pickers.
        if self.mode == Mode::Ab && !self.panes.is_empty() {
            ui.horizontal(|ui| {
                self.ab_picker(ui, true);
                ui.separator();
                self.ab_picker(ui, false);
            });
        }

        if !self.status.is_empty() {
            ui.label(egui::RichText::new(&self.status).weak().small());
        }
    }

    pub(super) fn ab_picker(&mut self, ui: &mut egui::Ui, is_a: bool) {
        let n = self.panes.len();
        let slot = if is_a { &mut self.slot_a } else { &mut self.slot_b };
        *slot = (*slot).min(n - 1);
        ui.label(if is_a { "A:" } else { "B:" });
        if ui.small_button("◀").clicked() {
            *slot = (*slot + n - 1) % n;
        }
        let name = self.panes[*slot].media.name().to_string();
        ui.monospace(format!("{}·{}", *slot + 1, ellipsize(&name, 16)));
        if ui.small_button("▶").clicked() {
            *slot = (*slot + 1) % n;
        }
    }

    // ---- full-width frame bar -------------------------------------------

    /// The bottom transport strip: play / step controls, the selected media's
    /// name and frame counter, and a full-width scrubber. Shown only while the
    /// focused media is a sequence; its length tracks the selected media.
    pub(super) fn draw_frame_bar(&mut self, ui: &mut egui::Ui) {
        let len = self.timeline_len();
        let at_end = self.current_at_end();
        let cur = self.control.min(self.panes.len().saturating_sub(1));
        let name = self
            .panes
            .get(cur)
            .map(|p| p.media.name().to_string())
            .unwrap_or_default();

        ui.horizontal(|ui| {
            // --- transport group ---
            let play = if self.playing { "Pause" } else { "Play" };
            if ui.button(play).clicked() {
                self.playing = !self.playing;
            }
            if ui.button("Prev").on_hover_text("Previous frame").clicked() {
                self.pending_seek = None;
                if self.shared_frame > 0 {
                    self.shared_frame -= 1;
                } else if at_end {
                    self.shared_frame = len - 1;
                }
            }
            if ui.button("Next").on_hover_text("Next frame").clicked() {
                self.pending_seek = None;
                if self.shared_frame + 1 < len {
                    self.shared_frame += 1;
                } else if at_end {
                    self.shared_frame = 0;
                }
            }
            ui.separator();

            // --- loop group: enable, reset-to-full, and start/end fields ---
            if ui
                .selectable_label(self.loop_playback, "Loop")
                .on_hover_text("Loop playback when it reaches the window end")
                .clicked()
            {
                self.loop_playback = !self.loop_playback;
            }
            if ui
                .add_enabled(self.loop_range.is_some(), egui::Button::new("Full"))
                .on_hover_text("Reset the loop range to the whole sequence")
                .clicked()
            {
                self.loop_range = None;
            }
            // 1-based start/end fields (typeable or draggable). Editing either
            // sets a sub-range; they mirror the timeline brackets.
            let (lo, hi) = self.loop_bounds(len);
            let last = len.saturating_sub(1);
            let (mut s, mut e) = (lo, hi);
            ui.label("[");
            let s_resp = ui.add(egui::DragValue::new(&mut s).range(0..=hi).speed(0.25));
            ui.label("–");
            let e_resp = ui.add(egui::DragValue::new(&mut e).range(lo..=last).speed(0.25));
            ui.label("]");
            if s_resp.changed() {
                self.loop_range = Some((s.min(hi), hi));
            }
            if e_resp.changed() {
                self.loop_range = Some((lo, e.clamp(lo, last)));
            }
            ui.separator();

            // --- rate / load group ---
            ui.add(
                egui::Slider::new(&mut self.fps, 1.0..=60.0)
                    .suffix(" fps")
                    .fixed_decimals(0),
            );
            if ui.button("Load all").clicked() {
                self.load_all();
            }
            ui.separator();
            ui.strong(ellipsize(&name, 40));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // 0-based: current index over the last discovered index.
                let last = len.saturating_sub(1);
                let count = if at_end {
                    format!("{last}")
                } else {
                    format!("{last}+")
                };
                ui.monospace(format!("frame {} / {}", self.shared_frame, count));
            });
        });

        self.draw_scrubber(ui, len, at_end);
    }

    /// A wide, click/drag-seekable frame track filling the panel width. Frames
    /// resident in memory are drawn in the accent colour (the rest greyed);
    /// draggable brackets mark the loop window (dimmed outside); a playhead
    /// marks the current frame, and per-frame ticks appear when short enough.
    pub(super) fn draw_scrubber(&mut self, ui: &mut egui::Ui, len: usize, at_end: bool) {
        let width = ui.available_width();
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(width, 26.0), Sense::click_and_drag());
        let painter = ui.painter_at(rect);

        let span = (len.saturating_sub(1)).max(1) as f32;
        let x_of = |k: usize| rect.left() + rect.width() * (k as f32 / span);
        let (lo, hi) = self.loop_bounds(len);
        let (xlo, xhi) = (x_of(lo), x_of(hi));

        // Base track = "not loaded".
        painter.rect_filled(rect, 0.0, Color32::from_gray(28));

        // Frames resident in memory, in the accent colour. Merge contiguous
        // runs so a long cached span is one rect (cheap, and reads as solid).
        let mut res: Vec<usize> = self
            .panes
            .get(self.control)
            .map(|p| {
                p.media
                    .resident_frames()
                    .into_iter()
                    .map(|(i, _, _)| i)
                    .collect()
            })
            .unwrap_or_default();
        res.sort_unstable();
        let cell = rect.width() / span; // px per frame
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for k in res {
            match runs.last_mut() {
                Some(last) if k == last.1 + 1 => last.1 = k,
                _ => runs.push((k, k)),
            }
        }
        let loaded = Color32::from_rgb(56, 104, 162);
        for (a, b) in runs {
            let xa = (x_of(a) - cell / 2.0).max(rect.left());
            let xb = (x_of(b) + cell / 2.0).min(rect.right());
            painter.rect_filled(
                Rect::from_min_max(Pos2::new(xa, rect.top()), Pos2::new(xb, rect.bottom())),
                0.0,
                loaded,
            );
        }

        // Dim outside the loop window.
        let dim = Color32::from_black_alpha(120);
        if xlo > rect.left() {
            painter.rect_filled(
                Rect::from_min_max(rect.left_top(), Pos2::new(xlo, rect.bottom())),
                0.0,
                dim,
            );
        }
        if xhi < rect.right() {
            painter.rect_filled(
                Rect::from_min_max(Pos2::new(xhi, rect.top()), rect.right_bottom()),
                0.0,
                dim,
            );
        }

        // Per-frame ticks while they stay legible; a "+" marker at the frontier.
        if len <= 80 {
            for k in 0..len {
                let x = x_of(k);
                painter.line_segment(
                    [Pos2::new(x, rect.bottom() - 5.0), Pos2::new(x, rect.bottom())],
                    Stroke::new(1.0, Color32::from_gray(90)),
                );
            }
        }
        if !at_end {
            painter.text(
                rect.right_center() - Vec2::new(6.0, 0.0),
                Align2::RIGHT_CENTER,
                "+",
                FontId::proportional(16.0),
                Color32::from_gray(150),
            );
        }

        // Loop brackets: [ at the start, ] at the end.
        let amber = Color32::from_rgb(240, 200, 80);
        let st = Stroke::new(2.0, amber);
        let (top, bot, cap) = (rect.top(), rect.bottom(), 5.0);
        painter.line_segment([Pos2::new(xlo, top), Pos2::new(xlo, bot)], st);
        painter.line_segment([Pos2::new(xlo, top), Pos2::new(xlo + cap, top)], st);
        painter.line_segment([Pos2::new(xlo, bot), Pos2::new(xlo + cap, bot)], st);
        painter.line_segment([Pos2::new(xhi, top), Pos2::new(xhi, bot)], st);
        painter.line_segment([Pos2::new(xhi, top), Pos2::new(xhi - cap, top)], st);
        painter.line_segment([Pos2::new(xhi, bot), Pos2::new(xhi - cap, bot)], st);

        // Playhead.
        let px = x_of(self.shared_frame.min(len.saturating_sub(1)));
        painter.line_segment(
            [Pos2::new(px, rect.top()), Pos2::new(px, rect.bottom())],
            Stroke::new(2.0, Color32::from_gray(235)),
        );
        painter.circle_filled(Pos2::new(px, rect.center().y), 6.0, Color32::from_gray(240));

        // Interaction: a drag that starts on a bracket moves it (sets the loop
        // range); otherwise a click/drag seeks.
        if len <= 1 {
            return;
        }
        // A generous grab zone on either side of each bracket so they're easy
        // to catch; when both are in reach, the nearer one wins.
        let grab = 18.0;
        let frame_at = |p: Pos2| -> usize {
            let t = ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            (t * span).round() as usize
        };
        if resp.drag_started() {
            self.loop_drag = resp.interact_pointer_pos().and_then(|p| {
                let (dlo, dhi) = ((p.x - xlo).abs(), (p.x - xhi).abs());
                if dlo <= grab || dhi <= grab {
                    Some(dlo <= dhi) // true = start bracket (the nearer one)
                } else {
                    None
                }
            });
        }
        if resp.dragged() {
            if let Some(p) = resp.interact_pointer_pos() {
                let f = frame_at(p);
                match self.loop_drag {
                    Some(true) => self.loop_range = Some((f.min(hi), hi)),
                    Some(false) => self.loop_range = Some((lo, f.max(lo).min(len - 1))),
                    None => {
                        self.pending_seek = None;
                        self.shared_frame = f;
                    }
                }
            }
        }
        if resp.drag_stopped() {
            self.loop_drag = None;
        }
        if resp.clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                self.pending_seek = None;
                self.shared_frame = frame_at(p);
            }
        }
    }

    // ---- view command ----------------------------------------------------

    /// The "View command" window: shows the `cim …` line that reopens the
    /// current files at this exact view, with a button to copy it.
    pub(super) fn draw_viewcmd(&mut self, ctx: &egui::Context) {
        let mut open = self.show_viewcmd;
        let cmd = self.view_command();
        egui::Window::new("View command")
            .open(&mut open)
            .resizable(true)
            .default_width(560.0)
            .show(ctx, |ui| {
                ui.label("Reopen the current files at this exact view by running:");
                ui.add_space(4.0);
                let mut text = cmd.clone();
                ui.add(
                    egui::TextEdit::multiline(&mut text)
                        .desired_width(f32::INFINITY)
                        .desired_rows(3)
                        .font(egui::TextStyle::Monospace),
                );
                ui.add_space(6.0);
                if ui.button("Copy to clipboard").clicked() {
                    ui.output_mut(|o| o.copied_text = cmd.clone());
                    self.status = "View command copied to clipboard".into();
                }
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Captures files, layout, columns, shared zoom/pan, frame, \
                         focus and the A/B split.",
                    )
                    .weak()
                    .small(),
                );
            });
        self.show_viewcmd = open;
    }

    pub(super) fn view_zoom_label(&self) -> f32 {
        if self.panes.is_empty() {
            1.0
        } else {
            self.view_ref(self.current.min(self.panes.len() - 1)).zoom
        }
    }

    pub(super) fn apply_action_local(&mut self, action: Action) {
        if action == Action::ResetView {
            self.shared_view.needs_fit = true;
            for p in &mut self.panes {
                p.transform.needs_fit = true;
            }
        }
    }

    pub(super) fn draw_manager(&mut self, ctx: &egui::Context) {
        let mut open = self.show_manager;
        let shared_view = self.shared_view;
        let shared_frame = self.shared_frame;
        let shared_contrast = self.shared_contrast;
        let shared_tone = self.shared_tone;
        let shared_details = self.shared_details;

        // Row drag-to-reorder: rows collects each media row's (vec index, screen
        // y-band) so a drop can be mapped to a target; do_move carries the
        // resolved (from, to) out to be applied after the window closes.
        let mut rows: Vec<(usize, egui::Rangef)> = Vec::new();
        let mut do_move: Option<(usize, usize)> = None;

        egui::Window::new("☰ Media")
            .open(&mut open)
            .resizable(true)
            .default_width(560.0)
            .show(ctx, |ui| {
                if self.panes.is_empty() {
                    ui.label("No media open. Use 📂 Open or drop files onto the window.");
                    return;
                }

                egui::ScrollArea::vertical().show(ui, |ui| {
                    egui::Grid::new("media_table")
                        .num_columns(8)
                        .striped(true)
                        .spacing([10.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("Show");
                            ui.label("#");
                            ui.label("Name");
                            ui.label("Frames");
                            ui.label("Single");
                            ui.label("A / B");
                            ui.label("Sync")
                                .on_hover_text(
                                    "Pos / Time / Transformations sync (Transf shares the \
                                     per-pane Transformations popup) and the timeline Control",
                                );
                            ui.label("");
                            ui.end_row();

                            // Aggregate row: each toggle here drives the matching
                            // column for every media below it. Single / A / B are
                            // single-target selectors, so they get no aggregate.
                            {
                                let mut all_vis = self.panes.iter().all(|p| p.visible);
                                if ui
                                    .checkbox(&mut all_vis, "")
                                    .on_hover_text("Show / hide all")
                                    .changed()
                                {
                                    for p in &mut self.panes {
                                        p.visible = all_vis;
                                    }
                                }
                                ui.label("");
                                ui.strong("all");
                                ui.label("");
                                ui.label(""); // Single
                                ui.label(""); // A / B
                                ui.horizontal(|ui| {
                                    let mut all_pos = self.panes.iter().all(|p| p.sync_spatial);
                                    if ui.checkbox(&mut all_pos, "Pos").changed() {
                                        for p in &mut self.panes {
                                            if !all_pos && p.sync_spatial {
                                                p.transform = shared_view;
                                            }
                                            p.sync_spatial = all_pos;
                                        }
                                    }
                                    let mut all_time = self.panes.iter().all(|p| p.sync_temporal);
                                    if ui.checkbox(&mut all_time, "Time").changed() {
                                        for p in &mut self.panes {
                                            if !all_time && p.sync_temporal {
                                                p.frame = shared_frame;
                                            }
                                            p.sync_temporal = all_time;
                                        }
                                    }
                                    // Transformations sync: share the per-pane
                                    // Transformations popup across all panes.
                                    let mut all_ts = self.panes.iter().all(|p| p.sync_tone);
                                    if ui
                                        .checkbox(&mut all_ts, "Transformations")
                                        .on_hover_text("Sync the Transformations popup across all")
                                        .changed()
                                    {
                                        for p in &mut self.panes {
                                            if p.sync_tone == all_ts {
                                                continue;
                                            }
                                            if !all_ts {
                                                p.contrast = shared_contrast;
                                                p.tone = shared_tone;
                                                p.details = shared_details;
                                            }
                                            p.sync_tone = all_ts;
                                            p.tex = None;
                                        }
                                    }
                                });
                                if ui
                                    .small_button("⟳")
                                    .on_hover_text("Reload all from disk")
                                    .clicked()
                                {
                                    self.pending_reload_all = true;
                                }
                                ui.end_row();
                            }

                            let mut to_remove = None;
                            let mut to_reload = None;
                            for i in 0..self.panes.len() {
                                let count = self.panes[i].media.frame_count();
                                let resident = self.panes[i].media.resident_count();

                                ui.checkbox(&mut self.panes[i].visible, "");

                                // The index doubles as a drag handle: grab the
                                // ⠿ grip to reorder the media list.
                                let handle = ui
                                    .add(
                                        egui::Label::new(
                                            egui::RichText::new(format!("⠿ {}", i + 1)).monospace(),
                                        )
                                        .selectable(false) // drag it, don't select the text
                                        .sense(Sense::drag()),
                                    )
                                    .on_hover_text("Drag to reorder");
                                if handle.hovered() || self.manager_drag == Some(i) {
                                    ctx.set_cursor_icon(egui::CursorIcon::Grab);
                                }
                                if handle.drag_started() {
                                    self.manager_drag = Some(i);
                                }
                                rows.push((i, handle.rect.y_range()));

                                let name = self.panes[i].media.name().to_string();
                                ui.label(ellipsize(&name, 26));

                                if count > 1 {
                                    ui.monospace(format!("{count}  ({resident}◈)"));
                                } else {
                                    ui.monospace("still");
                                }

                                if ui
                                    .selectable_label(self.current == i, "▢")
                                    .on_hover_text("Show alone in Single view")
                                    .clicked()
                                {
                                    self.current = i;
                                    self.mode = Mode::Single;
                                }

                                ui.horizontal(|ui| {
                                    if ui.selectable_label(self.slot_a == i, "A").clicked() {
                                        self.slot_a = i;
                                    }
                                    if ui.selectable_label(self.slot_b == i, "B").clicked() {
                                        self.slot_b = i;
                                    }
                                });

                                ui.horizontal(|ui| {
                                    let mut ss = self.panes[i].sync_spatial;
                                    if ui.checkbox(&mut ss, "Pos").changed() {
                                        if !ss {
                                            self.panes[i].transform = shared_view;
                                        }
                                        self.panes[i].sync_spatial = ss;
                                    }
                                    let mut st = self.panes[i].sync_temporal;
                                    if ui.checkbox(&mut st, "Time").changed() {
                                        if !st {
                                            self.panes[i].frame = shared_frame;
                                        }
                                        self.panes[i].sync_temporal = st;
                                    }
                                    // Transformations sync: this pane follows the
                                    // shared Transformations (edited in any synced
                                    // pane's popup). Toggling off keeps its look.
                                    let mut ts = self.panes[i].sync_tone;
                                    if ui
                                        .checkbox(&mut ts, "Transformations")
                                        .on_hover_text(
                                            "Sync the Transformations popup with other Transf panes",
                                        )
                                        .changed()
                                    {
                                        self.set_sync_tone(i, ts);
                                    }
                                    // Only a sequence can drive the timeline; pick
                                    // which one the transport / loop follows.
                                    if self.panes[i].media.frame_count() > 1
                                        && ui
                                            .selectable_label(self.control == i, "Control")
                                            .on_hover_text(
                                                "This sequence drives the timeline & playback",
                                            )
                                            .clicked()
                                        && self.control != i
                                    {
                                        self.control = i;
                                        self.loop_range = None; // range is per-sequence
                                    }
                                });

                                ui.horizontal(|ui| {
                                    if ui
                                        .small_button("⟳")
                                        .on_hover_text("Reload this media from disk")
                                        .clicked()
                                    {
                                        to_reload = Some(i);
                                    }
                                    if ui.small_button("×").clicked() {
                                        to_remove = Some(i);
                                    }
                                });
                                ui.end_row();
                            }

                            if let Some(i) = to_remove {
                                self.pending_remove = Some(i);
                            }
                            if let Some(i) = to_reload {
                                self.pending_reload = Some(i);
                            }
                        });
                });

                // Drag-reorder feedback + drop resolution. While a ⠿ handle is
                // held, draw an amber insertion marker on the target row; on
                // release, record the move to apply once the window closes.
                if let Some(from) = self.manager_drag {
                    ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
                    let ptr = ctx.input(|i| i.pointer.interact_pos());
                    let target = ptr.and_then(|p| drop_target(&rows, p.y));
                    // Tint the row in motion so it's clear which one is dragging.
                    if let Some(&(_, band)) = rows.iter().find(|(idx, _)| *idx == from) {
                        ui.painter().rect_filled(
                            Rect::from_x_y_ranges(ui.max_rect().x_range(), band),
                            0.0,
                            Color32::from_rgba_unmultiplied(56, 104, 162, 80),
                        );
                    }
                    if let Some(to) = target {
                        if to != from {
                            if let Some(&(_, band)) = rows.iter().find(|(idx, _)| *idx == to) {
                                let y = if to > from { band.max } else { band.min };
                                ui.painter().hline(
                                    ui.max_rect().x_range(),
                                    y,
                                    Stroke::new(2.0, Color32::from_rgb(240, 200, 80)),
                                );
                            }
                        }
                    }
                    if ctx.input(|i| i.pointer.any_released()) {
                        do_move = target.map(|to| (from, to));
                        self.manager_drag = None;
                    }
                }
            });
        self.show_manager = open;

        if let Some((from, to)) = do_move {
            if from != to && from < self.panes.len() && to < self.panes.len() {
                let p = self.panes.remove(from);
                self.panes.insert(to, p);
                remap_move(&mut self.current, from, to);
                remap_move(&mut self.control, from, to);
                remap_move(&mut self.slot_a, from, to);
                remap_move(&mut self.slot_b, from, to);
            }
        }
    }

    /// Recompute pane `idx`'s cached histogram when its frame changes. The cache
    /// lives **on the pane** (not a single shared slot) so several open
    /// Transformations popups don't thrash one cache — each scan runs once per
    /// frame, not once per popup per repaint.
    pub(super) fn ensure_pane_histogram(&mut self, idx: usize) {
        let f = self.frame_disp(idx);
        let key = (self.panes[idx].id, f);
        if self.panes[idx].hist.as_ref().map(|h| h.key) == Some(key) {
            return;
        }
        if let Some(frame) = self.panes[idx].media.resident(f) {
            self.panes[idx].hist = Some(HistCache {
                key,
                data: frame.histogram_display(256),
            });
        }
    }

    pub(super) fn draw_histogram(&self, ui: &mut egui::Ui, idx: usize) {
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 140.0), Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_gray(16));

        let Some(hist) = &self.panes[idx].hist else { return };
        let data = &hist.data;

        // Peak across every channel/bin; sqrt scaling makes tails legible.
        let peak = data
            .bins
            .iter()
            .flat_map(|c| c.iter().copied())
            .max()
            .unwrap_or(1)
            .max(1) as f32;

        let colors: &[Color32] = if data.mono {
            &[Color32::from_gray(210)]
        } else {
            &[
                Color32::from_rgb(230, 90, 90),
                Color32::from_rgb(90, 210, 90),
                Color32::from_rgb(100, 140, 240),
            ]
        };

        for (ci, chan) in data.bins.iter().enumerate() {
            let nb = chan.len().max(2);
            let mut pts = Vec::with_capacity(nb);
            for (v, &count) in chan.iter().enumerate() {
                let x = rect.left() + (v as f32 / (nb - 1) as f32) * rect.width();
                let h = (count as f32 / peak).sqrt();
                let y = rect.bottom() - h * rect.height();
                pts.push(Pos2::new(x, y));
            }
            painter.add(egui::Shape::line(pts, Stroke::new(1.0, colors[ci])));
        }

        // True value extent under the graph: min at left, max at right.
        // Whole numbers (integer sources) print plainly; floats get 4 digits.
        let fmt = |v: f32| -> String {
            if v.fract() == 0.0 {
                format!("{}", v as i64)
            } else {
                format!("{v:.4}")
            }
        };
        ui.horizontal(|ui| {
            ui.monospace(format!("min {}", fmt(data.min)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.monospace(format!("max {}", fmt(data.max)));
            });
        });
    }

    pub(super) fn draw_settings(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        egui::Window::new("⚙ Settings")
            .open(&mut open)
            .resizable(true)
            .default_width(440.0)
            .show(ctx, |ui| {
                ui.heading("Layout");
                ui.horizontal(|ui| {
                    ui.label("Max columns");
                    ui.add(egui::Slider::new(&mut self.config.max_columns, 1..=8));
                });
                ui.horizontal(|ui| {
                    ui.label("UI scale");
                    ui.add(
                        egui::Slider::new(&mut self.config.ui_scale, 0.6..=2.0)
                            .suffix("×")
                            .fixed_decimals(2),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("Frame cache");
                    ui.add(
                        egui::Slider::new(&mut self.config.cache_budget_mb, 128..=32768)
                            .suffix(" MiB")
                            .logarithmic(true),
                    )
                    .on_hover_text(
                        "Memory ceiling for decoded frames kept resident across all \
                         sequences; oldest unshown frames are evicted beyond it.",
                    );
                });

                ui.add_space(8.0);
                ui.separator();
                ui.heading("Keyboard shortcuts");
                ui.add_space(4.0);

                egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                    egui::Grid::new("keys")
                        .num_columns(3)
                        .striped(true)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            for action in Action::all() {
                                ui.label(action.label());
                                let key_txt = self
                                    .config
                                    .keybindings
                                    .key_for(action)
                                    .map(|k| k.name().to_string())
                                    .unwrap_or_else(|| "—".into());
                                if self.rebinding == Some(action) {
                                    ui.colored_label(
                                        Color32::from_rgb(240, 200, 120),
                                        "press a key…",
                                    );
                                } else {
                                    ui.monospace(key_txt);
                                }
                                ui.horizontal(|ui| {
                                    if ui.small_button("Rebind").clicked() {
                                        self.rebinding = Some(action);
                                    }
                                    if ui.small_button("Clear").clicked() {
                                        self.config.keybindings.clear(action);
                                        self.config.save();
                                    }
                                });
                                ui.end_row();
                            }
                        });
                });

                ui.add_space(8.0);
                ui.separator();
                if ui.button("Save settings").clicked() {
                    self.config.save();
                    self.status = "Settings saved".into();
                }
            });
        self.show_settings = open;
    }
}
