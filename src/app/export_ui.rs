//! The export panel: building an `ExportPlan` from live app state, driving a
//! run one frame per update, and the export window UI. The composition and
//! ffmpeg encoding live in `crate::export`.

use super::*;

/// An in-progress export: encoder + snapshotted plan + progress.
pub(super) struct ExportRun {
    enc: Encoder,
    plan: ExportPlan,
    frame: usize,
    total: usize,
    path: String,
}

impl CimApp {
    pub(super) fn toggle_export(&mut self) {
        self.show_export = !self.show_export;
        if self.show_export {
            self.export_mode = self.mode; // default to what's on screen
        } else if self.selecting_region {
            // Panel closed mid-selection: abandon it and restore the view.
            self.selecting_region = false;
            self.sel_start = None;
            self.sel_rect = None;
            if let Some(m) = self.pre_select_mode.take() {
                self.mode = m;
            }
        }
    }

    /// The export source for pane `idx` (how its frames are decoded at export).
    fn export_source(&self, idx: usize) -> ExportSource {
        let p = &self.panes[idx];
        if let Some((files, map)) = p.media.concat_layout() {
            // Concatenation of multi-page TIFFs: hand export the files + the
            // discovered global→(file,page) map so it composites the same
            // continuous timeline (Load-all first to export it in full).
            ExportSource::Concat { files, map }
        } else {
            match p.media.decode_job(0) {
                Some(media::DecodeReq::Tiff { path, .. }) => ExportSource::Seq { path },
                // A numbered still sequence: hand export its frame file list.
                Some(media::DecodeReq::File(_)) => match &p.source {
                    Source::Sequence { files, .. } => ExportSource::Files {
                        paths: files.clone(),
                    },
                    _ => ExportSource::Still(
                        p.media.resident(0).expect("sequence frame 0 resident"),
                    ),
                },
                None => ExportSource::Still(
                    p.media.resident(0).expect("still frame always resident"),
                ),
            }
        }
    }

    /// Snapshot a participating pane for the export plan, including its mask
    /// overlay (sourced from the referenced mask pane) so the export matches
    /// what's on screen.
    pub(super) fn export_pane(&self, idx: usize) -> ExportPane {
        let p = &self.panes[idx];
        let mut pane = ExportPane::new(
            *self.view_ref(idx),
            self.contrast_of(idx),
            self.details_of(idx),
            p.media.frame_count(),
            p.sync_temporal,
            p.frame,
            self.export_source(idx),
        );
        // Use the effective overlay (shared when the pane is tone-synced), and
        // skip mask panes (they don't take an overlay), matching prepare_overlay.
        if let Some(ov) = self.overlay_of(idx).filter(|_| !p.media.is_mask()) {
            if let Some(m) = self.panes.iter().position(|q| q.id == ov.src_id) {
                let mp = &self.panes[m];
                pane.set_overlay(
                    self.export_source(m),
                    mp.media.frame_count(),
                    mp.sync_temporal,
                    mp.frame,
                    [ov.color.r(), ov.color.g(), ov.color.b()],
                    (ov.opacity.clamp(0.0, 1.0) * 255.0) as u8,
                );
            }
        }
        pane
    }

    /// The composition-space rect the export renders (fixes the output aspect).
    /// With an image-space crop, panes become cells of exactly the crop's pixel
    /// size laid out side by side; without one it's the live screen area.
    pub(super) fn export_canvas(&self) -> Rect {
        match self.export_region {
            Some(reg) => {
                let (w, h) = (reg.width(), reg.height());
                match self.export_mode {
                    Mode::Grid => {
                        let n = self.visible_indices().len().max(1);
                        let cols = self.config.max_columns.max(1).min(n);
                        let rows = n.div_ceil(cols);
                        Rect::from_min_size(
                            Pos2::ZERO,
                            Vec2::new(cols as f32 * w, rows as f32 * h),
                        )
                    }
                    Mode::Single | Mode::Ab => {
                        Rect::from_min_size(Pos2::ZERO, Vec2::new(w, h))
                    }
                }
            }
            None => self.last_area,
        }
    }

    /// Inclusive (start, end) of the exported timeline range, clamped to what's
    /// currently known of the timeline. None = start to finish.
    pub(super) fn export_frames(&self) -> (usize, usize) {
        let tl = self.timeline_len().max(1);
        let (s, e) = self.export_range.unwrap_or((0, tl - 1));
        let s = s.min(tl - 1);
        (s, e.clamp(s, tl - 1))
    }

    pub(super) fn build_export_plan(&self) -> Result<ExportPlan, String> {
        if self.panes.is_empty() {
            return Err("No media to export".into());
        }
        let area = self.last_area;
        if self.export_region.is_none() && area.width() < 2.0 {
            return Err("View not ready yet".into());
        }
        let crop = self.export_region;
        let region = self.export_canvas();
        let (out_w, out_h) = export::out_dims(region, self.out_height);
        // Always nearest for export: source pixels are sampled directly onto the
        // output grid, so blending would soften detail the comparison exists to show.
        let bilinear = false;
        let (start, end) = self.export_frames();
        let total = end - start + 1;

        let mut panes = Vec::new();
        let layout = match self.export_mode {
            Mode::Grid => {
                let vis = self.visible_indices();
                if vis.is_empty() {
                    return Err("No visible media (enable some in ☰ Media)".into());
                }
                let mut v = Vec::new();
                if let Some(reg) = crop {
                    // Side-by-side of just the cropped image region: one cell of
                    // the crop's exact pixel size per pane, nothing outside it.
                    let cols = self.config.max_columns.max(1).min(vis.len());
                    for (k, &idx) in vis.iter().enumerate() {
                        let (r, c) = (k / cols, k % cols);
                        let cell = Rect::from_min_size(
                            Pos2::new(c as f32 * reg.width(), r as f32 * reg.height()),
                            reg.size(),
                        );
                        let mut pane = self.export_pane(idx);
                        pane.view = region_view(reg);
                        panes.push(pane);
                        v.push((k, cell));
                    }
                } else {
                    let cells = self.grid_cells(&vis, area);
                    for (k, &(idx, cell)) in cells.iter().enumerate() {
                        panes.push(self.export_pane(idx));
                        v.push((k, image_area(cell)));
                    }
                }
                ExportLayout::Grid(v)
            }
            Mode::Single => {
                let idx = self.current.min(self.panes.len() - 1);
                let mut pane = self.export_pane(idx);
                let cell = match crop {
                    Some(reg) => {
                        pane.view = region_view(reg);
                        Rect::from_min_size(Pos2::ZERO, reg.size())
                    }
                    None => image_area(area),
                };
                panes.push(pane);
                ExportLayout::Single(0, cell)
            }
            Mode::Ab => {
                let n = self.panes.len();
                let a = self.slot_a.min(n - 1);
                let b = self.slot_b.min(n - 1);
                let mut pa = self.export_pane(a);
                let mut pb = self.export_pane(b);
                let img = match crop {
                    Some(reg) => {
                        pa.view = region_view(reg);
                        pb.view = region_view(reg);
                        Rect::from_min_size(Pos2::ZERO, reg.size())
                    }
                    None => Rect::from_min_max(
                        area.min,
                        Pos2::new(area.max.x, area.max.y - FOOTER_H - 2.0),
                    ),
                };
                panes.push(pa);
                panes.push(pb);
                let split_x = img.min.x + self.ab_split.clamp(0.02, 0.98) * img.width();
                ExportLayout::Ab {
                    a: 0,
                    b: 1,
                    img,
                    split_x,
                }
            }
        };

        Ok(ExportPlan {
            panes,
            layout,
            region,
            out_w,
            out_h,
            start,
            total,
            bilinear,
        })
    }

    pub(super) fn start_export(&mut self) {
        let name = self.export_name.trim();
        if name.is_empty() {
            self.export_status = "Enter an output file name first".into();
            return;
        }
        let name = if Path::new(name).extension().is_some() {
            name.to_string()
        } else {
            format!("{name}.mp4")
        };
        let path = PathBuf::from(&name); // relative -> current working directory
        let plan = match self.build_export_plan() {
            Ok(p) => p,
            Err(e) => {
                self.export_status = e;
                return;
            }
        };
        let (w, h, total) = (plan.out_w, plan.out_h, plan.total);
        match Encoder::start(&path, w, h, self.export_fps, self.crf) {
            Ok(enc) => {
                self.export_status = format!("Exporting {total} frames…");
                self.export_run = Some(ExportRun {
                    enc,
                    plan,
                    frame: 0,
                    total,
                    path: path.display().to_string(),
                });
            }
            Err(e) => self.export_status = e,
        }
    }

    /// Encode one frame per call; driven from `update` while a run is active.
    pub(super) fn export_tick(&mut self) {
        let Some(mut run) = self.export_run.take() else {
            return;
        };
        if self.cancel_export {
            self.cancel_export = false;
            run.enc.kill();
            self.export_status = "Export cancelled".into();
            return; // run dropped
        }
        if run.frame >= run.total {
            self.export_status = match run.enc.finish() {
                Ok(()) => format!("Exported {} frames → {}", run.total, run.path),
                Err(e) => e,
            };
            return; // run dropped
        }
        let buf = run.plan.compose(run.frame);
        match run.enc.write_frame(&buf) {
            Ok(()) => {
                run.frame += 1;
                self.export_run = Some(run);
            }
            Err(e) => {
                run.enc.kill();
                self.export_status = format!("Export failed: {e}");
            }
        }
    }

    pub(super) fn draw_export(&mut self, ctx: &egui::Context) {
        let mut open = self.show_export;
        let running = self.export_run.is_some();
        let region = self.export_canvas();
        let (out_w, out_h) = export::out_dims(region, self.out_height);
        let tl = self.timeline_len().max(1);
        let (start, end) = self.export_frames();
        let total = end - start + 1;

        egui::Window::new("Export comparison")
            .open(&mut open)
            .resizable(true)
            .default_width(360.0)
            .show(ctx, |ui| {
                ui.add_enabled_ui(!running, |ui| {
                    egui::Grid::new("export_grid")
                        .num_columns(2)
                        .spacing([12.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("Layout");
                            egui::ComboBox::from_id_salt("exp_layout")
                                .selected_text(match self.export_mode {
                                    Mode::Grid => "Side by side",
                                    Mode::Single => "Single",
                                    Mode::Ab => "A / B wipe",
                                })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.export_mode, Mode::Grid, "Side by side");
                                    ui.selectable_value(&mut self.export_mode, Mode::Single, "Single");
                                    ui.selectable_value(&mut self.export_mode, Mode::Ab, "A / B wipe");
                                });
                            ui.end_row();

                            ui.label("Region");
                            ui.horizontal(|ui| {
                                if ui
                                    .button("Select…")
                                    .on_hover_text(
                                        "Drag the crop on a single image; it then applies \
                                         to every pane of the comparison",
                                    )
                                    .clicked()
                                {
                                    // Pick the crop on one image: force Single
                                    // view for the drag, restore after.
                                    if self.mode != Mode::Single {
                                        self.pre_select_mode = Some(self.mode);
                                        self.mode = Mode::Single;
                                    }
                                    self.selecting_region = true;
                                }
                                let has = self.export_region.is_some();
                                if ui.add_enabled(has, egui::Button::new("Full view")).clicked() {
                                    self.export_region = None;
                                }
                                match self.export_region {
                                    Some(r) => ui.label(format!(
                                        "{}×{} px",
                                        r.width().round() as u32,
                                        r.height().round() as u32
                                    )),
                                    None => ui.label("full"),
                                };
                            });
                            ui.end_row();

                            ui.label("Frames");
                            ui.horizontal(|ui| {
                                let mut all = self.export_range.is_none();
                                if ui.checkbox(&mut all, "all").changed() {
                                    self.export_range =
                                        if all { None } else { Some((0, tl - 1)) };
                                }
                                if let Some((s, e)) = self.export_range {
                                    // 0-based inclusive (matches the transport bar).
                                    let (mut s0, mut e0) = (s, e);
                                    ui.add(
                                        egui::DragValue::new(&mut s0)
                                            .range(0..=e0)
                                            .prefix("from "),
                                    );
                                    ui.add(
                                        egui::DragValue::new(&mut e0)
                                            .range(s0..=(tl - 1))
                                            .prefix("to "),
                                    );
                                    self.export_range = Some((s0, e0));
                                }
                                // Adopt the current playback loop window.
                                let (llo, lhi) = self.loop_bounds(tl);
                                if ui
                                    .add_enabled(
                                        self.loop_range.is_some(),
                                        egui::Button::new("Use loop range"),
                                    )
                                    .on_hover_text(
                                        "Set the frame range to the playback loop window",
                                    )
                                    .clicked()
                                {
                                    self.export_range = Some((llo, lhi));
                                }
                            });
                            ui.end_row();

                            ui.label("Output height");
                            ui.horizontal(|ui| {
                                ui.add(egui::DragValue::new(&mut self.out_height).range(120..=2160));
                                if ui.button("= view").clicked() {
                                    self.out_height = region.height().round() as u32;
                                }
                                ui.monospace(format!("→ {out_w}×{out_h}"));
                            });
                            ui.end_row();

                            ui.label("Compression");
                            ui.add(
                                egui::Slider::new(&mut self.crf, 0..=51)
                                    .text("CRF")
                                    .custom_formatter(|n, _| format!("{n:.0}")),
                            );
                            ui.end_row();

                            ui.label("FPS");
                            ui.add(egui::DragValue::new(&mut self.export_fps).range(1.0..=60.0));
                            ui.end_row();
                        });
                });

                // Sequence lengths are discovered lazily, so warn when a media's
                // true end isn't known yet — the range above may be short.
                if self.panes.iter().any(|p| !p.media.at_end()) {
                    ui.horizontal(|ui| {
                        ui.colored_label(
                            Color32::from_rgb(240, 200, 120),
                            "⚠ Some media aren't fully loaded — frame counts may be incomplete.",
                        );
                        if ui.button("Load all").clicked() {
                            self.load_all();
                        }
                    });
                }

                ui.label(format!(
                    "{total} frames · {:.1}s",
                    total as f32 / self.export_fps.max(1.0),
                ));

                ui.horizontal(|ui| {
                    ui.label("Save as");
                    ui.add_enabled(
                        !running,
                        egui::TextEdit::singleline(&mut self.export_name).desired_width(180.0),
                    );
                });
                ui.label(
                    egui::RichText::new(format!(
                        "→ {}",
                        ellipsize(
                            &std::env::current_dir()
                                .unwrap_or_default()
                                .display()
                                .to_string(),
                            40
                        )
                    ))
                    .weak()
                    .small(),
                );

                ui.separator();
                if let Some(run) = &self.export_run {
                    ui.add(
                        egui::ProgressBar::new(run.frame as f32 / run.total.max(1) as f32)
                            .text(format!("{}/{}", run.frame, run.total)),
                    );
                    if ui.button("Cancel").clicked() {
                        self.cancel_export = true;
                    }
                } else {
                    let ready = !self.export_name.trim().is_empty();
                    if ui.add_enabled(ready, egui::Button::new("Export MP4")).clicked() {
                        self.start_export();
                    }
                }

                if !self.export_status.is_empty() {
                    ui.label(&self.export_status);
                }
            });
        self.show_export = open;
    }
}
