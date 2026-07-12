//! Comparison export (MP4 video or a single PNG/JPEG still).
//!
//! The app builds a self-contained [`ExportPlan`] (a snapshot of layout, views,
//! tone/detail settings and media sources), then composites each timeline frame on the CPU.
//! For **video** it pipes raw RGBA to the `ffmpeg` CLI (H.264) on a worker thread
//! (`app/export_ui.rs::run_export`) while the UI keeps interacting; for a **still**
//! it composes one frame and writes it with [`save_image`]. Uncovered pixels
//! (gutters / letterboxing) are composited **transparent**, so a still never bakes
//! in the dark background (MP4 ignores alpha, keeping its `BG`).

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use eframe::egui::{Pos2, Rect, Vec2};

use crate::media::{FrameData, SeqReader};
use crate::settings::ContrastMode;
use crate::view::ViewTransform;

const BG: [u8; 4] = [24, 24, 24, 255];

/// Where a pane's frames come from during export.
pub enum ExportSource {
    Still(Arc<FrameData>),
    Seq { path: std::path::PathBuf },
    /// A numbered still sequence: one file per frame.
    Files { paths: Vec<std::path::PathBuf> },
    /// Several multi-page TIFFs concatenated: `map[frame] = (file, page)`.
    Concat {
        files: Vec<std::path::PathBuf>,
        map: Vec<(usize, usize)>,
    },
}

/// One pane, snapshotted for export, plus a small decode/render cache.
pub struct ExportPane {
    pub view: ViewTransform,
    pub contrast: ContrastMode,
    pub details: bool,
    /// Linear-clip state: `Some(percent)` clips that percentile off each tail,
    /// `None` maps the full range. Snapshotted from the pane's tone (LUT_ALPHA
    /// always `None` — it takes the full range and does its own contrast).
    pub clip: Option<f32>,
    /// Display rotation in **radians**, about the image centre (0 = none). Applied
    /// when sampling so an exported pane matches the rotated live view.
    pub rotation: f32,
    pub count: usize,
    pub sync_temporal: bool,
    pub own_frame: usize,
    pub source: ExportSource,

    /// Persistent reader for `Seq`/`Concat` sources, opened on first use, so a
    /// long export doesn't re-walk the IFD chain for every frame.
    reader: Option<SeqReader>,
    /// Which concatenated file `reader` is currently open on (reopened when the
    /// timeline crosses into the next file).
    cur_file: Option<usize>,
    /// The freshly decoded source frame, stashed between the decode phase and the
    /// render phase (the render needs the crop box, computed from every pane's
    /// size after all have decoded). `raw_idx` = which frame it holds.
    raw_frame: Option<Arc<FrameData>>,
    raw_idx: Option<usize>,
    /// The frame index currently *rendered* into `cur_display`, plus the crop box
    /// it was rendered at — so an unchanged (frame, box) skips re-rendering.
    cur_idx: Option<usize>,
    cur_box: Option<[usize; 4]>,
    /// The rendered display RGBA — **only the cropped region** (`cur_render_size`),
    /// whose top-left is `cur_origin` in full-image pixels.
    cur_display: Option<Vec<u8>>,
    cur_origin: [usize; 2],
    cur_render_size: [usize; 2],
    /// Full source-frame size (kept for the rotation centre and overlay mapping,
    /// which work in full-image space regardless of the crop).
    cur_size: [usize; 2],
    /// This pane's proprietary operator instances, reused across the exported
    /// frames (rebuilt only on a size change) — same lifecycle as the live view.
    ops: crate::imageproc::PaneOps,
    /// Optional boolean-mask overlay tinted over this pane (mirrors the live view).
    overlay: Option<ExportOverlay>,
}

/// A boolean-mask overlay snapshotted for export: its own source + decode cache,
/// plus the tint. Sampled in the pane's image space and blended over the base.
struct ExportOverlay {
    source: ExportSource,
    count: usize,
    sync_temporal: bool,
    own_frame: usize,
    color: [u8; 3],
    alpha: u8,
    reader: Option<SeqReader>,
    cur_file: Option<usize>,
    cur_idx: Option<usize>,
    /// Rendered overlay RGBA (true → colour at `alpha`, false → transparent).
    cur_mask: Option<Vec<u8>>,
    cur_size: [usize; 2],
}

impl ExportOverlay {
    fn src_index(&self, t: usize) -> usize {
        let c = self.count.max(1);
        if self.sync_temporal {
            t.min(c - 1)
        } else {
            self.own_frame % c
        }
    }
}

/// Decode frame `idx` from `source`, keeping `reader`/`cur_file` warm for
/// seekable sources. `None` means "keep the previous frame" (open/decode miss).
fn decode_source(
    source: &ExportSource,
    idx: usize,
    reader: &mut Option<SeqReader>,
    cur_file: &mut Option<usize>,
) -> Option<Arc<FrameData>> {
    match source {
        ExportSource::Still(f) => Some(f.clone()),
        ExportSource::Seq { path } => {
            if reader.is_none() {
                *reader = Some(SeqReader::open(path).ok()?);
            }
            match reader.as_mut().unwrap().decode(idx) {
                Ok(Some(f)) => Some(Arc::new(f)),
                _ => None,
            }
        }
        ExportSource::Files { paths } => {
            crate::media::decode_file(paths.get(idx)?).ok().map(Arc::new)
        }
        ExportSource::Concat { files, map } => {
            let &(file, page) = map.get(idx)?;
            if *cur_file != Some(file) {
                *reader = Some(SeqReader::open(files.get(file)?).ok()?);
                *cur_file = Some(file);
            }
            match reader.as_mut().unwrap().decode(page) {
                Ok(Some(f)) => Some(Arc::new(f)),
                _ => None,
            }
        }
    }
}

impl ExportPane {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        view: ViewTransform,
        contrast: ContrastMode,
        details: bool,
        clip: Option<f32>,
        count: usize,
        sync_temporal: bool,
        own_frame: usize,
        source: ExportSource,
    ) -> Self {
        Self {
            view,
            contrast,
            details,
            clip,
            rotation: 0.0,
            count,
            sync_temporal,
            own_frame,
            source,
            reader: None,
            cur_file: None,
            raw_frame: None,
            raw_idx: None,
            cur_idx: None,
            cur_box: None,
            cur_display: None,
            cur_origin: [0, 0],
            cur_render_size: [0, 0],
            cur_size: [0, 0],
            ops: crate::imageproc::PaneOps::default(),
            overlay: None,
        }
    }

    /// Attach a boolean-mask overlay tinted `color` at `alpha`, sourced from the
    /// mask media `source` (with its own timeline sync like a pane).
    #[allow(clippy::too_many_arguments)]
    pub fn set_overlay(
        &mut self,
        source: ExportSource,
        count: usize,
        sync_temporal: bool,
        own_frame: usize,
        color: [u8; 3],
        alpha: u8,
    ) {
        self.overlay = Some(ExportOverlay {
            source,
            count,
            sync_temporal,
            own_frame,
            color,
            alpha,
            reader: None,
            cur_file: None,
            cur_idx: None,
            cur_mask: None,
            cur_size: [0, 0],
        });
    }

    /// Which source frame this pane shows at timeline position `t`.
    fn src_index(&self, t: usize) -> usize {
        let c = self.count.max(1);
        if self.sync_temporal {
            t.min(c - 1) // shorter sequences hold on their last frame
        } else {
            self.own_frame % c
        }
    }

    /// Decode + render the mask overlay for timeline `t` (if any), caching the
    /// tinted RGBA to blend during sampling.
    fn ensure_overlay(&mut self, t: usize) {
        let Some(ov) = &mut self.overlay else {
            return;
        };
        let idx = ov.src_index(t);
        if ov.cur_idx == Some(idx) {
            return;
        }
        if let Some(frame) = decode_source(&ov.source, idx, &mut ov.reader, &mut ov.cur_file) {
            ov.cur_size = frame.size;
            let mut buf = ov.cur_mask.take().unwrap_or_default();
            // Match the live view: a boolean mask tints where true, any other
            // single-channel image tints by normalised intensity.
            if frame.is_mask() {
                frame.render_mask_rgba(ov.color, ov.alpha, &mut buf);
            } else {
                frame.render_intensity_rgba(ov.color, ov.alpha, &mut buf);
            }
            ov.cur_mask = Some(buf);
            ov.cur_idx = Some(idx);
        }
    }

    /// Phase 1 — decode the source frame for timeline `t` (and its overlay) and
    /// stash it for the render phase. The render can't run yet: the crop box is
    /// computed once *all* panes' sizes are known (`ExportPlan::pane_boxes`). A
    /// no-op when the same frame is already decoded; keeps the previous on a miss.
    fn decode(&mut self, t: usize) {
        self.ensure_overlay(t);
        let idx = self.src_index(t);
        if self.raw_idx == Some(idx) {
            return;
        }
        let Some(frame) = decode_source(&self.source, idx, &mut self.reader, &mut self.cur_file)
        else {
            return;
        };
        self.cur_size = frame.size;
        self.raw_frame = Some(frame);
        self.raw_idx = Some(idx);
    }

    /// Phase 2 — render the pane's display buffer for the source-space crop box
    /// `b = [x0, y0, w, h]`, running the **whole** tone pipeline (LUT bounds, LUT
    /// render, and the proprietary LUT_ALPHA / details operators) on **only that
    /// sub-rectangle**, so a small crop never processes the full image. `None` =
    /// the pane isn't sampled by the output (nothing to render).
    ///
    /// The box is the axis-aligned bounding box, in the *unrotated* source image,
    /// of the pixels the (possibly rotated) crop actually samples — so a rotated
    /// pane renders the tight source rectangle that covers its rotated region.
    fn render(&mut self, b: Option<[usize; 4]>) {
        if self.cur_idx == self.raw_idx && self.cur_box == b {
            return; // already rendered this frame at this box
        }
        let Some(frame) = self.raw_frame.clone() else {
            return;
        };
        let Some([x0, y0, cw, ch]) = b else {
            self.cur_display = None;
            self.cur_box = None;
            self.cur_idx = self.raw_idx;
            return;
        };
        // Crop first, so bounds / LUT / operators all see only the region. The
        // Linear clip toggle + its per-tail percentile (`self.clip`) mirror the
        // pane's tone; LUT_ALPHA takes the full range (`clip == None`). The
        // operators run on a single-channel 16-bit render, only for single-channel
        // 16-bit frames with the library loaded; everything else is the plain
        // 8-bit render. Matches the live view (on the cropped region).
        //
        // Skip the copy when the box already covers the whole frame (e.g. a
        // full-view export), so that common case is no slower than before.
        let [fw, fh] = frame.size;
        let cropped;
        let sub: &FrameData = if x0 == 0 && y0 == 0 && cw == fw && ch == fh {
            &*frame
        } else {
            cropped = frame.crop(x0, y0, cw, ch);
            &cropped
        };
        let (lo, hi) = match self.clip {
            Some(pct) => sub.clip_bounds(pct),
            None => sub.display_bounds(false),
        };
        // The one shared render tail (plain LUT, or operators on a full-precision
        // 16-bit render) — identical to the live-view path by construction.
        let mut rgba = Vec::new();
        self.ops.render_display(
            &sub,
            lo,
            hi,
            self.contrast == ContrastMode::LutAlpha,
            self.details,
            &mut rgba,
        );
        self.cur_display = Some(rgba);
        self.cur_origin = [x0, y0];
        self.cur_render_size = [cw, ch];
        self.cur_box = b;
        self.cur_idx = self.raw_idx;
    }

    /// Undo the pane's display rotation on a sampled image point: map the point
    /// `ip` (in the unrotated view's image space) to the source pixel it shows by
    /// rotating it by `-rotation` about the image centre. Mirrors the live view's
    /// `rot_screen_to_img`.
    fn unrotate(&self, ip: Vec2) -> Vec2 {
        if self.rotation == 0.0 {
            return ip;
        }
        let [w, h] = self.cur_size;
        let center = Vec2::new(w as f32 / 2.0, h as f32 / 2.0);
        let d = ip - center;
        let (s, c) = (-self.rotation).sin_cos();
        center + Vec2::new(d.x * c - d.y * s, d.x * s + d.y * c)
    }

    /// Sample the composited pane colour (base image with the mask overlay, if
    /// any, blended on top) at image-space point `ip`.
    fn sample(&self, ip: Vec2) -> Option<[u8; 3]> {
        let base = self.sample_base(ip)?;
        Some(self.blend_overlay(base, ip))
    }

    /// Blend the mask overlay over `base` at image point `ip`. The overlay is
    /// stretched onto the base image rect (as in the live view), so the base
    /// pixel maps to the mask pixel proportionally.
    fn blend_overlay(&self, base: [u8; 3], ip: Vec2) -> [u8; 3] {
        let Some(ov) = &self.overlay else {
            return base;
        };
        let Some(buf) = &ov.cur_mask else {
            return base;
        };
        let ([mw, mh], [bw, bh]) = (ov.cur_size, self.cur_size);
        if mw == 0 || mh == 0 || bw == 0 || bh == 0 {
            return base;
        }
        let mx = (ip.x / bw as f32 * mw as f32) as usize;
        let my = (ip.y / bh as f32 * mh as f32) as usize;
        if mx >= mw || my >= mh {
            return base;
        }
        let i = (my * mw + mx) * 4;
        let a = buf[i + 3] as f32 / 255.0;
        if a <= 0.0 {
            return base;
        }
        let mut out = base;
        for k in 0..3 {
            out[k] = (base[k] as f32 * (1.0 - a) + buf[i + k] as f32 * a).round() as u8;
        }
        out
    }

    /// Nearest-neighbour sample of the base image at image point `ip`. Always
    /// nearest so every exported pixel is a true source value, never a blend —
    /// upscaling just replicates source pixels.
    fn sample_base(&self, ip: Vec2) -> Option<[u8; 3]> {
        let buf = self.cur_display.as_ref()?;
        // `cur_display` covers only the crop box (`cur_render_size`) whose top-left
        // is `cur_origin` in full-image pixels — shift `ip` into that sub-buffer.
        let [ox, oy] = self.cur_origin;
        let [cw, ch] = self.cur_render_size;
        let x = ip.x - ox as f32;
        let y = ip.y - oy as f32;
        if cw == 0 || ch == 0 || x < 0.0 || y < 0.0 || x >= cw as f32 || y >= ch as f32 {
            return None;
        }
        let i = (y as usize * cw + x as usize) * 4;
        Some([buf[i], buf[i + 1], buf[i + 2]])
    }
}

/// One cell of a grid export. `place` is the slot in composition space; `area`
/// is the screen rect the pane's `view` was calibrated to; `content` is the
/// sub-rect of `area` that `place` shows. Decoupling `place` from `area` lets the
/// export pack each pane's *content* flush (no gaps or background margins)
/// regardless of how it was panned/zoomed on screen: a composition point in
/// `place` is remapped into `content` (same size) before the view samples it.
pub struct GridCell {
    pub pane: usize,
    pub place: Rect,
    pub area: Rect,
    pub content: Rect,
}

/// Which pane occupies which region of the composited frame.
pub enum ExportLayout {
    Grid(Vec<GridCell>),
    Single(usize, Rect), // pane, image area
    Ab {                 // wipe
        a: usize,
        b: usize,
        img: Rect,
        split_x: f32,
    },
}

/// A resolved hit: which pane, the screen area its view maps against, and the
/// point (in that area's space) to sample.
struct Located {
    pane: usize,
    area: Rect,
    sample: Pos2,
}

impl ExportLayout {
    fn locate(&self, c: Pos2) -> Option<Located> {
        match self {
            ExportLayout::Grid(cells) => cells.iter().find(|g| g.place.contains(c)).map(|g| Located {
                pane: g.pane,
                area: g.area,
                sample: g.content.min + (c - g.place.min),
            }),
            ExportLayout::Single(i, r) => r.contains(c).then_some(Located {
                pane: *i,
                area: *r,
                sample: c,
            }),
            ExportLayout::Ab { a, b, img, split_x } => img.contains(c).then_some(Located {
                pane: if c.x < *split_x { *a } else { *b },
                area: *img,
                sample: c,
            }),
        }
    }
}

pub struct ExportPlan {
    pub panes: Vec<ExportPane>,
    pub layout: ExportLayout,
    pub region: Rect,
    pub out_w: usize,
    pub out_h: usize,
    /// First timeline frame to export; frame `t` of the output shows timeline
    /// position `start + t`.
    pub start: usize,
    pub total: usize,
}

impl ExportPlan {
    /// Composite output frame `t` (of `total`) into an RGBA buffer.
    pub fn compose(&mut self, t: usize) -> Vec<u8> {
        // Decode every pane's source frame first (so their sizes are known), then
        // compute the source-space crop box each one actually needs and render
        // only that sub-rectangle — the tone pipeline (LUT + operators) never
        // touches pixels outside the exported region.
        for p in &mut self.panes {
            p.decode(self.start + t);
        }
        let boxes = self.pane_boxes();
        for (p, b) in self.panes.iter_mut().zip(boxes) {
            p.render(b);
        }
        let (w, h) = (self.out_w, self.out_h);
        let (rw, rh) = (self.region.width(), self.region.height());
        let mut out = vec![0u8; w * h * 4];
        for oy in 0..h {
            let cy = self.region.min.y + (oy as f32 + 0.5) / h as f32 * rh;
            for ox in 0..w {
                let cx = self.region.min.x + (ox as f32 + 0.5) / w as f32 * rw;
                let c = Pos2::new(cx, cy);
                let o = (oy * w + ox) * 4;
                // Uncovered pixels (gutters between panes, letterboxing around an
                // image) get alpha 0 — flagged as background. The still export
                // crops them off (`crop_to_content`); MP4 (yuv420p) ignores alpha,
                // so its dark `BG` is unchanged.
                let mut col = [BG[0], BG[1], BG[2], 0];
                if let Some(loc) = self.layout.locate(c) {
                    let pane = &self.panes[loc.pane];
                    let ip = pane.unrotate(pane.view.screen_to_img(loc.sample, loc.area));
                    if let Some(rgb) = pane.sample(ip) {
                        col = [rgb[0], rgb[1], rgb[2], 255];
                    }
                }
                out[o..o + 4].copy_from_slice(&col);
            }
        }
        out
    }

    /// Per-pane source-image crop box `[x0, y0, w, h]` (or `None` when a pane is
    /// not sampled): the axis-aligned bounding box, in the *unrotated* source
    /// image, of the pixels the exported region actually reads from that pane.
    ///
    /// The composition→source map (region → cell content → view `screen_to_img` →
    /// `unrotate`) is a pure affine (the view is a rotation-free similarity, and
    /// `unrotate` is a rotation), so an axis-aligned composition rectangle maps to
    /// a parallelogram whose bounding box is exactly the bound over its four
    /// corners — mapping corners is exact for any rotation, and O(cells) rather
    /// than a per-output-pixel pass. A 1px pad absorbs rounding at the edges.
    fn pane_boxes(&self) -> Vec<Option<[usize; 4]>> {
        // Accumulate a float bbox (min/max source x,y) per pane over mapped corners.
        let mut acc: Vec<Option<(f32, f32, f32, f32)>> = vec![None; self.panes.len()];
        let hit = |acc: &mut Vec<Option<(f32, f32, f32, f32)>>, pane: usize, sample: Pos2, area: Rect| {
            let p = &self.panes[pane];
            let ip = p.unrotate(p.view.screen_to_img(sample, area));
            let e = &mut acc[pane];
            *e = Some(match *e {
                None => (ip.x, ip.y, ip.x, ip.y),
                Some((x0, y0, x1, y1)) => (x0.min(ip.x), y0.min(ip.y), x1.max(ip.x), y1.max(ip.y)),
            });
        };
        let corners = |r: Rect| {
            [
                r.min,
                Pos2::new(r.max.x, r.min.y),
                Pos2::new(r.min.x, r.max.y),
                r.max,
            ]
        };
        // A rectangle with real area (intersect can yield an empty/negative one).
        let positive = |r: Rect| r.width() > 0.0 && r.height() > 0.0;
        match &self.layout {
            ExportLayout::Single(i, r) => {
                let cr = self.region.intersect(*r);
                if positive(cr) {
                    for c in corners(cr) {
                        hit(&mut acc, *i, c, *r);
                    }
                }
            }
            ExportLayout::Ab { a, b, img, split_x } => {
                let base = self.region.intersect(*img);
                if positive(base) {
                    let mid = split_x.clamp(base.min.x, base.max.x);
                    let left = Rect::from_min_max(base.min, Pos2::new(mid, base.max.y));
                    let right = Rect::from_min_max(Pos2::new(mid, base.min.y), base.max);
                    if positive(left) {
                        for c in corners(left) {
                            hit(&mut acc, *a, c, *img);
                        }
                    }
                    if positive(right) {
                        for c in corners(right) {
                            hit(&mut acc, *b, c, *img);
                        }
                    }
                }
            }
            ExportLayout::Grid(cells) => {
                for g in cells {
                    let pr = self.region.intersect(g.place);
                    if !positive(pr) {
                        continue;
                    }
                    // A composition point maps into `content` before the view samples.
                    let off = g.content.min - g.place.min;
                    for c in corners(pr) {
                        hit(&mut acc, g.pane, c + off, g.area);
                    }
                }
            }
        }
        // Float bbox → integer pixel box, padded then clamped to each frame's size.
        acc.into_iter()
            .enumerate()
            .map(|(i, e)| {
                let (x0, y0, x1, y1) = e?;
                let [w, h] = self.panes[i].cur_size;
                if w == 0 || h == 0 {
                    return None;
                }
                let clampw = |v: f32| v.max(0.0).min(w as f32) as usize;
                let clamph = |v: f32| v.max(0.0).min(h as f32) as usize;
                let ix0 = clampw((x0 - 1.0).floor());
                let iy0 = clamph((y0 - 1.0).floor());
                let ix1 = clampw((x1 + 1.0).floor() + 1.0);
                let iy1 = clamph((y1 + 1.0).floor() + 1.0);
                (ix1 > ix0 && iy1 > iy0).then_some([ix0, iy0, ix1 - ix0, iy1 - iy0])
            })
            .collect()
    }
}

/// A running ffmpeg encoder fed raw RGBA frames over stdin.
pub struct Encoder {
    child: Child,
    stdin: Option<ChildStdin>,
    log: Arc<Mutex<String>>,
}

impl Encoder {
    pub fn start(path: &Path, w: usize, h: usize, fps: f32, crf: u32) -> Result<Self, String> {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-y", "-f", "rawvideo", "-pixel_format", "rgba", "-video_size"])
            .arg(format!("{w}x{h}"))
            .arg("-framerate")
            .arg(format!("{fps}"))
            .args(["-i", "pipe:0", "-an", "-c:v", "libx264", "-preset", "medium"])
            .args(["-pix_fmt", "yuv420p", "-crf"])
            .arg(format!("{crf}"))
            .args(["-movflags", "+faststart"])
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "ffmpeg not found on PATH. Install it (RHEL: `sudo dnf install ffmpeg`, \
                 Ubuntu: `sudo apt install ffmpeg`, Windows: ffmpeg.org) and retry."
                    .to_string()
            } else {
                format!("failed to start ffmpeg: {e}")
            }
        })?;

        let stdin = child.stdin.take();
        let log = Arc::new(Mutex::new(String::new()));
        if let Some(mut err) = child.stderr.take() {
            let l = log.clone();
            thread::spawn(move || {
                let mut s = String::new();
                let _ = err.read_to_string(&mut s);
                *l.lock().unwrap() = s; // drained so ffmpeg never blocks on stderr
            });
        }
        Ok(Self { child, stdin, log })
    }

    pub fn write_frame(&mut self, buf: &[u8]) -> Result<(), String> {
        self.stdin
            .as_mut()
            .ok_or_else(|| "encoder stdin closed".to_string())?
            .write_all(buf)
            .map_err(|e| format!("write to ffmpeg: {e}"))
    }

    pub fn finish(&mut self) -> Result<(), String> {
        self.stdin.take(); // closing stdin lets ffmpeg finalise the file
        let status = self.child.wait().map_err(|e| format!("wait ffmpeg: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            let tail = self.log.lock().unwrap();
            let tail: String = tail.lines().rev().take(3).collect::<Vec<_>>().join(" | ");
            Err(format!("ffmpeg failed: {tail}"))
        }
    }

    pub fn kill(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Crop an RGBA buffer to the bounding box of its **content** (pixels with
/// alpha > 0), discarding the surrounding background so a still export cuts the
/// gutters/letterboxing off entirely. Returns `None` when everything is
/// background. Interior background (e.g. a grid gutter) can't be removed from a
/// rectangle, so those pixels keep alpha 0.
pub fn crop_to_content(rgba: &[u8], w: usize, h: usize) -> Option<(usize, usize, Vec<u8>)> {
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (usize::MAX, usize::MAX, 0usize, 0usize);
    for y in 0..h {
        for x in 0..w {
            if rgba[(y * w + x) * 4 + 3] != 0 {
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    if min_x > max_x {
        return None; // no content
    }
    let (cw, ch) = (max_x - min_x + 1, max_y - min_y + 1);
    let mut out = vec![0u8; cw * ch * 4];
    for y in 0..ch {
        let src = ((min_y + y) * w + min_x) * 4;
        let dst = y * cw * 4;
        out[dst..dst + cw * 4].copy_from_slice(&rgba[src..src + cw * 4]);
    }
    Some((cw, ch, out))
}

/// Save one composited RGBA frame as a still image, format chosen by extension.
/// The background is already cropped off by [`crop_to_content`]; any residual
/// (interior) transparent pixels are kept as-is for PNG, and flattened onto white
/// for JPEG (which has no alpha) so a black background is never baked in.
pub fn save_image(path: &Path, w: usize, h: usize, rgba: &[u8]) -> Result<(), String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => image::save_buffer(path, rgba, w as u32, h as u32, image::ColorType::Rgba8)
            .map_err(|e| format!("write PNG: {e}")),
        "jpg" | "jpeg" => {
            let mut rgb = vec![0u8; w * h * 3];
            for i in 0..w * h {
                if rgba[i * 4 + 3] == 0 {
                    rgb[i * 3..i * 3 + 3].copy_from_slice(&[255, 255, 255]);
                } else {
                    rgb[i * 3..i * 3 + 3].copy_from_slice(&rgba[i * 4..i * 4 + 3]);
                }
            }
            image::save_buffer(path, &rgb, w as u32, h as u32, image::ColorType::Rgb8)
                .map_err(|e| format!("write JPEG: {e}"))
        }
        other => Err(format!("unsupported image extension '.{other}' — use .png or .jpg")),
    }
}

/// Output pixel size for a region and target height (both forced even).
pub fn out_dims(region: Rect, target_h: u32) -> (usize, usize) {
    let h = (target_h.max(2) as usize) & !1;
    let aspect = (region.width() / region.height().max(1.0)).max(0.01);
    let w = (((h as f32 * aspect).round() as usize).max(2)) & !1;
    (w, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::Samples;

    /// An image-space crop exported 1:1 must reproduce exactly the region's
    /// pixels: cell of the crop's size + view (zoom 1, centred on the crop).
    #[test]
    fn region_crop_is_pixel_exact() {
        // 8×8 gradient: value = y*8 + x, easy to identify per pixel.
        let frame = FrameData::new([8, 8], 1, Samples::U8((0..64).collect()));
        // Crop the 4×2 region at (2, 3).
        let reg = Rect::from_min_size(Pos2::new(2.0, 3.0), Vec2::new(4.0, 2.0));
        let cell = Rect::from_min_size(Pos2::ZERO, reg.size());
        let view = ViewTransform {
            zoom: 1.0,
            center: reg.center().to_vec2(),
            needs_fit: false,
        };
        let pane = ExportPane::new(
            view,
            ContrastMode::Linear,
            false,
            None, // no clip
            1,
            true,
            0,
            ExportSource::Still(Arc::new(frame)),
        );
        let mut plan = ExportPlan {
            panes: vec![pane],
            layout: ExportLayout::Single(0, cell),
            region: cell,
            out_w: 4,
            out_h: 2,
            start: 0,
            total: 1,
        };
        let buf = plan.compose(0);
        for oy in 0..2 {
            for ox in 0..4 {
                let expect = (3 + oy) * 8 + (2 + ox); // source pixel value
                let got = buf[(oy * 4 + ox) * 4] as usize;
                assert_eq!(got, expect, "pixel ({ox},{oy})");
            }
        }
    }

    /// A **rotated** pane with a small region crop still exports pixel-exact: the
    /// render box is the source bounding box of the rotated crop, and sampling
    /// offsets into that sub-buffer. 180° flips the region about the image centre.
    #[test]
    fn rotated_region_crop_is_pixel_exact() {
        let frame = FrameData::new([8, 8], 1, Samples::U8((0..64).collect()));
        let reg = Rect::from_min_size(Pos2::new(2.0, 3.0), Vec2::new(4.0, 2.0));
        let cell = Rect::from_min_size(Pos2::ZERO, reg.size());
        let view = ViewTransform {
            zoom: 1.0,
            center: reg.center().to_vec2(),
            needs_fit: false,
        };
        let mut pane = ExportPane::new(
            view,
            ContrastMode::Linear,
            false,
            None,
            1,
            true,
            0,
            ExportSource::Still(Arc::new(frame)),
        );
        pane.rotation = std::f32::consts::PI; // 180°
        let mut plan = ExportPlan {
            panes: vec![pane],
            layout: ExportLayout::Single(0, cell),
            region: cell,
            out_w: 4,
            out_h: 2,
            start: 0,
            total: 1,
        };
        let buf = plan.compose(0);
        for oy in 0..2usize {
            for ox in 0..4usize {
                // 180° about the 8×8 centre maps the unrotated crop pixel
                // (2+ox, 3+oy) to (5-ox, 4-oy).
                let expect = (4 - oy) * 8 + (5 - ox);
                let got = buf[(oy * 4 + ox) * 4] as usize;
                assert_eq!(got, expect, "pixel ({ox},{oy})");
            }
        }
    }

    /// A mask overlay must tint the exported composite: diagonal mask pixels
    /// take the overlay colour, the rest keep the base image.
    #[test]
    fn mask_overlay_tints_export() {
        let base = FrameData::new([8, 8], 1, Samples::U8(vec![100u8; 64]));
        let mut m = vec![0u8; 64];
        for k in 0..8 {
            m[k * 8 + k] = 1; // true on the diagonal
        }
        let mask = FrameData::new_mask([8, 8], 1, Samples::U8(m));

        let cell = Rect::from_min_size(Pos2::ZERO, Vec2::new(8.0, 8.0));
        let view = ViewTransform {
            zoom: 1.0,
            center: Vec2::new(4.0, 4.0),
            needs_fit: false,
        };
        let mut pane = ExportPane::new(
            view,
            ContrastMode::Linear,
            false,
            None, // no clip
            1,
            true,
            0,
            ExportSource::Still(Arc::new(base)),
        );
        pane.set_overlay(
            ExportSource::Still(Arc::new(mask)),
            1,
            true,
            0,
            [255, 0, 0],
            255,
        );
        let mut plan = ExportPlan {
            panes: vec![pane],
            layout: ExportLayout::Single(0, cell),
            region: cell,
            out_w: 8,
            out_h: 8,
            start: 0,
            total: 1,
        };
        let buf = plan.compose(0);
        for y in 0..8 {
            for x in 0..8 {
                let o = (y * 8 + x) * 4;
                let px = [buf[o], buf[o + 1], buf[o + 2]];
                if x == y {
                    assert_eq!(px, [255, 0, 0], "diagonal ({x},{y}) tinted");
                } else {
                    assert_eq!(px, [100, 100, 100], "off-diagonal ({x},{y}) base");
                }
            }
        }
    }

    /// A 180° pane rotation flips the exported image about its centre: output
    /// pixel (x, y) shows source pixel (w-1-x, h-1-y).
    #[test]
    fn rotation_180_flips_export() {
        let frame = FrameData::new([8, 8], 1, Samples::U8((0..64).collect()));
        let cell = Rect::from_min_size(Pos2::ZERO, Vec2::new(8.0, 8.0));
        let view = ViewTransform {
            zoom: 1.0,
            center: Vec2::new(4.0, 4.0),
            needs_fit: false,
        };
        let mut pane = ExportPane::new(
            view,
            ContrastMode::Linear,
            false,
            None, // no clip
            1,
            true,
            0,
            ExportSource::Still(Arc::new(frame)),
        );
        pane.rotation = std::f32::consts::PI; // 180°
        let mut plan = ExportPlan {
            panes: vec![pane],
            layout: ExportLayout::Single(0, cell),
            region: cell,
            out_w: 8,
            out_h: 8,
            start: 0,
            total: 1,
        };
        let buf = plan.compose(0);
        for oy in 0..8usize {
            for ox in 0..8usize {
                let expect = (7 - oy) * 8 + (7 - ox); // flipped source value
                let got = buf[(oy * 8 + ox) * 4] as usize;
                assert_eq!(got, expect, "pixel ({ox},{oy})");
            }
        }
    }

    /// With the image panned so it only partly covers the view, exporting the
    /// **content** region (not the full view) yields no background pixels.
    #[test]
    fn content_region_excludes_background() {
        let frame = FrameData::new([4, 4], 1, Samples::U8((0..16).collect()));
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(10.0, 10.0));
        let view = ViewTransform {
            zoom: 1.0,
            center: Vec2::new(2.0, 2.0),
            needs_fit: false,
        };
        // The image occupies only a 4×4 sub-rect of the 10×10 view.
        let content = view.image_rect([4, 4], area).intersect(area);
        let pane = ExportPane::new(
            view,
            ContrastMode::Linear,
            false,
            None, // no clip
            1,
            true,
            0,
            ExportSource::Still(Arc::new(frame)),
        );
        let mut plan = ExportPlan {
            panes: vec![pane],
            layout: ExportLayout::Single(0, area),
            region: content,
            out_w: 4,
            out_h: 4,
            start: 0,
            total: 1,
        };
        let buf = plan.compose(0);
        for i in 0..4 * 4 {
            assert_eq!(buf[i * 4 + 3], 255, "pixel {i} must be content, not background");
        }
    }

    /// The background (alpha 0) is cropped off, leaving only the content's
    /// bounding box.
    #[test]
    fn crop_to_content_trims_background() {
        // 4×3 buffer, a single opaque pixel at (2,1) surrounded by background.
        let (w, h) = (4usize, 3usize);
        let mut rgba = vec![BG[0], BG[1], BG[2], 0].repeat(w * h);
        let i = (1 * w + 2) * 4;
        rgba[i..i + 4].copy_from_slice(&[10, 20, 30, 255]);
        let (cw, ch, out) = crop_to_content(&rgba, w, h).expect("has content");
        assert_eq!((cw, ch), (1, 1));
        assert_eq!(&out[..4], &[10, 20, 30, 255]);
        // An all-background buffer yields nothing to export.
        assert!(crop_to_content(&vec![0u8; w * h * 4], w, h).is_none());
    }

    /// A full-frame 1:1 export must equal the live LUT render byte-for-byte,
    /// including the pane's clip percentile (the export honours the same tone
    /// as the view). This locks the export half of the "all render paths match
    /// pixel-for-pixel" invariant before the paths are unified.
    #[test]
    fn full_frame_export_matches_lut_render() {
        // u16 ramp with tail outliers so the 0.5% clip actually cuts values.
        let mut v: Vec<u16> = (0..16 * 8).map(|i| 1000 + (i as u16) * 300).collect();
        v[0] = 0;
        v[127] = 65535;
        let frame = Arc::new(FrameData::new([16, 8], 1, Samples::U16(v)));

        let cell = Rect::from_min_size(Pos2::ZERO, Vec2::new(16.0, 8.0));
        let view = ViewTransform {
            zoom: 1.0,
            center: Vec2::new(8.0, 4.0),
            needs_fit: false,
        };
        let pane = ExportPane::new(
            view,
            ContrastMode::Linear,
            false,
            Some(0.5), // clip on, non-default percentile
            1,
            true,
            0,
            ExportSource::Still(frame.clone()),
        );
        let mut plan = ExportPlan {
            panes: vec![pane],
            layout: ExportLayout::Single(0, cell),
            region: cell,
            out_w: 16,
            out_h: 8,
            start: 0,
            total: 1,
        };
        let buf = plan.compose(0);

        let (lo, hi) = frame.clip_bounds(0.5);
        let mut reference = Vec::new();
        frame.render_into(lo, hi, &mut reference);
        for i in 0..16 * 8 {
            assert_eq!(
                buf[i * 4..i * 4 + 3],
                reference[i * 4..i * 4 + 3],
                "pixel {i}"
            );
        }
    }

    /// Full compose → ffmpeg encode of a few frames. Skips gracefully only if
    /// ffmpeg is unavailable (e.g. CI without ffmpeg).
    #[test]
    fn export_single_pane_mp4() {
        let dir = crate::testutil::fixture_dir("export_mp4");
        let src = dir.join("seq.tif");
        crate::testutil::write_multipage_tiff_u16(&src, &[[64, 48]; 4]);
        let area = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(320.0, 240.0));
        let frame0 = SeqReader::open(&src)
            .expect("open")
            .decode(0)
            .expect("decode page 0")
            .expect("page 0 exists");
        let mut view = ViewTransform::default();
        view.fit(frame0.size, area);

        let pane = ExportPane::new(
            view,
            ContrastMode::Linear,
            false,
            None, // no clip
            4,
            true,
            0,
            ExportSource::Seq { path: src },
        );
        let mut plan = ExportPlan {
            panes: vec![pane],
            layout: ExportLayout::Single(0, area),
            region: area,
            out_w: 160,
            out_h: 120,
            start: 0,
            total: 4,
        };

        let out = std::env::temp_dir().join("cim_plan_test.mp4");
        let mut enc = match Encoder::start(&out, 160, 120, 12.0, 28) {
            Ok(e) => e,
            Err(_) => return, // ffmpeg not installed
        };
        for t in 0..plan.total {
            let buf = plan.compose(t);
            assert_eq!(buf.len(), 160 * 120 * 4);
            enc.write_frame(&buf).expect("write frame");
        }
        enc.finish().expect("ffmpeg finish");
        assert!(out.metadata().map(|m| m.len() > 0).unwrap_or(false));
    }
}
