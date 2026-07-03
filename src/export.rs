//! MP4 comparison export.
//!
//! The app builds a self-contained [`ExportPlan`] (a snapshot of layout, views,
//! clip flags and media sources), then composites each timeline frame on the CPU
//! and pipes raw RGBA to the `ffmpeg` CLI, which encodes H.264. Keeping the plan
//! decoupled from live app state means an in-progress export is stable even if
//! the user keeps interacting, and it could move to a worker thread later.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use eframe::egui::{Pos2, Rect, Vec2};

use crate::media::{FrameData, SeqReader};
use crate::view::ViewTransform;

const BG: [u8; 4] = [24, 24, 24, 255];

/// Where a pane's frames come from during export.
pub enum ExportSource {
    Still(Arc<FrameData>),
    Seq { path: std::path::PathBuf },
}

/// One pane, snapshotted for export, plus a small decode/render cache.
pub struct ExportPane {
    pub view: ViewTransform,
    pub clip: bool,
    pub count: usize,
    pub sync_temporal: bool,
    pub own_frame: usize,
    pub source: ExportSource,

    /// Persistent reader for `Seq` sources, opened on first use, so a long
    /// export doesn't re-walk the IFD chain for every frame.
    reader: Option<SeqReader>,
    cur_idx: Option<usize>,
    cur_display: Option<Vec<u8>>,
    cur_size: [usize; 2],
}

impl ExportPane {
    pub fn new(
        view: ViewTransform,
        clip: bool,
        count: usize,
        sync_temporal: bool,
        own_frame: usize,
        source: ExportSource,
    ) -> Self {
        Self {
            view,
            clip,
            count,
            sync_temporal,
            own_frame,
            source,
            reader: None,
            cur_idx: None,
            cur_display: None,
            cur_size: [0, 0],
        }
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

    /// Ensure the display buffer for timeline `t` is decoded + rendered.
    fn ensure_frame(&mut self, t: usize) {
        let idx = self.src_index(t);
        if self.cur_idx == Some(idx) {
            return;
        }
        let frame = match &self.source {
            ExportSource::Still(f) => f.clone(),
            ExportSource::Seq { path } => {
                if self.reader.is_none() {
                    match SeqReader::open(path) {
                        Ok(r) => self.reader = Some(r),
                        Err(_) => return, // keep last frame on open failure
                    }
                }
                match self.reader.as_mut().unwrap().decode(idx) {
                    Ok(Some(f)) => Arc::new(f),
                    // No page here (past the end) or a decode error: keep last.
                    Ok(None) | Err(_) => return,
                }
            }
        };
        self.cur_size = frame.size;
        self.cur_display = Some(frame.render_rgba(self.clip));
        self.cur_idx = Some(idx);
    }

    fn sample(&self, ip: Vec2, bilinear: bool) -> Option<[u8; 3]> {
        let [w, h] = self.cur_size;
        let buf = self.cur_display.as_ref()?;
        if w == 0 || h == 0 || ip.x < 0.0 || ip.y < 0.0 || ip.x >= w as f32 || ip.y >= h as f32 {
            return None;
        }
        let get = |x: usize, y: usize| -> [u8; 3] {
            let i = (y * w + x) * 4;
            [buf[i], buf[i + 1], buf[i + 2]]
        };
        if !bilinear {
            return Some(get(ip.x as usize, ip.y as usize));
        }
        // Bilinear on pixel centres, clamped at the edges.
        let fx = (ip.x - 0.5).max(0.0);
        let fy = (ip.y - 0.5).max(0.0);
        let x0 = (fx.floor() as usize).min(w - 1);
        let y0 = (fy.floor() as usize).min(h - 1);
        let x1 = (x0 + 1).min(w - 1);
        let y1 = (y0 + 1).min(h - 1);
        let tx = fx - x0 as f32;
        let ty = fy - y0 as f32;
        let (a, b, c, d) = (get(x0, y0), get(x1, y0), get(x0, y1), get(x1, y1));
        let mut out = [0u8; 3];
        for k in 0..3 {
            let top = a[k] as f32 * (1.0 - tx) + b[k] as f32 * tx;
            let bot = c[k] as f32 * (1.0 - tx) + d[k] as f32 * tx;
            out[k] = (top * (1.0 - ty) + bot * ty).round() as u8;
        }
        Some(out)
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
    pub bilinear: bool,
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
                    if let Some(rgb) = pane.sample(ip, self.bilinear) {
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
        let frame = FrameData {
            size: [8, 8],
            channels: 1,
            samples: Samples::U8((0..64).collect()),
        };
        // Crop the 4×2 region at (2, 3).
        let reg = Rect::from_min_size(Pos2::new(2.0, 3.0), Vec2::new(4.0, 2.0));
        let cell = Rect::from_min_size(Pos2::ZERO, reg.size());
        let view = ViewTransform {
            zoom: 1.0,
            center: reg.center().to_vec2(),
            needs_fit: false,
        };
        let pane = ExportPane::new(view, false, 1, true, 0, ExportSource::Still(Arc::new(frame)));
        let mut plan = ExportPlan {
            panes: vec![pane],
            layout: ExportLayout::Single(0, cell),
            region: cell,
            out_w: 4,
            out_h: 2,
            start: 0,
            total: 1,
            bilinear: false,
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

        let pane = ExportPane::new(view, false, 4, true, 0, ExportSource::Seq { path: src });
        let mut plan = ExportPlan {
            panes: vec![pane],
            layout: ExportLayout::Single(0, area),
            region: area,
            out_w: 160,
            out_h: 120,
            start: 0,
            total: 4,
            bilinear: true,
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
