//! The toolbar and the floating tool windows: media manager, and settings
//! (layout + keybindings).

use super::*;

impl CimApp {
    pub(super) fn draw_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui
                .button("Open")
                .on_hover_text(self.hover_for(Action::OpenFiles, ""))
                .clicked()
            {
                self.open_dialog();
            }
            ui.separator();
            for (mode, label, action) in [
                (Mode::Grid, "Grid", Action::ViewGrid),
                (Mode::Single, "Single", Action::ViewSingle),
                (Mode::Ab, "A/B", Action::ViewAb),
            ] {
                if ui
                    .selectable_label(self.mode == mode, label)
                    .on_hover_text(self.hover_for(action, ""))
                    .clicked()
                {
                    self.mode = mode;
                }
            }
            ui.separator();
            if ui
                .button("Fit")
                .on_hover_text(self.hover_for(Action::ResetView, ""))
                .clicked()
            {
                self.apply_action_local(Action::ResetView);
            }
            if ui
                .button("100%")
                .on_hover_text(self.hover_for(Action::ActualSize, ""))
                .clicked()
                && !self.panes.is_empty()
            {
                let size = self.panes[self.current].media.size();
                self.view_mut(self.current).actual_size(size);
            }
            ui.label(format!("{:.0}%", self.view_zoom_label() * 100.0));
            // Playback (Play/speed/Load all) and the scrubber live in the
            // full-width bottom frame bar, shown when the media is a sequence.

            ui.separator();
            if ui
                .selectable_label(self.show_manager, "Media")
                .on_hover_text(self.hover_for(Action::ToggleManager, ""))
                .clicked()
            {
                self.show_manager = !self.show_manager;
            }
            if ui
                .selectable_label(self.show_transform, "Transformations")
                .on_hover_text(self.hover_for(
                    Action::ToggleVis,
                    "Tone / details / overlay / rotation for the selected pane",
                ))
                .clicked()
            {
                self.show_transform = !self.show_transform;
            }
            if ui
                .selectable_label(false, "Compute")
                .on_hover_text(
                    self.hover_for(Action::OpenCompute, "Add a Compute pane (mean / std / diff of other media)"),
                )
                .clicked()
            {
                self.deferred.push(Deferred::CreateCompute);
            }
            if ui
                .selectable_label(self.export.show, "Export")
                .on_hover_text(self.hover_for(Action::ToggleExport, ""))
                .clicked()
            {
                self.toggle_export();
            }
            if ui.selectable_label(self.show_viewcmd, "View cmd").clicked() {
                self.show_viewcmd = !self.show_viewcmd;
            }
            if ui
                .selectable_label(self.show_settings, "Settings")
                .on_hover_text(self.hover_for(Action::ToggleSettings, ""))
                .clicked()
            {
                self.show_settings = !self.show_settings;
            }
            // Pipeline profiler — only offered when launched with CIM_DEBUG=1.
            if crate::debug::enabled()
                && ui
                    .selectable_label(self.show_debug, "Debug")
                    .on_hover_text("Per-stage timing (read → display) to spot bottlenecks")
                    .clicked()
            {
                self.show_debug = !self.show_debug;
            }

            // Transient notifications (e.g. "Settings saved") sit on the far
            // right of this same row, at normal size; they auto-clear after
            // `STATUS_TTL` (see `update`).
            if !self.status.is_empty() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(self.status.text());
                });
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
    }

    pub(super) fn ab_picker(&mut self, ui: &mut egui::Ui, is_a: bool) {
        let n = self.panes.len();
        let cur = (if is_a { self.slot_a } else { self.slot_b }).min(n - 1);
        ui.label(if is_a { "A:" } else { "B:" });
        // A dropdown listing every open media (1-based index · name).
        let cur_text = format!("{}·{}", cur + 1, self.panes[cur].media.name());
        let mut chosen = cur;
        egui::ComboBox::from_id_salt(if is_a { "ab_pick_a" } else { "ab_pick_b" })
            .selected_text(cur_text)
            .show_ui(ui, |ui| {
                for i in 0..n {
                    let label = format!("{}·{}", i + 1, self.panes[i].media.name());
                    ui.selectable_value(&mut chosen, i, label);
                }
            });
        if is_a {
            self.slot_a = chosen;
        } else {
            self.slot_b = chosen;
        }
    }

    // ---- full-width frame bar -------------------------------------------

    /// The bottom transport strip: play / step controls, the selected media's
    /// name and frame counter, and a full-width scrubber. Shown only while the
    /// focused media is a sequence; its length tracks the selected media.
    pub(super) fn draw_frame_bar(&mut self, ui: &mut egui::Ui) {
        let len = self.timeline_len();
        let at_end = self.current_at_end();
        let cur = self.loop_control();
        let name = self
            .panes
            .get(cur)
            .map(|p| p.media.name().to_string())
            .unwrap_or_default();

        ui.horizontal(|ui| {
            // --- transport group ---
            let play = if self.playback.playing { "Pause" } else { "Play" };
            if ui
                .button(play)
                .on_hover_text(self.hover_for(Action::PlayPause, ""))
                .clicked()
            {
                self.playback.playing = !self.playback.playing;
            }
            // Step through `apply_action` so these obey the active loop window
            // exactly like the keyboard / Ctrl+wheel controls.
            if ui
                .button("Prev")
                .on_hover_text(self.hover_for(Action::PrevFrame, "Previous frame"))
                .clicked()
            {
                self.apply_action(Action::PrevFrame, ui.ctx());
            }
            if ui
                .button("Next")
                .on_hover_text(self.hover_for(Action::NextFrame, "Next frame"))
                .clicked()
            {
                self.apply_action(Action::NextFrame, ui.ctx());
            }
            ui.separator();

            // --- loop group: enable, reset-to-full, and start/end fields ---
            if ui
                .selectable_label(self.playback.loop_playback, "Loop")
                .on_hover_text("Loop playback when it reaches the window end")
                .clicked()
            {
                self.playback.loop_playback = !self.playback.loop_playback;
            }
            if ui
                .add_enabled(self.playback.loop_range.is_some(), egui::Button::new("Full"))
                .on_hover_text("Reset the loop range to the whole sequence")
                .clicked()
            {
                self.playback.loop_range = None;
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
                self.playback.loop_range = Some((s.min(hi), hi));
            }
            if e_resp.changed() {
                self.playback.loop_range = Some((lo, e.clamp(lo, last)));
            }
            ui.separator();

            // --- rate / load group ---
            ui.add(
                egui::Slider::new(&mut self.playback.fps, 1.0..=60.0)
                    .suffix(" fps")
                    .fixed_decimals(0),
            );
            // While a bulk load runs, offer Stop; otherwise Load all / Load offsets.
            if self.decoding_all {
                if ui
                    .button("Stop")
                    .on_hover_text("Stop the running Load all / Load offsets")
                    .clicked()
                {
                    self.stop_load();
                }
            } else {
                if ui
                    .button("Load all")
                    .on_hover_text(self.hover_for(
                        Action::LoadAll,
                        "Decode every frame (up to the frame-cache budget; the rest \
                         continue as offsets/headers only)",
                    ))
                    .clicked()
                {
                    self.load_all();
                }
                // A single offset-discovery button. Fast offset discovery
                // availability for the timeline-driving media (cached per pane; a
                // few header reads, re-measured on reload) picks both the label
                // and the action: when it's available the button reads "Load
                // offsets (fast)" and runs the instant binary-search path;
                // otherwise it reads "Load offset" and rides the ordinary
                // header-probe discovery (its reason rides the hover).
                let fast_avail = self
                    .panes
                    .get_mut(cur)
                    .map(|p| {
                        p.fast_jump
                            .get_or_insert_with(|| media::fast_jump_availability(&p.media))
                            .clone()
                    })
                    .unwrap_or_else(|| Err(String::new()));
                if fast_avail.is_ok() {
                    if ui
                        .button("Load offsets (fast)")
                        .on_hover_text(
                            "Discover the whole length at once by binary-searching each \
                             file's page count (pages are uniform and uncompressed, so a \
                             page's position is predictable) — then any index is instantly \
                             seekable in the readout",
                        )
                        .clicked()
                    {
                        self.load_offsets_fast();
                    }
                } else {
                    let reason = fast_avail.as_ref().err().map_or("", String::as_str);
                    let offsets_hover = if reason.is_empty() {
                        "Discover the full sequence length via headers only (no pixel \
                         decode, no cache pressure)"
                            .to_string()
                    } else {
                        format!(
                            "Discover the full sequence length via headers only (no pixel \
                             decode, no cache pressure).\n\nFast offset discovery isn't \
                             available here: {reason}"
                        )
                    };
                    if ui.button("Load offset").on_hover_text(offsets_hover).clicked() {
                        self.load_offsets();
                    }
                }
            }
            // Fast-forward stride: decode 1 of every N frames, skim the N-1 in
            // between by header only. Affects Load all and playback. 1 = every frame.
            ui.label("FF");
            let mut ff = self.playback.fast_forward.max(1);
            if ui
                .add(egui::DragValue::new(&mut ff).range(1..=1_000_000).speed(0.1))
                .on_hover_text(
                    "Fast-forward stride",
                )
                .changed()
            {
                self.playback.fast_forward = ff.max(1);
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
                ui.monospace(format!("/ {count}"));
                // Typeable current index: jump straight to any frame. A target
                // past the discovered end rides the frontier (spinner, no
                // intermediate frames drawn) via `pending_seek`/`drive_seek`.
                let field = egui::TextEdit::singleline(&mut self.frame_edit)
                    .desired_width(52.0)
                    .horizontal_align(egui::Align::Max)
                    .font(egui::TextStyle::Monospace);
                let resp = ui.add(field);
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    // Commit the typed target on Enter (focus is already lost here,
                    // so this must be checked before the not-focused sync below).
                    // Try a fast jump first (instant on a regular layout); it
                    // falls back to riding the frontier when that isn't possible.
                    if let Ok(target) = self.frame_edit.trim().parse::<usize>() {
                        self.do_fast_jump(target);
                    }
                    self.frame_edit = self.shared_frame.to_string();
                } else if !resp.has_focus() {
                    // Keep the buffer showing the live frame while not editing.
                    self.frame_edit = self.shared_frame.to_string();
                }
                ui.monospace("frame");
                // While a typed seek is riding the frontier (target past the
                // discovered end), offer a Stop to abandon the look-ahead so the
                // timeline stays where it is instead of chasing the frontier.
                if self.pending_seek.is_some()
                    && ui
                        .button("Stop")
                        .on_hover_text("Stop seeking to the typed frame")
                        .clicked()
                {
                    self.pending_seek = None;
                }
            });
        });

        ui.add_space(3.0);

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
            .get(self.loop_control())
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
                    Stroke::new(1.0_f32, Color32::from_gray(90)),
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
        let st = Stroke::new(2.0_f32, amber);
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
            Stroke::new(2.0_f32, Color32::from_gray(235)),
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
            self.playback.loop_drag = resp.interact_pointer_pos().and_then(|p| {
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
                match self.playback.loop_drag {
                    Some(true) => self.playback.loop_range = Some((f.min(hi), hi)),
                    Some(false) => self.playback.loop_range = Some((lo, f.max(lo).min(len - 1))),
                    None => {
                        self.pending_seek = None;
                        self.playback.prefetch = None;
                        self.shared_frame = f;
                    }
                }
            }
        }
        if resp.drag_stopped() {
            self.playback.loop_drag = None;
        }
        if resp.clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                self.pending_seek = None;
                self.playback.prefetch = None;
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
            .default_pos(ctx.screen_rect().center())
            .pivot(egui::Align2::CENTER_CENTER)
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
                if ui
                    .button("Copy to clipboard")
                    .on_hover_text("Or press Ctrl+Shift+C anywhere")
                    .clicked()
                {
                    self.copy_view_command(ui.ctx());
                }
            });
        self.show_viewcmd = open;
    }

    /// The `CIM_DEBUG` profiler window: one row per pipeline stage (read →
    /// display) with last / average / min / max times over the recent window, so
    /// the slowest stage stands out. Only reachable when `crate::debug::enabled()`.
    pub(super) fn draw_debug(&mut self, ctx: &egui::Context) {
        use crate::debug::Stage;
        let mut open = self.show_debug;
        egui::Window::new("Debug — pipeline timing")
            .open(&mut open)
            .default_pos(ctx.screen_rect().center())
            .pivot(egui::Align2::CENTER_CENTER)
            .resizable(true)
            .default_width(440.0)
            .show(ctx, |ui| {
                ui.label(
                    "Time each frame spends per stage on its way to the screen.",
                );
                ui.add_space(6.0);

                let ms = |v: Option<f64>| v.map(|v| format!("{v:.2}")).unwrap_or_else(|| "—".into());
                let row = |ui: &mut egui::Ui, name: &str, s: &Stage| {
                    ui.label(name);
                    ui.monospace(ms(s.last()));
                    ui.monospace(ms(s.avg()));
                    ui.monospace(ms(s.min()));
                    ui.monospace(ms(s.max()));
                    ui.monospace(format!("{}", s.count()));
                    ui.end_row();
                };

                egui::Grid::new("debug_timings")
                    .num_columns(6)
                    .striped(true)
                    .spacing([14.0, 6.0])
                    .show(ui, |ui| {
                        ui.strong("stage");
                        ui.strong("last");
                        ui.strong("avg");
                        ui.strong("min");
                        ui.strong("max");
                        ui.strong("n");
                        ui.end_row();

                        let m = &self.metrics;
                        row(ui, "Read", &m.read);
                        row(ui, "Decode", &m.decode);
                        row(ui, "Render", &m.lut);
                        row(ui, "Custom operators", &m.operators);
                        row(ui, "Texture upload", &m.upload);
                        row(ui, "Global update", &m.frame);
                    });
            });
        self.show_debug = open;
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
        let shared_rotation = self.shared_rotation;
        let shared_overlay = self.shared_overlay;

        // Row drag-to-reorder: rows collects each media row's (vec index, screen
        // y-band) so a drop can be mapped to a target; do_move carries the
        // resolved (from, to) out to be applied after the window closes.
        let mut rows: Vec<(usize, egui::Rangef)> = Vec::new();
        let mut do_move: Option<(usize, usize)> = None;

        egui::Window::new("☰ Media")
            .open(&mut open)
            .default_pos(ctx.screen_rect().center())
            .pivot(egui::Align2::CENTER_CENTER)
            // Width stays user-resizable, but let the height auto-size so the
            // window grows to include every media row (the inner ScrollArea only
            // kicks in once the list is taller than the screen).
            .resizable([true, false])
            .default_width(560.0)
            .show(ctx, |ui| {
                if self.panes.is_empty() {
                    ui.label("No media open. Use Open or drop files onto the window.");
                    return;
                }

                // Cap at ~the screen height so an auto-sizing window doesn't grow
                // off-screen; below that the ScrollArea shrinks to content so every
                // row shows without scrolling.
                let max_h = ctx.screen_rect().height() * 0.85;
                egui::ScrollArea::vertical().max_height(max_h).show(ui, |ui| {
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
                            ui.label("Synchronize")
                                .on_hover_text(
                                    "Pos / Time / Visualization (tone·details·overlay) / Geometry \
                                     (rotation) sync, and the timeline Control",
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
                                    // Visualization sync: share tone / details /
                                    // overlay across all panes.
                                    let mut all_ts = self.panes.iter().all(|p| p.sync_tone);
                                    if ui
                                        .checkbox(&mut all_ts, "Visu")
                                        .on_hover_text("Synchronize the Visualization Transformations (tone·details·overlay) across all panes")
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
                                                p.overlay = shared_overlay;
                                            }
                                            p.sync_tone = all_ts;
                                            p.overlay_tex = None; // tone re-renders via tone_sig
                                        }
                                    }
                                    // Geometry sync: share rotation across all panes.
                                    let mut all_geom = self.panes.iter().all(|p| p.sync_geometry);
                                    if ui
                                        .checkbox(&mut all_geom, "Geom")
                                        .on_hover_text("Synchronize the Geometry Transformations (rotation) across all panes")
                                        .changed()
                                    {
                                        for p in &mut self.panes {
                                            if p.sync_geometry == all_geom {
                                                continue;
                                            }
                                            if !all_geom {
                                                p.rotation = shared_rotation;
                                            }
                                            p.sync_geometry = all_geom;
                                        }
                                    }
                                });
                                if ui
                                    .small_button("⟳")
                                    .on_hover_text(self.hover_for(Action::ReloadAll, "Reload all from disk"))
                                    .clicked()
                                {
                                    self.deferred.push(Deferred::ReloadAll);
                                }
                                ui.end_row();
                            }

                            let mut to_remove = None;
                            let mut to_reload = None;
                            for i in 0..self.panes.len() {
                                let count = self.panes[i].media.frame_count();
                                let resident = self.panes[i].media.resident_count();

                                if ui.checkbox(&mut self.panes[i].visible, "").changed()
                                    && !self.panes[i].visible
                                {
                                    self.reselect_if_hidden();
                                }

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
                                ui.label(name);

                                if count > 1 {
                                    ui.monospace(format!("{count}  ({resident}◈)"));
                                } else {
                                    ui.monospace("still");
                                }

                                if ui
                                    .selectable_label(self.current == i, "▢")
                                    .on_hover_text(
                                        self.hover_for(Action::SelectMedia(i), "Show alone in Single view"),
                                    )
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
                                    // Visualization sync: this pane follows the
                                    // shared tone / details / overlay. Toggling off
                                    // keeps its look.
                                    let mut ts = self.panes[i].sync_tone;
                                    if ui
                                        .checkbox(&mut ts, "Visu")
                                        .on_hover_text(
                                            "Synchronize the Visualization Transformations (tone·details·overlay)",
                                        )
                                        .changed()
                                    {
                                        self.set_sync_tone(i, ts);
                                    }
                                    // Geometry sync: this pane follows the shared
                                    // rotation. Toggling off keeps its angle.
                                    let mut gs = self.panes[i].sync_geometry;
                                    if ui
                                        .checkbox(&mut gs, "Geom")
                                        .on_hover_text("Synchronize the Geometry Transformations (rotation)")
                                        .changed()
                                    {
                                        self.set_sync_geometry(i, gs);
                                    }
                                    // The Control pane is the shared clip-bounds
                                    // source (any media) and, when it's a sequence,
                                    // also drives the timeline / loop.
                                    if ui
                                        .selectable_label(self.control == i, "Control")
                                        .on_hover_text(
                                            "This media is the shared clip source; a sequence also drives the timeline & playback",
                                        )
                                        .clicked()
                                        && self.control != i
                                    {
                                        let old_loop = self.loop_control();
                                        self.control = i;
                                        // A loop sub-range belongs to a specific
                                        // sequence; drop it only if the loop-driving
                                        // sequence actually changed (picking a still
                                        // as Control leaves the loop untouched).
                                        if self.loop_control() != old_loop {
                                            self.playback.loop_range = None;
                                        }
                                    }
                                });

                                ui.horizontal(|ui| {
                                    if ui
                                        .small_button("Reload")
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
                                self.deferred.push(Deferred::Remove(i));
                            }
                            if let Some(i) = to_reload {
                                self.deferred.push(Deferred::Reload(i));
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
                                    Stroke::new(2.0_f32, Color32::from_rgb(240, 200, 80)),
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

        let Some(hist) = &self.panes[idx].hist else {
            painter.rect_filled(rect, 0.0, Color32::from_gray(16));
            return;
        };
        let data = &hist.data;
        let peak_val = draw_hist_curves(&painter, rect, data);

        // True value extent under the graph: min at left, max at right, and the
        // peak (mode) value centred between them in the tick's amber (mono only).
        // Whole numbers (integer sources) print plainly; floats get 4 digits.
        let fmt = |v: f32| -> String {
            if v.fract() == 0.0 {
                format!("{}", v as i64)
            } else {
                format!("{v:.4}")
            }
        };
        // Bound this row to one text line: `ui.columns` passes the full available
        // height down to its children, and the centered layouts below would grab it
        // — in a `scroll(false)` window (unbounded height) that inflates the content
        // and pins the window to its maximum height. `allocate_ui` caps it.
        let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
        ui.allocate_ui(Vec2::new(ui.available_width(), row_h), |ui| {
            ui.columns(3, |cols| {
                cols[0].monospace(format!("min {}", fmt(data.min)));
                if let Some(pv) = peak_val {
                    cols[1].vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new(format!("peak {}", fmt(pv)))
                                .monospace()
                                .color(Color32::from_rgb(240, 200, 80)),
                        );
                    });
                }
                cols[2].with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.monospace(format!("max {}", fmt(data.max)));
                });
            });
        });
    }

    pub(super) fn draw_settings(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        egui::Window::new("⚙ Settings")
            .open(&mut open)
            .default_pos(ctx.screen_rect().center())
            .pivot(egui::Align2::CENTER_CENTER)
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
                ui.horizontal(|ui| {
                    ui.label("Decode threads");
                    ui.add(
                        egui::Slider::new(&mut self.config.decode_threads, 0..=16).custom_formatter(
                            |n, _| {
                                if n == 0.0 {
                                    "auto".to_owned()
                                } else {
                                    format!("{n}")
                                }
                            },
                        ),
                    )
                    .on_hover_text(
                        "Background image-decoding worker threads shared by all sequences. \
                         0 = auto (scales with CPU cores, capped). Lower it to leave CPU \
                         for other users when several instances share one server / VNC host. \
                         Applies immediately.",
                    );
                });
                ui.checkbox(&mut self.config.cursor_dot, "Cursor dot on other panes")
                    .on_hover_text(
                        "Mark the hovered pixel on every other pane with a red dot, so \
                         the same location is easy to compare across panes.",
                    );
                ui.add_space(8.0);
                ui.separator();
                ui.heading("Image operators (C++)");
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Library folder")
                        .on_hover_text(
                            "Folder holding the proprietary operator libraries \
                             (LUT_ALPHA / Details). Leave empty to use the LIBS \
                             folder next to the cim executable. Libraries load \
                             automatically when this folder changes.",
                        );
                    if ui.button("📂 Browse…").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            self.config.cpp_lib_dir = dir.to_string_lossy().into_owned();
                        }
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut self.config.cpp_lib_dir)
                            .hint_text("(uses <cim>/LIBS)")
                            .desired_width(f32::INFINITY),
                    );
                });
                // Live found/not-found indicator for the two libraries: green ✔
                // both present, orange ✔ only one, red ✖ none. A pure filesystem
                // check on the configured folder — it doesn't load anything.
                let dir = super::cpp_lib_dir(&self.config);
                let (lut_ok, details_ok) = crate::imageproc::libs_present(dir.as_deref());
                ui.horizontal(|ui| {
                    let (icon, color, msg) = match (lut_ok, details_ok) {
                        (true, true) => (
                            "✔",
                            Color32::from_rgb(120, 210, 120),
                            "Both operator libraries found".to_owned(),
                        ),
                        (false, false) => (
                            "✖",
                            Color32::from_rgb(230, 120, 120),
                            "No operator libraries found in this folder".to_owned(),
                        ),
                        _ => (
                            "✔",
                            Color32::from_rgb(240, 180, 90),
                            format!(
                                "Only the {} library found",
                                if lut_ok { "LUT_ALPHA" } else { "Details" }
                            ),
                        ),
                    };
                    ui.colored_label(color, egui::RichText::new(icon).strong());
                    ui.colored_label(color, msg);
                });

                // What's actually loaded right now. Libraries auto-load when the
                // folder changes (see the `update` loop / `CimApp::load_cpp_libs`),
                // so this reflects the effect of the path above without any button.
                let lut_loaded = crate::imageproc::lut_alpha_available();
                let details_loaded = crate::imageproc::details_available();
                let loaded = match (lut_loaded, details_loaded) {
                    (true, true) => "loaded: LUT_ALPHA, Details".to_owned(),
                    (true, false) => "loaded: LUT_ALPHA".to_owned(),
                    (false, true) => "loaded: Details".to_owned(),
                    (false, false) => "loaded: none".to_owned(),
                };
                ui.label(egui::RichText::new(loaded).weak());

                ui.add_space(8.0);
                ui.separator();
                ui.heading("Keyboard shortcuts");
                ui.label(
                    egui::RichText::new(
                        "Rebind, then press a key — hold Ctrl / Shift / Alt for a chord \
                         (e.g. Ctrl+R). Esc cancels.",
                    )
                    .weak()
                    .small(),
                );
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
                                    .chord_for(action)
                                    .map(|c| c.name())
                                    .unwrap_or_else(|| "—".into());
                                if self.rebinding == Some(action) {
                                    ui.colored_label(
                                        Color32::from_rgb(240, 200, 120),
                                        "press a key or chord…",
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
                                    }
                                });
                                ui.end_row();
                            }
                        });
                });

                ui.add_space(8.0);
                ui.separator();
                // Settings are written only here — never on exit — so warn while
                // the live config differs from what's on disk.
                let dirty = self.config != self.saved_config;
                ui.horizontal(|ui| {
                    if ui.button("Save settings").clicked() {
                        self.config.save();
                        self.saved_config = self.config.clone();
                        self.status.set("Settings saved");
                    }
                    if dirty {
                        ui.label(
                            egui::RichText::new("⚠ Unsaved changes — not written until you save")
                                .color(Color32::from_rgb(240, 200, 120)),
                        );
                    }
                });
            });
        self.show_settings = open;
    }
}
