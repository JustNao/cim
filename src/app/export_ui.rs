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
///
/// **Pipelined**: composition runs on a second thread, double-buffered against
/// the encode through a bounded channel (capacity 1 → at most two frames in
/// flight), so frame `t+1` composes while ffmpeg encodes `t` — the export runs
/// at the pace of the slower of the two stages instead of their sum. The
/// composer owns the plan; this thread owns the encoder.
fn run_export(
    mut enc: Encoder,
    mut plan: ExportPlan,
    total: usize,
    progress: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
) -> ExportOutcome {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(1);
    let cancel2 = Arc::clone(&cancel);
    let composer = thread::spawn(move || {
        for t in 0..total {
            if cancel2.load(Ordering::Relaxed) {
                return; // cancelled: stop composing
            }
            let buf = plan.compose(t);
            if tx.send(buf).is_err() {
                return; // encoder side bailed (write error / cancel): stop
            }
        }
    });
    let outcome = (|| {
        for t in 0..total {
            let Ok(buf) = rx.recv() else {
                // The composer only stops sending early on a cancel.
                enc.kill();
                return ExportOutcome::Cancelled;
            };
            if cancel.load(Ordering::Relaxed) {
                enc.kill();
                return ExportOutcome::Cancelled;
            }
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
    })();
    // Unblock a composer stuck in `send` on the (bounded) channel before joining
    // it — on an early exit above nothing would ever `recv` again.
    drop(rx);
    let _ = composer.join();
    outcome
}

impl CimApp {
    pub(super) fn toggle_export(&mut self) {
        self.export.show = !self.export.show;
        if self.export.show {
            self.export.mode = self.mode; // default to what's on screen
        } else {
            // Panel closed mid-selection: abandon it and restore the view.
            self.cancel_region_select();
        }
    }

    /// Abandon an in-progress export-region selection, restoring the view mode it
    /// forced to Single. A no-op when not selecting. Must run on **every** way the
    /// export panel closes — the toolbar toggle *and* the window's title-bar ✕ —
    /// or `selecting_region` stays stuck true and keeps suppressing pane
    /// interaction (rotate / reorder / focus) after the panel is gone.
    pub(super) fn cancel_region_select(&mut self) {
        if !self.export.selecting {
            return;
        }
        self.export.selecting = false;
        self.export.sel_start = None;
        self.export.sel_rect = None;
        if let Some(m) = self.export.pre_select_mode.take() {
            self.mode = m;
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
        // Snapshot the clip: on for Linear / Colormap with the toggle set;
        // LUT_ALPHA takes the full range (None).
        let clip = {
            let t = self.tone_of(idx);
            if self.contrast_of(idx) != ContrastMode::LutAlpha && t.clip.enabled {
                Some(t.clip.percent)
            } else {
                None
            }
        };
        let mut pane = ExportPane::new(
            *self.view_ref(idx),
            self.contrast_of(idx),
            self.details_of(idx),
            clip,
            p.media.frame_count(),
            p.sync_temporal,
            p.frame,
            self.export_source(idx),
        );
        // "Share clip" locks the bounds to the Control media's; snapshot them as
        // an explicit window override for any non-LUT_ALPHA tone so the exported
        // frame matches the live view. (Falls back to the pane's own bounds when
        // the Control frame isn't resident.)
        {
            let t = self.tone_of(idx);
            if self.contrast_of(idx) != ContrastMode::LutAlpha && t.share_clip {
                pane.window = self.control_clip_bounds();
            }
            if self.contrast_of(idx) == ContrastMode::Colormap {
                pane.palette = Some(t.palette);
            }
        }
        pane.rotation = self.rotation_of(idx).to_radians();
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

    /// The on-screen rect actually covered by pane `idx`'s image within its view
    /// reference `area_ref` — the image's rect clipped to the visible area, i.e.
    /// content with the surrounding background excluded.
    fn pane_content_in(&self, idx: usize, area_ref: Rect) -> Rect {
        self.view_ref(idx)
            .image_rect(self.disp_size(idx), area_ref)
            .intersect(area_ref)
    }

    /// Pack each visible pane's on-screen **content** flush into a grid (per-column
    /// widths / per-row heights), removing the inter-cell gaps and the background
    /// margins around each panned/zoomed image. Returns the total composition rect
    /// and the cells (each carrying its slot `place`, view reference `area`, and
    /// the `content` sub-rect shown). `None` when nothing is visible.
    fn packed_grid(&self) -> Option<(Rect, Vec<GridCell>)> {
        let vis = self.visible_indices();
        if vis.is_empty() {
            return None;
        }
        let cells = self.grid_cells(&vis, self.last_area);
        let cols = self.config.max_columns.max(1).min(vis.len()).max(1);
        let rows = vis.len().div_ceil(cols);
        // (media index, content sub-rect, view-reference area) per pane, in order.
        let items: Vec<(usize, Rect, Rect)> = cells
            .iter()
            .map(|&(idx, cell)| (idx, self.pane_content_in(idx, cell), cell))
            .collect();

        let mut col_w = vec![0f32; cols];
        let mut row_h = vec![0f32; rows];
        for (k, (_, content, _)) in items.iter().enumerate() {
            col_w[k % cols] = col_w[k % cols].max(content.width());
            row_h[k / cols] = row_h[k / cols].max(content.height());
        }
        let mut col_x = vec![0f32; cols + 1];
        for i in 0..cols {
            col_x[i + 1] = col_x[i] + col_w[i];
        }
        let mut row_y = vec![0f32; rows + 1];
        for i in 0..rows {
            row_y[i + 1] = row_y[i] + row_h[i];
        }
        let region = Rect::from_min_size(Pos2::ZERO, Vec2::new(col_x[cols], row_y[rows]));
        if !region.is_positive() {
            return None;
        }
        let packed = items
            .into_iter()
            .enumerate()
            .map(|(k, (idx, content, area))| GridCell {
                pane: idx, // remapped to the plan-pane index by the caller
                place: Rect::from_min_size(
                    Pos2::new(col_x[k % cols], row_y[k / cols]),
                    content.size(),
                ),
                area,
                content,
            })
            .collect();
        Some((region, packed))
    }

    /// Composition-space region covering only image **content** (no surrounding
    /// background) for the current mode — used when no explicit crop is set, so
    /// panning the image into a corner doesn't export the empty background.
    /// `None` when nothing is on screen (falls back to the full area).
    fn content_region(&self) -> Option<Rect> {
        if self.panes.is_empty() {
            return None;
        }
        let area = self.last_area;
        let n = self.panes.len();
        let r = match self.export.mode {
            Mode::Single => {
                let idx = self.current.min(n - 1);
                self.pane_content_in(idx, area)
            }
            Mode::Ab => {
                let a = self.slot_a.min(n - 1);
                let b = self.slot_b.min(n - 1);
                // A and B share the image area spatially; cover both.
                self.pane_content_in(a, area)
                    .union(self.pane_content_in(b, area))
                    .intersect(area)
            }
            // Grid packs content flush, so its region is the packed total.
            Mode::Grid => return self.packed_grid().map(|(r, _)| r),
        };
        r.is_positive().then_some(r)
    }

    /// The composition-space rect the export renders (fixes the output aspect).
    /// With an image-space crop, panes become cells of exactly the crop's pixel
    /// size laid out side by side; without one it's the image content on screen
    /// (background around a panned/zoomed image is excluded).
    pub(super) fn export_canvas(&self) -> Rect {
        match self.export.region {
            Some(reg) => {
                let (w, h) = (reg.width(), reg.height());
                match self.export.mode {
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
            None => self.content_region().unwrap_or(self.last_area),
        }
    }

    /// The panes an export actually composites, by current mode — so a warning /
    /// check only considers media that end up in the output.
    fn export_participants(&self) -> Vec<usize> {
        if self.panes.is_empty() {
            return Vec::new();
        }
        let n = self.panes.len();
        match self.export.mode {
            Mode::Grid => self.visible_indices(),
            Mode::Single => vec![self.current.min(n - 1)],
            Mode::Ab => vec![self.slot_a.min(n - 1), self.slot_b.min(n - 1)],
        }
    }

    /// Whether the selected export range could still be cut short (or change) by
    /// lazy length discovery — the only case the "not fully loaded" warning is
    /// meaningful. `false` once the chosen range is fully discovered: an explicit
    /// sub-range whose frames every participating sequence has already found needs
    /// no more loading, even if some tail is still undiscovered.
    fn export_range_incomplete(&self) -> bool {
        // "All" over a still-discovering timeline is inherently open-ended: the
        // end grows with discovery, so it's never complete until the true end.
        if self.export.range.is_none() && !self.current_at_end() {
            return true;
        }
        let (_, end) = self.export_frames();
        // Frames are discovered contiguously, so a sequence covers the range once
        // it's at its true end, or has already found a frame at index `end`. A
        // still-discovering pane that hasn't reached `end` yet may still gain
        // frames within the range (changing what it shows there), so warn.
        self.export_participants().iter().any(|&i| {
            let m = &self.panes[i].media;
            m.frame_count() > 1 && !m.at_end() && m.frame_count() <= end
        })
    }

    /// Inclusive (start, end) of the exported timeline range, clamped to what's
    /// currently known of the timeline. None = start to finish.
    pub(super) fn export_frames(&self) -> (usize, usize) {
        let tl = self.timeline_len().max(1);
        let (s, e) = self.export.range.unwrap_or((0, tl - 1));
        let s = s.min(tl - 1);
        (s, e.clamp(s, tl - 1))
    }

    pub(super) fn build_export_plan(&self) -> Result<ExportPlan, String> {
        if self.panes.is_empty() {
            return Err("No media to export".into());
        }
        let area = self.last_area;
        if self.export.region.is_none() && area.width() < 2.0 {
            return Err("View not ready yet".into());
        }
        let crop = self.export.region;
        let region = self.export_canvas();
        let (out_w, out_h) = export::out_dims(region, self.export.out_height);
        let (start, end) = self.export_frames();
        let total = end - start + 1;

        let mut panes = Vec::new();
        let layout = match self.export.mode {
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
                        // Crop already fills the cell 1:1: place = area = content.
                        v.push(GridCell { pane: k, place: cell, area: cell, content: cell });
                    }
                } else {
                    // No crop: pack each pane's on-screen content flush, so the
                    // export has no background around or between the images.
                    let (_, packed) = self
                        .packed_grid()
                        .ok_or("No visible media (enable some in ☰ Media)")?;
                    for (k, mut cell) in packed.into_iter().enumerate() {
                        panes.push(self.export_pane(cell.pane));
                        cell.pane = k; // remap media index → plan-pane index
                        v.push(cell);
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
                    None => area,
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
                    None => area,
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
        let name = self.export.name.trim();
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
        let name = self.export.name.trim();
        if name.is_empty() {
            self.export.status = "Enter an output file name first".into();
            return;
        }
        // Resolve the output format from the extension. A bare name defaults to
        // MP4; a recognised extension is kept; anything else (e.g. a stray "." in
        // the name like "clip.v2") is rejected rather than handed to ffmpeg with
        // an unusable output name.
        let name = match Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
        {
            None => format!("{name}.mp4"),
            Some(ext) if matches!(ext.as_str(), "mp4" | "png" | "jpg" | "jpeg") => name.to_string(),
            Some(ext) => {
                self.export.status = format!(
                    "Unsupported extension '.{ext}' — use .mp4, .png or .jpg \
                     (or no extension for MP4)"
                );
                return;
            }
        };
        // Resolve to an absolute path against the current working directory so
        // the file lands somewhere predictable (when cim is launched from a
        // desktop/app launcher the CWD may be `/` or $HOME, not where the user
        // expects) and the status message shows the full destination. An
        // absolute name the user typed is kept unchanged.
        let mut path = PathBuf::from(&name);
        if path.is_relative() {
            if let Ok(cwd) = std::env::current_dir() {
                path = cwd.join(&path);
            }
        }
        if self.export_format() == ExportFormat::Image {
            self.export_still_image(path);
            return;
        }
        let plan = match self.build_export_plan() {
            Ok(p) => p,
            Err(e) => {
                self.export.status = e;
                return;
            }
        };
        let (w, h, total) = (plan.out_w, plan.out_h, plan.total);
        let enc = match Encoder::start(&path, w, h, self.export.fps, self.export.crf) {
            Ok(enc) => enc,
            Err(e) => {
                self.export.status = e;
                return;
            }
        };
        // Compose + encode on a worker thread so the UI stays responsive; the
        // plan is a self-contained snapshot, so live edits don't affect it.
        let progress = Arc::new(AtomicUsize::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (pc, cc) = (progress.clone(), cancel.clone());
        let handle = thread::spawn(move || run_export(enc, plan, total, pc, cc));
        self.export.status = format!("Exporting {total} frames…");
        self.export.run = Some(ExportRun {
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
                self.export.status = e;
                return;
            }
        };
        // A still shows exactly the current timeline frame, not a range.
        plan.start = self.shared_frame.min(self.timeline_len().saturating_sub(1));
        plan.total = 1;
        let (w, h) = (plan.out_w, plan.out_h);
        let rgba = plan.compose(0);
        // Cut the background off: crop to the actual image content.
        let Some((cw, ch, cropped)) = export::crop_to_content(&rgba, w, h) else {
            self.export.status = "Nothing to export (all background)".into();
            return;
        };
        self.export.status = match export::save_image(&path, cw, ch, &cropped) {
            Ok(()) => format!("Exported image ({cw}x{ch}) {}", path.display()),
            Err(e) => format!("Export failed: {e}"),
        };
    }

    /// Poll the export worker from `update`: relay a cancel request and, once the
    /// thread has finished, join it for the outcome and report it. The heavy
    /// compose/encode work runs on the worker, not here.
    pub(super) fn export_tick(&mut self) {
        let Some(run) = self.export.run.as_mut() else {
            return;
        };
        if self.export.cancel {
            self.export.cancel = false;
            run.cancel.store(true, Ordering::Relaxed);
            self.export.status = "Cancelling…".into();
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
        self.export.run = None;
        self.export.status = match outcome {
            ExportOutcome::Done(n) => format!("Exported {n} frames at {path}"),
            ExportOutcome::Cancelled => "Export cancelled".into(),
            ExportOutcome::Failed(e) => e,
        };
    }

    pub(super) fn draw_export(&mut self, ctx: &egui::Context) {
        let mut open = self.export.show;
        let running = self.export.run.is_some();
        let region = self.export_canvas();
        let (out_w, out_h) = export::out_dims(region, self.export.out_height);
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
                                .selected_text(match self.export.mode {
                                    Mode::Grid => "Side by side",
                                    Mode::Single => "Single",
                                    Mode::Ab => "A / B wipe",
                                })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.export.mode, Mode::Grid, "Side by side");
                                    ui.selectable_value(&mut self.export.mode, Mode::Single, "Single");
                                    ui.selectable_value(&mut self.export.mode, Mode::Ab, "A / B wipe");
                                });
                            ui.end_row();

                            ui.label("Region");
                            ui.horizontal(|ui| {
                                if ui
                                    .button("Select…")
                                    .on_hover_text(
                                        "Right-drag the crop on a single image (left-drag pans, \
                                         wheel zooms); it then applies to every pane of the \
                                         comparison",
                                    )
                                    .clicked()
                                {
                                    // Pick the crop on one image: force Single
                                    // view for the drag, restore after.
                                    if self.mode != Mode::Single {
                                        self.export.pre_select_mode = Some(self.mode);
                                        self.mode = Mode::Single;
                                    }
                                    self.export.selecting = true;
                                }
                                let has = self.export.region.is_some();
                                if ui.add_enabled(has, egui::Button::new("Full view")).clicked() {
                                    self.export.region = None;
                                }
                                match self.export.region {
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
                                let mut all = self.export.range.is_none();
                                if ui.checkbox(&mut all, "all").changed() {
                                    self.export.range =
                                        if all { None } else { Some((0, tl - 1)) };
                                }
                                if let Some((s, e)) = self.export.range {
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
                                    self.export.range = Some((s0, e0));
                                }
                                // Adopt the current playback loop window, but with
                                // the end **exclusive** — a loop [20, 40] exports
                                // frames 20..40 (20 frames), not through 40.
                                let (llo, lhi) = self.loop_bounds(tl);
                                if ui
                                    .add_enabled(
                                        self.playback.loop_range.is_some(),
                                        egui::Button::new("Use loop range"),
                                    )
                                    .on_hover_text(
                                        "Set the frame range to the playback loop window \
                                         (end exclusive: [20, 40] → frames 20–39)",
                                    )
                                    .clicked()
                                {
                                    self.export.range =
                                        Some((llo, lhi.saturating_sub(1).max(llo)));
                                }
                            });
                            ui.end_row();

                            ui.label("Output height");
                            ui.horizontal(|ui| {
                                ui.add(egui::DragValue::new(&mut self.export.out_height).range(120..=2160));
                                if ui.button("= view").clicked() {
                                    self.export.out_height = region.height().round() as u32;
                                }
                                ui.monospace(format!("→ {out_w}×{out_h}"));
                            });
                            ui.end_row();

                            ui.label("Compression");
                            ui.add(
                                egui::Slider::new(&mut self.export.crf, 0..=51)
                                    .text("CRF")
                                    .custom_formatter(|n, _| format!("{n:.0}")),
                            );
                            ui.end_row();

                            ui.label("FPS");
                            ui.add(egui::DragValue::new(&mut self.export.fps).range(1.0..=60.0));
                            ui.end_row();
                        });
                });

                // Sequence lengths are discovered lazily, so warn when the chosen
                // range isn't fully discovered yet — but not when it already is
                // (e.g. a loop sub-range whose frames are all known).
                if self.export_range_incomplete() {
                    ui.horizontal(|ui| {
                        ui.colored_label(
                            Color32::from_rgb(240, 200, 120),
                            "⚠ Some media aren't fully loaded — frame counts may be incomplete.",
                        );
                        if self.decoding_all {
                            if ui.button("Stop").clicked() {
                                self.stop_load();
                            }
                        } else {
                            if ui
                                .button("Load all")
                                .on_hover_text("Decode every frame; warns if the cache is too small")
                                .clicked()
                            {
                                self.load_all();
                                self.export_load_pending = true; // arm the cache-too-small modal
                            }
                            if ui
                                .button("Load offsets")
                                .on_hover_text(
                                    "Discover the full length via headers only — enough for the \
                                     export range, with no cache pressure",
                                )
                                .clicked()
                            {
                                self.load_offsets();
                            }
                        }
                    });
                }

                let is_image = self.export_format() == ExportFormat::Image;
                if is_image {
                    ui.label("1 still image (the current frame)");
                } else {
                    ui.label(format!(
                        "{total} frames · {:.1}s",
                        total as f32 / self.export.fps.max(1.0),
                    ));
                }

                ui.horizontal(|ui| {
                    ui.label("Save as");
                    ui.add_enabled(
                        !running,
                        egui::TextEdit::singleline(&mut self.export.name).desired_width(180.0),
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
                if let Some(run) = &self.export.run {
                    let done = run.progress.load(Ordering::Relaxed);
                    ui.add(
                        egui::ProgressBar::new(done as f32 / run.total.max(1) as f32)
                            .text(format!("{}/{}", done, run.total)),
                    );
                    if ui.button("Cancel").clicked() {
                        self.export.cancel = true;
                    }
                } else {
                    let ready = !self.export.name.trim().is_empty();
                    let label = if self.export_format() == ExportFormat::Image {
                        "Export image"
                    } else {
                        "Export MP4"
                    };
                    if ui.add_enabled(ready, egui::Button::new(label)).clicked() {
                        self.start_export();
                    }
                }

                if !self.export.status.is_empty() {
                    ui.label(&self.export.status);
                }
            });
        // Closing via the window's ✕ (rather than the toolbar toggle) must still
        // tear down an in-progress region selection, or it stays stuck on.
        if self.export.show && !open {
            self.cancel_region_select();
        }
        self.export.show = open;
    }
}
