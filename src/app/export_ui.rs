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

/// Rasterize one label to a coverage bitmap through **egui's own font atlas**, so
/// the burnt-in text matches the UI font exactly.
///
/// The atlas is rasterized at `pixels_per_point`, so asking for a font size of
/// `size_px / ppp` *points* puts glyphs in the atlas at exactly `size_px` pixels
/// — the blit is then 1:1, with no resampling of the glyph coverage. Laying the
/// text out is what adds its glyphs to the atlas, hence the image is fetched
/// after (a second `ctx.fonts` call: the first still holds the lock).
fn rasterize_label(ctx: &egui::Context, text: &str, size_px: f32) -> Option<LabelBitmap> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let ppp = ctx.pixels_per_point().max(0.1);
    let size = size_px.clamp(LabelStyle::MIN_SIZE, LabelStyle::MAX_SIZE) / ppp;
    let galley = ctx.fonts(|f| {
        f.layout_no_wrap(
            text.to_owned(),
            FontId::proportional(size),
            Color32::WHITE, // colour is applied when blending, not here
        )
    });
    let atlas = ctx.fonts(|f| f.image());
    let (aw, ah) = (atlas.size[0], atlas.size[1]);
    let (w, h) = (
        (galley.size().x * ppp).ceil() as usize + 2,
        (galley.size().y * ppp).ceil() as usize + 2,
    );
    if w == 0 || h == 0 {
        return None;
    }
    let mut alpha = vec![0u8; w * h];
    for row in &galley.rows {
        for g in &row.glyphs {
            let uv = g.uv_rect;
            if uv.is_nothing() {
                continue; // whitespace
            }
            let dx = ((g.pos.x + uv.offset.x) * ppp).round() as i64;
            let dy = ((g.pos.y + uv.offset.y) * ppp).round() as i64;
            let (sw, sh) = (
                (uv.max[0] - uv.min[0]) as usize,
                (uv.max[1] - uv.min[1]) as usize,
            );
            for sy in 0..sh {
                let ay = uv.min[1] as usize + sy;
                let ty = dy + sy as i64;
                if ay >= ah || ty < 0 || ty >= h as i64 {
                    continue;
                }
                for sx in 0..sw {
                    let ax = uv.min[0] as usize + sx;
                    let tx = dx + sx as i64;
                    if ax >= aw || tx < 0 || tx >= w as i64 {
                        continue;
                    }
                    let cov = (atlas.pixels[ay * aw + ax].clamp(0.0, 1.0) * 255.0).round() as u8;
                    // Glyph boxes can touch; keep the strongest coverage.
                    let d = &mut alpha[ty as usize * w + tx as usize];
                    *d = (*d).max(cov);
                }
            }
        }
    }
    Some(LabelBitmap { w, h, alpha })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The label rasterizer really pulls glyph coverage out of egui's font
    /// atlas: non-empty ink, and a bitmap about the requested pixel height.
    #[test]
    fn rasterizes_text_from_the_font_atlas() {
        let ctx = egui::Context::default();
        // One frame so the fonts exist (they're built on the first pass).
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        let lb = rasterize_label(&ctx, "Reference", 32.0).expect("no bitmap");
        assert!(lb.w > 0 && lb.h > 0);
        assert_eq!(lb.alpha.len(), lb.w * lb.h);
        let ink = lb.alpha.iter().filter(|&&a| a > 0).count();
        assert!(ink > 0, "rasterized label has no ink");
        // The glyph box is a line height tall — near the requested 32 px.
        assert!(
            (16..=64).contains(&lb.h),
            "unexpected label height {}",
            lb.h
        );
        // Blank text draws nothing at all.
        assert!(rasterize_label(&ctx, "   ", 32.0).is_none());
    }
}

/// Human-readable name of a label position (the 3×3 selector's hover text).
fn anchor_name(a: LabelAnchor) -> &'static str {
    match a {
        LabelAnchor::TopLeft => "Top left",
        LabelAnchor::TopCenter => "Top centre",
        LabelAnchor::TopRight => "Top right",
        LabelAnchor::MidLeft => "Middle left",
        LabelAnchor::Center => "Centre",
        LabelAnchor::MidRight => "Middle right",
        LabelAnchor::BottomLeft => "Bottom left",
        LabelAnchor::BottomCenter => "Bottom centre",
        LabelAnchor::BottomRight => "Bottom right",
    }
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
                Some(media::DecodeReq::Video { path, .. }) => ExportSource::Video { path },
                // A numbered still sequence: hand export its frame file list.
                Some(media::DecodeReq::File(_)) => match &p.source {
                    Source::Sequence { files, .. } => ExportSource::Files {
                        paths: files.clone(),
                    },
                    _ => {
                        ExportSource::Still(p.media.resident(0).expect("sequence frame 0 resident"))
                    }
                },
                None => {
                    ExportSource::Still(p.media.resident(0).expect("still frame always resident"))
                }
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
        // "Share clip" locks the bounds to the Control media's. Rather than
        // freeze a single snapshot, flag the pane so `ExportPlan::compose`
        // recomputes the shared window from the Control's frame every exported
        // frame (matching the live view). The plan attaches the Control source.
        {
            let t = self.tone_of(idx);
            if self.contrast_of(idx) != ContrastMode::LutAlpha && t.share_clip {
                pane.share_clip = true;
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
                        Rect::from_min_size(Pos2::ZERO, Vec2::new(cols as f32 * w, rows as f32 * h))
                    }
                    Mode::Single | Mode::Ab => Rect::from_min_size(Pos2::ZERO, Vec2::new(w, h)),
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

    /// The name burnt in for pane `idx`: the user's custom text, falling back to
    /// the media's own name when unset or blank.
    pub(super) fn label_text(&self, idx: usize) -> String {
        let p = &self.panes[idx];
        match self.export.labels.get(&p.id).map(|s| s.trim()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => p.media.name().to_string(),
        }
    }

    /// The rasterized label for pane `idx`, or `None` when names are off.
    fn export_label(&self, ctx: &egui::Context, idx: usize) -> Option<LabelBitmap> {
        if !self.export.labels_on {
            return None;
        }
        rasterize_label(ctx, &self.label_text(idx), self.export.label_style.size_px)
    }

    pub(super) fn build_export_plan(&self, ctx: &egui::Context) -> Result<ExportPlan, String> {
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
        // Labels are pushed alongside `panes` in every arm below, so the two
        // vectors stay index-aligned by construction.
        let mut labels: Vec<Option<LabelBitmap>> = Vec::new();
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
                        labels.push(self.export_label(ctx, idx));
                        // Crop already fills the cell 1:1: place = area = content.
                        v.push(GridCell {
                            pane: k,
                            place: cell,
                            area: cell,
                            content: cell,
                        });
                    }
                } else {
                    // No crop: pack each pane's on-screen content flush, so the
                    // export has no background around or between the images.
                    let (_, packed) = self
                        .packed_grid()
                        .ok_or("No visible media (enable some in ☰ Media)")?;
                    for (k, mut cell) in packed.into_iter().enumerate() {
                        panes.push(self.export_pane(cell.pane));
                        labels.push(self.export_label(ctx, cell.pane));
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
                labels.push(self.export_label(ctx, idx));
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
                labels.push(self.export_label(ctx, a));
                labels.push(self.export_label(ctx, b));
                let split_x = img.min.x + self.ab_split.clamp(0.02, 0.98) * img.width();
                ExportLayout::Ab {
                    a: 0,
                    b: 1,
                    img,
                    split_x,
                }
            }
        };

        let has_share_clip = panes.iter().any(|p| p.share_clip);
        let mut plan = ExportPlan {
            panes,
            layout,
            control: None,
            region,
            labels,
            label_style: self.export.label_style,
            out_w,
            out_h,
            start,
            total,
        };
        // Attach the Control media so its "Share clip" bounds are recomputed per
        // exported frame (the live view does this every frame via `tone_sig`).
        if has_share_clip {
            if let Some((source, count, sync, own, clip, region)) = self.export_control_snapshot() {
                plan.set_share_clip_control(source, count, sync, own, clip, region);
            }
        }
        Ok(plan)
    }

    /// Snapshot the Control media's tone inputs for the export's per-frame
    /// Share-clip window: its source, temporal sync/frame, and the clip percentile
    /// + region that reproduce `own_tone_bounds`. `None` only when there are no
    /// panes.
    fn export_control_snapshot(
        &self,
    ) -> Option<(ExportSource, usize, bool, usize, Option<f32>, Option<Rect>)> {
        if self.panes.is_empty() {
            return None;
        }
        let c = self.control.min(self.panes.len() - 1);
        let p = &self.panes[c];
        let contrast = self.contrast_of(c);
        let tone = self.tone_of(c);
        // Clip percentile, or None = full range (also for LUT_ALPHA's own contrast).
        let clip = (contrast != ContrastMode::LutAlpha && tone.clip.enabled).then_some(tone.clip.percent);
        // Region the Control's bounds are computed over, mirroring the precedence
        // in `own_tone_bounds`: the export crop while the panel is open, else its
        // stats region when region-tone is on; never for LUT_ALPHA.
        let region = if contrast == ContrastMode::LutAlpha {
            None
        } else if self.export.show && self.export.region.is_some() {
            self.export.region
        } else if p.region_tone {
            self.stats_region
        } else {
            None
        };
        Some((
            self.export_source(c),
            p.media.frame_count(),
            p.sync_temporal,
            p.frame,
            clip,
            region,
        ))
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

    pub(super) fn start_export(&mut self, ctx: &egui::Context) {
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
            self.export_still_image(ctx, path);
            return;
        }
        let plan = match self.build_export_plan(ctx) {
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
    fn export_still_image(&mut self, ctx: &egui::Context, path: PathBuf) {
        let mut plan = match self.build_export_plan(ctx) {
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

    /// The "Add names" section: one text field per exported media, the shared
    /// style controls, and the preview. Drawn under the export grid when the
    /// toggle is on.
    fn draw_label_options(&mut self, ui: &mut egui::Ui) {
        // Only the media that actually end up in the output, in output order.
        let participants = self.export_participants();
        // Snapshot (id, fallback name) so the map can be borrowed mutably below.
        let rows: Vec<(usize, u64, String)> = participants
            .iter()
            .map(|&i| (i, self.panes[i].id, self.panes[i].media.name().to_string()))
            .collect();

        // ui.add_space(4.0);
        for (idx, id, fallback) in &rows {
            ui.horizontal(|ui| {
                let text = self
                    .export
                    .labels
                    .entry(*id)
                    .or_insert_with(|| fallback.clone());
                ui.add(egui::TextEdit::singleline(text).desired_width(200.0));
                ui.label(egui::RichText::new((idx + 1).to_string()).weak().small());
            });
        }
        if rows.is_empty() {
            ui.label(egui::RichText::new("No media in this layout").weak());
        }

        ui.add_space(4.0);
        let st = &mut self.export.label_style;
        ui.horizontal(|ui| {
            ui.label("Text");
            ui.color_edit_button_srgba(&mut st.color);
            ui.add(
                egui::DragValue::new(&mut st.size_px)
                    .range(LabelStyle::MIN_SIZE..=LabelStyle::MAX_SIZE)
                    .suffix(" px"),
            )
            .on_hover_text("Text height in output pixels (independent of zoom)");
            ui.separator();
            ui.checkbox(&mut st.background, "Background");
            ui.add_enabled_ui(st.background, |ui| {
                ui.color_edit_button_srgba(&mut st.bg_color);
            });
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Position");
            // A 3×3 block of squares mirroring where the label sits in
            // each media's cell.
            ui.vertical(|ui| {
                for row in 0..3 {
                    ui.horizontal(|ui| {
                        for col in 0..3 {
                            let a = LabelAnchor::ALL[row * 3 + col];
                            let sel = st.anchor == a;
                            let (rect, resp) =
                                ui.allocate_exact_size(Vec2::new(22.0, 16.0), Sense::click());
                            // The cells carry no text, so each needs a fill of
                            // its own — otherwise the unselected positions are
                            // invisible and the grid can't be read.
                            let fill = if sel {
                                ui.visuals().selection.bg_fill
                            } else if resp.hovered() {
                                Color32::from_gray(110)
                            } else {
                                Color32::from_gray(70)
                            };
                            ui.painter().rect_filled(rect, 0.0, fill);
                            if resp.on_hover_text(anchor_name(a)).clicked() {
                                st.anchor = a;
                            }
                        }
                    });
                }
            });
            ui.add(
                egui::DragValue::new(&mut st.margin)
                    .range(0.0..=200.0)
                    .prefix("Margin ")
                    .suffix(" px"),
            );
        });

        ui.add_space(4.0);
        self.draw_label_preview(ui, &rows);
    }

    /// A small mock of one exported cell: the media's own image with the label
    /// drawn over it, using the same anchor / margin / padding maths as
    /// `ExportPlan::draw_labels`, scaled to the preview. Not a re-run of the
    /// compositor — just enough to see where the text lands and how it reads.
    fn draw_label_preview(&mut self, ui: &mut egui::Ui, rows: &[(usize, u64, String)]) {
        if rows.is_empty() {
            return;
        }
        // Which media to preview (defaults to the first exported one).
        let mut idx = rows
            .iter()
            .find(|(_, id, _)| Some(*id) == self.export.label_preview)
            .map_or(rows[0].0, |(i, _, _)| *i);
        if rows.len() > 1 {
            ui.horizontal(|ui| {
                ui.label("Preview");
                ui.add_space(4.0);
                egui::ComboBox::from_id_salt("exp_label_preview")
                    .selected_text(ellipsize(self.panes[idx].media.name(), 24))
                    .show_ui(ui, |ui| {
                        for (i, _, name) in rows {
                            if ui
                                .selectable_label(*i == idx, ellipsize(name, 30))
                                .clicked()
                            {
                                idx = *i;
                            }
                        }
                    });
            });
            self.export.label_preview = Some(self.panes[idx].id);
        }

        // Preview box, in the exported cell's aspect so the placement is honest.
        // With a crop selected it mirrors the region: the box takes the crop's
        // aspect and only the cropped sub-rect of the texture is shown (UVs), so
        // the preview matches what actually gets exported.
        let region = self.export_canvas();
        let (_, out_h) = export::out_dims(region, self.export.out_height);
        let dsz = self.disp_size(idx);
        let (aspect, uv) = match self.export.region {
            Some(reg) => {
                let (iw, ih) = (dsz[0].max(1) as f32, dsz[1].max(1) as f32);
                let uv = Rect::from_min_max(
                    Pos2::new((reg.min.x / iw).clamp(0.0, 1.0), (reg.min.y / ih).clamp(0.0, 1.0)),
                    Pos2::new((reg.max.x / iw).clamp(0.0, 1.0), (reg.max.y / ih).clamp(0.0, 1.0)),
                );
                (reg.width() / reg.height().max(1.0), uv)
            }
            None => {
                // No crop: the export composites only the on-screen **content**
                // (the image clipped to the visible view — `content_region` /
                // `pane_content_in`), so the preview must show that same sub-rect
                // honouring the live view's zoom/pan, not the whole image.
                let area = self.last_area;
                let vt = self.view_ref(idx);
                let content = vt.image_rect(dsz, area).intersect(area);
                if content.is_positive() {
                    let (iw, ih) = (dsz[0].max(1) as f32, dsz[1].max(1) as f32);
                    let a = vt.screen_to_img(content.min, area);
                    let b = vt.screen_to_img(content.max, area);
                    let uv = Rect::from_min_max(
                        Pos2::new((a.x / iw).clamp(0.0, 1.0), (a.y / ih).clamp(0.0, 1.0)),
                        Pos2::new((b.x / iw).clamp(0.0, 1.0), (b.y / ih).clamp(0.0, 1.0)),
                    );
                    (content.width() / content.height().max(1.0), uv)
                } else {
                    (
                        dsz[0] as f32 / dsz[1].max(1) as f32,
                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    )
                }
            }
        };
        let aspect = aspect.clamp(0.25, 4.0);
        let w = ui.available_width().clamp(80.0, 300.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(w, w / aspect), Sense::hover());
        let painter = ui.painter_at(rect);
        match self.pane_texture(idx) {
            Some(tex) => painter.add(egui::Shape::image(tex, rect, uv, Color32::WHITE)),
            // No committed texture yet (still decoding): a flat plate still shows
            // where the label lands.
            None => painter.rect_filled(rect, 0.0, Color32::from_gray(60)),
        };

        // Same geometry as the export, scaled by the preview's share of the
        // output height, so the label keeps its true relative size.
        let st = self.export.label_style;
        let scale = rect.height() / out_h.max(1) as f32;
        let font = FontId::proportional((st.size_px * scale).max(4.0));
        let text = self.label_text(idx);
        let galley = painter.layout_no_wrap(text, font, st.color);
        let pad = if st.background {
            st.bg_pad() * scale
        } else {
            0.0
        };
        let margin = st.margin * scale;
        let (bw, bh) = (galley.size().x + 2.0 * pad, galley.size().y + 2.0 * pad);
        let (fx, fy) = st.anchor.fractions();
        let free = Vec2::new(
            (rect.width() - bw - 2.0 * margin).max(0.0),
            (rect.height() - bh - 2.0 * margin).max(0.0),
        );
        let box_min = rect.min + Vec2::new(margin + free.x * fx, margin + free.y * fy);
        if st.background {
            painter.rect_filled(
                Rect::from_min_size(box_min, Vec2::new(bw, bh)),
                0.0,
                st.bg_color,
            );
        }
        painter.galley(box_min + Vec2::splat(pad), galley, st.color);
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
            .default_pos(ctx.screen_rect().center())
            .pivot(egui::Align2::CENTER_CENTER)
            .resizable(true)
            .default_width(240.0)
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
                                    ui.selectable_value(
                                        &mut self.export.mode,
                                        Mode::Grid,
                                        "Side by side",
                                    );
                                    ui.selectable_value(
                                        &mut self.export.mode,
                                        Mode::Single,
                                        "Single",
                                    );
                                    ui.selectable_value(
                                        &mut self.export.mode,
                                        Mode::Ab,
                                        "A / B wipe",
                                    );
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
                                if ui
                                    .add_enabled(has, egui::Button::new("Full view"))
                                    .clicked()
                                {
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
                                    self.export.range = if all { None } else { Some((0, tl - 1)) };
                                }
                                if let Some((s, e)) = self.export.range {
                                    // 0-based inclusive (matches the transport bar).
                                    let (mut s0, mut e0) = (s, e);
                                    ui.add(
                                        egui::DragValue::new(&mut s0).range(0..=e0).prefix("from "),
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
                                    self.export.range = Some((llo, lhi.saturating_sub(1).max(llo)));
                                }
                            });
                            ui.end_row();

                            ui.label("Output height");
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::DragValue::new(&mut self.export.out_height)
                                        .range(120..=2160),
                                );
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

                            ui.label("Add labels");
                            ui.checkbox(&mut self.export.labels_on, "").on_hover_text(
                                "Burn a text label into each media's cell of the output",
                            );
                            ui.end_row();
                        });
                    if self.export.labels_on {
                        self.draw_label_options(ui);
                    }
                });

                // Sequence lengths are discovered lazily, so warn when the chosen
                // range isn't fully discovered yet — but not when it already is
                // (e.g. a loop sub-range whose frames are all known).
                if self.export_range_incomplete() {
                    ui.scope(|ui| {
                        // Keep the warning (text + button) from stretching the
                        // window wider than the preview: cap it and let the label
                        // wrap onto as many lines as it needs.
                        ui.set_max_width(300.0);
                        ui.colored_label(
                            Color32::from_rgb(240, 200, 120),
                            "⚠ Some media aren't fully loaded — frame counts may be incomplete.",
                        );
                        if self.decoding_all {
                            if ui.button("Stop").clicked() {
                                self.stop_load();
                            }
                        } else if ui
                            .button("Load frames")
                            .on_hover_text(
                                "Discover the full length via headers only — enough for the \
                                 export range, with no cache pressure",
                            )
                            .clicked()
                        {
                            self.load_offsets();
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
                        self.start_export(ctx);
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
