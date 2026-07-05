//! The export panel: building an `ExportPlan` from live app state, running it on
//! a background thread (compose + ffmpeg encode), and the export window UI. The
//! composition and ffmpeg encoding live in `crate::export`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use super::*;

/// An in-progress export: a worker thread that owns the encoder + snapshotted
/// plan and composites/encodes every frame off the UI thread. The UI polls
/// `progress` for the bar and flips `cancel` to stop it; `handle` yields the
/// final outcome once the thread ends.
pub(super) struct ExportRun {
    handle: Option<thread::JoinHandle<ExportOutcome>>,
    progress: Arc<AtomicUsize>, // frames written so far
    cancel: Arc<AtomicBool>,
    total: usize,
    path: String,
}

/// What the current output name exports to (chosen by its file extension).
#[derive(PartialEq)]
enum ExportFormat {
    Video, // .mp4 (or a bare name) → ffmpeg H.264
    Image, // .png / .jpg / .jpeg → one composited still
}

/// How a finished export thread ended.
enum ExportOutcome {
    Done(usize), // frames written
    Cancelled,
    Failed(String),
}

/// Worker body: compose + encode every frame, publishing progress and honouring
/// a cancel request between frames. Runs on its own thread.
fn run_export(
    mut enc: Encoder,
    mut plan: ExportPlan,
    total: usize,
    progress: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
) -> ExportOutcome {
    for t in 0..total {
        if cancel.load(Ordering::Relaxed) {
            enc.kill();
            return ExportOutcome::Cancelled;
        }
        let buf = plan.compose(t);
        if let Err(e) = enc.write_frame(&buf) {
            enc.kill();
            return ExportOutcome::Failed(format!("Export failed: {e}"));
        }
        progress.store(t + 1, Ordering::Relaxed);
    }
    match enc.finish() {
        Ok(()) => ExportOutcome::Done(total),
        Err(e) => ExportOutcome::Failed(e),
    }
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
        })
    }

    /// Format an export produces, decided by the output file's extension
    /// (defaulting to MP4 when none is given).
    fn export_format(&self) -> ExportFormat {
        let name = self.export_name.trim();
        match Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref()
        {
            Some("png") | Some("jpg") | Some("jpeg") => ExportFormat::Image,
            _ => ExportFormat::Video,
        }
    }

    pub(super) fn start_export(&mut self) {
        let name = self.export_name.trim();
        if name.is_empty() {
            self.export_status = "Enter an output file name first".into();
            return;
        }
        // Bare names default to MP4; a .png/.jpg/.jpeg name exports a single still.
        let name = if Path::new(name).extension().is_some() {
            name.to_string()
        } else {
            format!("{name}.mp4")
        };
        let path = PathBuf::from(&name); // relative -> current working directory
        if self.export_format() == ExportFormat::Image {
            self.export_still_image(path);
            return;
        }
        let plan = match self.build_export_plan() {
            Ok(p) => p,
            Err(e) => {
                self.export_status = e;
                return;
            }
        };
        let (w, h, total) = (plan.out_w, plan.out_h, plan.total);
        let enc = match Encoder::start(&path, w, h, self.export_fps, self.crf) {
            Ok(enc) => enc,
            Err(e) => {
                self.export_status = e;
                return;
            }
        };
        // Compose + encode on a worker thread so the UI stays responsive; the
        // plan is a self-contained snapshot, so live edits don't affect it.
        let progress = Arc::new(AtomicUsize::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (pc, cc) = (progress.clone(), cancel.clone());
        let handle = thread::spawn(move || run_export(enc, plan, total, pc, cc));
        self.export_status = format!("Exporting {total} frames…");
        self.export_run = Some(ExportRun {
            handle: Some(handle),
            progress,
            cancel,
            total,
            path: path.display().to_string(),
        });
    }

    /// Export a single composited still (PNG/JPEG) — the same layout, region,
    /// tone and overlays as the MP4 path, but one frame (the one on screen) and
    /// no ffmpeg. Fast enough to run inline on the UI thread.
    fn export_still_image(&mut self, path: PathBuf) {
        let mut plan = match self.build_export_plan() {
            Ok(p) => p,
            Err(e) => {
                self.export_status = e;
                return;
            }
        };
        // A still shows exactly the current timeline frame, not a range.
        plan.start = self.shared_frame.min(self.timeline_len().saturating_sub(1));
        plan.total = 1;
        let (w, h) = (plan.out_w, plan.out_h);
        let rgba = plan.compose(0);
        self.export_status = match export::save_image(&path, w, h, &rgba) {
            Ok(()) => format!("Exported image → {}", path.display()),
            Err(e) => format!("Export failed: {e}"),
        };
    }

    /// Poll the export worker from `update`: relay a cancel request and, once the
    /// thread has finished, join it for the outcome and report it. The heavy
    /// compose/encode work runs on the worker, not here.
    pub(super) fn export_tick(&mut self) {
        let Some(run) = self.export_run.as_mut() else {
            return;
        };
        if self.cancel_export {
            self.cancel_export = false;
            run.cancel.store(true, Ordering::Relaxed);
            self.export_status = "Cancelling…".into();
        }
        // Still encoding? leave the run in place and poll again next update.
        if !run.handle.as_ref().is_some_and(|h| h.is_finished()) {
            return;
        }
        let outcome = run
            .handle
            .take()
            .unwrap()
            .join()
            .unwrap_or_else(|_| ExportOutcome::Failed("Export thread panicked".into()));
        let path = std::mem::take(&mut run.path);
        self.export_run = None;
        self.export_status = match outcome {
            ExportOutcome::Done(n) => format!("Exported {n} frames → {path}"),
            ExportOutcome::Cancelled => "Export cancelled".into(),
            ExportOutcome::Failed(e) => e,
        };
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

                let is_image = self.export_format() == ExportFormat::Image;
                if is_image {
                    ui.label("1 still image (the current frame)");
                } else {
                    ui.label(format!(
                        "{total} frames · {:.1}s",
                        total as f32 / self.export_fps.max(1.0),
                    ));
                }

                ui.horizontal(|ui| {
                    ui.label("Save as");
                    ui.add_enabled(
                        !running,
                        egui::TextEdit::singleline(&mut self.export_name).desired_width(180.0),
                    )
                    .on_hover_text(
                        "Extension picks the format: .mp4 (video), or .png / .jpg for a still",
                    );
                });
                ui.label(
                    egui::RichText::new(format!(
                        "{}",
                            &std::env::current_dir()
                                .unwrap_or_default()
                                .display()
                                .to_string(),
                    ))
                    .weak()
                    .small(),
                );

                ui.separator();
                if let Some(run) = &self.export_run {
                    let done = run.progress.load(Ordering::Relaxed);
                    ui.add(
                        egui::ProgressBar::new(done as f32 / run.total.max(1) as f32)
                            .text(format!("{}/{}", done, run.total)),
                    );
                    if ui.button("Cancel").clicked() {
                        self.cancel_export = true;
                    }
                } else {
                    let ready = !self.export_name.trim().is_empty();
                    let label = if self.export_format() == ExportFormat::Image {
                        "Export image"
                    } else {
                        "Export MP4"
                    };
                    if ui.add_enabled(ready, egui::Button::new(label)).clicked() {
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
