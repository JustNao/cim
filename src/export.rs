//! MP4 comparison export.
//!
//! The app builds a self-contained [`ExportPlan`] (a snapshot of layout, views,
//! tone/detail settings and media sources), then composites each timeline frame on the CPU
//! and pipes raw RGBA to the `ffmpeg` CLI, which encodes H.264. Because the plan
//! is decoupled from live app state, the whole compose+encode loop runs on a
//! worker thread (`app/export_ui.rs::run_export`) while the UI keeps interacting.

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
    cur_idx: Option<usize>,
    cur_display: Option<Vec<u8>>,
    cur_size: [usize; 2],
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
    pub fn new(
        view: ViewTransform,
        contrast: ContrastMode,
        details: bool,
        count: usize,
        sync_temporal: bool,
        own_frame: usize,
        source: ExportSource,
    ) -> Self {
        Self {
            view,
            contrast,
            details,
            count,
            sync_temporal,
            own_frame,
            source,
            reader: None,
            cur_file: None,
            cur_idx: None,
            cur_display: None,
            cur_size: [0, 0],
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
            frame.render_mask_rgba(ov.color, ov.alpha, &mut buf);
            ov.cur_mask = Some(buf);
            ov.cur_idx = Some(idx);
        }
    }

    /// Ensure the display buffer for timeline `t` is decoded + rendered.
    fn ensure_frame(&mut self, t: usize) {
        self.ensure_overlay(t);
        let idx = self.src_index(t);
        if self.cur_idx == Some(idx) {
            return;
        }
        // Keep the previous frame on any open/decode miss.
        let Some(frame) = decode_source(&self.source, idx, &mut self.reader, &mut self.cur_file)
        else {
            return;
        };
        self.cur_size = frame.size;
        // Built-in render (full range or 0.01% clip), then the same proprietary
        // operators the live view applies, so an export matches what's on screen.
        let [w, h] = frame.size;
        let mut rgba = frame.render_rgba(self.contrast.clips());
        if self.contrast == ContrastMode::LutAlpha {
            crate::imageproc::lut_alpha(&mut rgba, w, h);
        }
        if self.details {
            crate::imageproc::details_enhanced(&mut rgba, w, h);
        }
        self.cur_display = Some(rgba);
        self.cur_idx = Some(idx);
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
        let [w, h] = self.cur_size;
        let buf = self.cur_display.as_ref()?;
        if w == 0 || h == 0 || ip.x < 0.0 || ip.y < 0.0 || ip.x >= w as f32 || ip.y >= h as f32 {
            return None;
        }
        let (x, y) = (ip.x as usize, ip.y as usize);
        let i = (y * w + x) * 4;
        Some([buf[i], buf[i + 1], buf[i + 2]])
    }
}

/// Which pane occupies which region of the composited frame.
pub enum ExportLayout {
    Grid(Vec<(usize, Rect)>),          // (plan pane index, image area)
    Single(usize, Rect),               // pane, image area
    Ab {                               // wipe
        a: usize,
        b: usize,
        img: Rect,
        split_x: f32,
    },
}

impl ExportLayout {
    fn locate(&self, c: Pos2) -> Option<(usize, Rect)> {
        match self {
            ExportLayout::Grid(cells) => cells.iter().find(|(_, r)| r.contains(c)).copied(),
            ExportLayout::Single(i, r) => r.contains(c).then_some((*i, *r)),
            ExportLayout::Ab { a, b, img, split_x } => img
                .contains(c)
                .then_some((if c.x < *split_x { *a } else { *b }, *img)),
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
        for p in &mut self.panes {
            p.ensure_frame(self.start + t);
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
                let mut col = BG;
                if let Some((pi, area)) = self.layout.locate(c) {
                    let pane = &self.panes[pi];
                    let ip = pane.view.screen_to_img(c, area);
                    if let Some(rgb) = pane.sample(ip) {
                        col = [rgb[0], rgb[1], rgb[2], 255];
                    }
                }
                out[o..o + 4].copy_from_slice(&col);
            }
        }
        out
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
    use std::path::PathBuf;

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

    /// Full compose → ffmpeg encode of a few frames. Skips gracefully if the
    /// fixture or ffmpeg is unavailable (e.g. CI without ffmpeg).
    #[test]
    fn export_single_pane_mp4() {
        let src = PathBuf::from("examples/alpes_noisy_a.tif");
        if !src.exists() {
            return; // fixtures not present
        }
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
