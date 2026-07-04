//! The toolbar and the floating tool windows: media manager, visualise
//! (interpolation + histogram), and settings (layout + keybindings).

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
            if ui.selectable_label(self.show_vis, "Visualise").clicked() {
                self.show_vis = !self.show_vis;
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
        let cur = self.current.min(self.panes.len().saturating_sub(1));
        let name = self
            .panes
            .get(cur)
            .map(|p| p.media.name().to_string())
            .unwrap_or_default();

        ui.horizontal(|ui| {
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
                let count = if at_end {
                    format!("{len}")
                } else {
                    format!("{len}+")
                };
                ui.monospace(format!("frame {} / {}", self.shared_frame + 1, count));
            });
        });

        self.draw_scrubber(ui, len, at_end);
    }

    /// A wide, click/drag-seekable frame track filling the panel width. The
    /// filled portion reflects progress; a playhead marks the current frame, and
    /// per-frame ticks appear while the sequence is short enough to read them.
    pub(super) fn draw_scrubber(&mut self, ui: &mut egui::Ui, len: usize, at_end: bool) {
        let width = ui.available_width();
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(width, 26.0), Sense::click_and_drag());
        let painter = ui.painter_at(rect);

        let span = (len.saturating_sub(1)).max(1) as f32;
        let frac = self.shared_frame as f32 / span;

        painter.rect_filled(rect, 4.0, Color32::from_gray(30));
        let filled = Rect::from_min_size(rect.min, Vec2::new(rect.width() * frac, rect.height()));
        painter.rect_filled(filled, 4.0, Color32::from_rgb(56, 104, 162));

        // Per-frame ticks while they stay legible; a "+" marker at the frontier.
        if len <= 80 {
            for k in 0..len {
                let x = rect.left() + rect.width() * (k as f32 / span);
                painter.line_segment(
                    [Pos2::new(x, rect.bottom() - 5.0), Pos2::new(x, rect.bottom())],
                    Stroke::new(1.0, Color32::from_gray(80)),
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

        // Playhead.
        let px = rect.left() + rect.width() * frac;
        painter.line_segment(
            [Pos2::new(px, rect.top()), Pos2::new(px, rect.bottom())],
            Stroke::new(2.0, Color32::from_gray(235)),
        );
        painter.circle_filled(Pos2::new(px, rect.center().y), 6.0, Color32::from_gray(240));

        // Click or drag anywhere on the track to seek.
        if (resp.clicked() || resp.dragged()) && len > 1 {
            if let Some(p) = resp.interact_pointer_pos() {
                self.pending_seek = None; // manual seek cancels an automatic one
                let t = ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
                self.shared_frame = (t * span).round() as usize;
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
                        .num_columns(10)
                        .striped(true)
                        .spacing([10.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("Show");
                            ui.label("#");
                            ui.label("Name");
                            ui.label("Frames");
                            ui.label("Single");
                            ui.label("A / B");
                            ui.label("Sync");
                            ui.label("Tone");
                            ui.label("Detail");
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
                                });
                                // Aggregate tone: show the common mode (or the
                                // first pane's), and apply the pick to all.
                                let mut all_tone = self.panes[0].contrast;
                                egui::ComboBox::from_id_salt("tone_all")
                                    .selected_text(all_tone.label())
                                    .width(100.0)
                                    .show_ui(ui, |ui| {
                                        for m in ContrastMode::ORDER {
                                            if ui
                                                .selectable_value(&mut all_tone, m, m.label())
                                                .clicked()
                                            {
                                                for p in &mut self.panes {
                                                    if p.contrast != all_tone {
                                                        p.contrast = all_tone;
                                                        p.tex = None; // rebuild
                                                    }
                                                }
                                            }
                                        }
                                    });
                                let mut all_det = self.panes.iter().all(|p| p.details);
                                if ui
                                    .checkbox(&mut all_det, "On")
                                    .on_hover_text("DETAILS_ENHANCED for all")
                                    .changed()
                                {
                                    for p in &mut self.panes {
                                        if p.details != all_det {
                                            p.details = all_det;
                                            p.tex = None; // rebuild with new mapping
                                        }
                                    }
                                }
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

                                ui.monospace(format!("{}", i + 1));

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
                                });

                                let mut tone = self.panes[i].contrast;
                                egui::ComboBox::from_id_salt(("tone", i))
                                    .selected_text(tone.label())
                                    .width(100.0)
                                    .show_ui(ui, |ui| {
                                        for m in ContrastMode::ORDER {
                                            ui.selectable_value(&mut tone, m, m.label());
                                        }
                                    });
                                if tone != self.panes[i].contrast {
                                    self.panes[i].contrast = tone;
                                    self.panes[i].tex = None; // rebuild with new mapping
                                }

                                let mut det = self.panes[i].details;
                                if ui
                                    .checkbox(&mut det, "On")
                                    .on_hover_text("DETAILS_ENHANCED for this media")
                                    .changed()
                                {
                                    self.panes[i].details = det;
                                    self.panes[i].tex = None; // rebuild with new mapping
                                }

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
            });
        self.show_manager = open;
    }

    pub(super) fn draw_vis(&mut self, ctx: &egui::Context) {
        self.update_histogram();
        let mut open = self.show_vis;
        let mut changed = false;
        egui::Window::new("Visualise")
            .open(&mut open)
            .resizable(true)
            .default_width(340.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Interpolation");
                    egui::ComboBox::from_id_salt("interp")
                        .selected_text(match self.config.vis.interp {
                            Interpolation::Nearest => "Nearest",
                            Interpolation::Bilinear => "Bilinear",
                        })
                        .show_ui(ui, |ui| {
                            changed |= ui
                                .selectable_value(
                                    &mut self.config.vis.interp,
                                    Interpolation::Nearest,
                                    "Nearest",
                                )
                                .changed();
                            changed |= ui
                                .selectable_value(
                                    &mut self.config.vis.interp,
                                    Interpolation::Bilinear,
                                    "Bilinear",
                                )
                                .changed();
                        });
                });

                ui.add_space(6.0);
                ui.separator();
                ui.heading("Histogram");
                self.draw_histogram(ui);
            });

        if changed {
            // Rebuild textures so filter/clip changes are visible immediately.
            for p in &mut self.panes {
                p.tex = None;
            }
            self.config.save();
        }
        self.show_vis = open;
    }

    /// Recompute the histogram of the focused media/frame when it changes.
    pub(super) fn update_histogram(&mut self) {
        if self.panes.is_empty() {
            self.hist = None;
            return;
        }
        let cur = self.current.min(self.panes.len() - 1);
        let f = self.frame_disp(cur);
        let key = (self.panes[cur].id, f);
        if self.hist.as_ref().map(|h| h.key) == Some(key) {
            return;
        }
        if let Some(frame) = self.panes[cur].media.resident(f) {
            self.hist = Some(HistCache {
                key,
                data: frame.histogram_display(256),
            });
        }
    }

    pub(super) fn draw_histogram(&self, ui: &mut egui::Ui) {
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 140.0), Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_gray(16));

        let Some(hist) = &self.hist else { return };
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
