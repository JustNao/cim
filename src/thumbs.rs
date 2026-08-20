//! Timeline hover-preview thumbnails — one dedicated worker thread.
//!
//! Hovering the frame bar's scrubber shows a small preview of the frame under
//! the cursor (`app::preview`). Building it is an ordinary tone render, just at a
//! nearest decimation that brings the longest side under [`THUMB_PX`] — so a
//! 4096² frame renders about 1/17 of its pixels — plus, for a frame whose bounds
//! aren't memoized yet, a whole-image percentile scan (see the render docs on
//! `media::percentile`). Neither belongs on the UI thread while the user sweeps
//! the timeline, so both run here.
//!
//! **Deliberately not the `RenderPool`.** That pool is one worker per pane with
//! at most one job in flight per pane, and its results gate the lock-step commit
//! (`app::decode::refresh_textures`); a preview queued there would sit behind the
//! pane's own base/region renders and could stall the timeline. A preview also
//! never runs the proprietary C++ operators — the pane's render thread owns
//! those instances, and a preview substitutes plain Linear for them — so it has
//! no reason to be pinned to that thread either. One shared worker, mirroring
//! `offsets::OffsetScanner` / `watcher::FileWatcher`, keeps the two paths
//! independent.

use std::collections::HashMap;
use std::sync::{mpsc, Arc};
use std::thread;

use eframe::egui;

use crate::media::{FrameData, Region, ToneLut};

/// Longest side, in texels, of a rendered thumbnail.
pub const THUMB_PX: usize = 240;

/// Most thumbnails kept resident. At most `THUMB_PX²` RGBA each (~230 KiB), so
/// this bounds the cache at ~29 MiB — small next to the frame cache, and enough
/// that sweeping back over a stretch of timeline just re-reads it.
const CACHE_CAP: usize = 128;

/// Identity of one thumbnail: the pane it came from, the source frame, and the
/// tone it was rendered with (`CimApp::tone_sig`, salted when the operator tone
/// was substituted — a preview must not be filed under a tone it didn't render).
pub type ThumbKey = (u64, usize, u64);

/// How a thumbnail's display bounds are obtained.
pub enum Window {
    /// Already known on the UI thread: "Share clip" locks the pane to the
    /// Control media's bounds, which are computed from the Control's *shown*
    /// frame and so are memoized already.
    Fixed(f32, f32),
    /// Computed here, from the pane's clip percentile and tone region. A frame
    /// the user has never displayed has no memoized bounds, so this is a
    /// whole-image percentile scan — exactly what must not run on the UI thread
    /// while the cursor sweeps the scrubber.
    Compute {
        clip: Option<f32>,
        region: Option<egui::Rect>,
    },
}

pub struct ThumbJob {
    pub key: ThumbKey,
    pub data: Arc<FrameData>,
    pub window: Window,
    /// The Colormap palette, when the pane renders through one.
    ///
    /// Deliberately these two fields rather than an `imageproc::Display`: that
    /// type carries the operator flags, and a preview never runs the operators.
    /// Not having them here is what stops one being set by accident.
    pub palette: Option<crate::palette::Palette>,
    /// Nearest decimation bringing the frame under `THUMB_PX` (see [`step_for`]).
    pub step: usize,
}

pub struct ThumbDone {
    pub key: ThumbKey,
    pub image: egui::ColorImage,
}

/// The decimation that brings `size`'s longest side to at most [`THUMB_PX`].
/// `div_ceil`, not a truncating divide: one texel over the target is harmless
/// here, but rounding the *other* way is how a "thumbnail" ends up full size.
pub fn step_for(size: [usize; 2]) -> usize {
    size[0].max(size[1]).div_ceil(THUMB_PX).max(1)
}

pub struct ThumbPool {
    job_tx: mpsc::Sender<ThumbJob>,
    done_rx: mpsc::Receiver<ThumbDone>,
}

impl ThumbPool {
    /// `ctx` is woken when a thumbnail lands, so the preview appears as soon as
    /// it is ready rather than on the next paced repaint (the pattern every
    /// worker here follows).
    pub fn new(ctx: egui::Context) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<ThumbJob>();
        let (done_tx, done_rx) = mpsc::channel::<ThumbDone>();
        thread::spawn(move || {
            // One table reused across jobs: consecutive previews of the same pane
            // share a window, so the 64 Ki entries are built once, not per frame
            // (the same reason each pane owns a `ToneLut`).
            let mut lut = ToneLut::default();
            while let Ok(job) = job_rx.recv() {
                let image = render(&job, &mut lut);
                if done_tx
                    .send(ThumbDone {
                        key: job.key,
                        image,
                    })
                    .is_err()
                {
                    break; // receiver dropped: the app is shutting down
                }
                ctx.request_repaint();
            }
        });
        Self { job_tx, done_rx }
    }

    pub fn request(&self, job: ThumbJob) {
        let _ = self.job_tx.send(job);
    }

    /// Take every finished thumbnail available right now (non-blocking).
    pub fn drain(&self) -> Vec<ThumbDone> {
        self.done_rx.try_iter().collect()
    }
}

/// The render itself: the same `render_lut` / `render_cmap` the pane uses, over a
/// decimated whole-image `Region`. Every dropped sample is still a true source
/// value (nearest, never a blend), so a preview is a decimation of the pane's own
/// render rather than a differently-computed image.
fn render(job: &ThumbJob, lut: &mut ToneLut) -> egui::ColorImage {
    // On the instance's budgeted pool, not rayon's machine-sized global one: both
    // the percentile scan and the render below split across cores (`crate::cpu`).
    crate::cpu::install(|| {
        let (lo, hi) = match job.window {
            Window::Fixed(lo, hi) => (lo, hi),
            Window::Compute { clip, region } => crate::tone::frame_bounds(&job.data, clip, region),
        };
        let region = Region::whole(job.data.size, job.step);
        let mut pixels: Vec<egui::Color32> = Vec::new();
        match job.palette {
            Some(pal) => job.data.render_cmap(lo, hi, region, pal, lut, &mut pixels),
            None => job.data.render_lut(lo, hi, region, lut, &mut pixels),
        }
        egui::ColorImage {
            size: region.out,
            pixels,
        }
    })
}

/// The finished thumbnails, as egui textures, under a plain capacity LRU.
///
/// A `Vec` recency list rather than the `BTreeSet<(tick, key)>` the frame and
/// region caches use: this one is bounded at [`CACHE_CAP`] entries and touched
/// once per frame, so linear scans over it are noise, and the budget is a count
/// rather than a byte total.
#[derive(Default)]
pub struct ThumbCache {
    map: HashMap<ThumbKey, egui::TextureHandle>,
    /// Least-recently-used first.
    order: Vec<ThumbKey>,
}

impl ThumbCache {
    pub fn contains(&self, key: &ThumbKey) -> bool {
        self.map.contains_key(key)
    }

    /// The thumbnail for `key`, marked most-recently-used.
    pub fn get(&mut self, key: &ThumbKey) -> Option<&egui::TextureHandle> {
        if !self.map.contains_key(key) {
            return None;
        }
        self.order.retain(|k| k != key);
        self.order.push(*key);
        self.map.get(key)
    }

    pub fn insert(&mut self, ctx: &egui::Context, key: ThumbKey, image: egui::ColorImage) {
        // Nearest at every zoom, as everywhere else in this tool.
        let tex = ctx.load_texture("thumb", image, egui::TextureOptions::NEAREST);
        if self.map.insert(key, tex).is_none() {
            self.order.push(key);
        }
        while self.order.len() > CACHE_CAP {
            let old = self.order.remove(0);
            self.map.remove(&old);
        }
    }

    /// Drop a pane's thumbnails on close or reload — they describe media that is
    /// gone or has been re-read, and nothing will ever ask for them again.
    pub fn forget_pane(&mut self, id: u64) {
        self.map.retain(|(pid, _, _), _| *pid != id);
        self.order.retain(|(pid, _, _)| *pid != id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A preview must be a *decimation of the pane's own render*, not a second
    /// tone path that could drift from it — the same equivalence rule the export
    /// and the adaptive region path are held to.
    #[test]
    fn a_thumbnail_is_the_decimated_pane_render() {
        use crate::media::{FrameData, Samples};
        let size = [700usize, 500usize];
        let px: Vec<u16> = (0..size[0] * size[1])
            .map(|i| (i * 7 % 4096) as u16)
            .collect();
        let frame = Arc::new(FrameData::new(size, 1, Samples::U16(px)));
        let step = step_for(size);
        let (lo, hi) = crate::tone::frame_bounds(&frame, Some(0.01), None);

        let job = ThumbJob {
            key: (0, 0, 0),
            data: frame.clone(),
            window: Window::Compute {
                clip: Some(0.01),
                region: None,
            },
            palette: None,
            step,
        };
        let got = render(&job, &mut ToneLut::default());

        let region = Region::whole(size, step);
        let mut want: Vec<egui::Color32> = Vec::new();
        frame.render_lut(lo, hi, region, &mut ToneLut::default(), &mut want);
        assert_eq!(got.size, region.out);
        assert_eq!(got.pixels, want);
    }

    #[test]
    fn step_brings_every_size_under_the_thumbnail_target() {
        for size in [[1, 1], [240, 100], [241, 100], [4096, 4096], [25000, 3]] {
            let s = step_for(size);
            let out = Region::whole(size, s).out;
            assert!(
                out[0].max(out[1]) <= THUMB_PX,
                "{size:?} at step {s} -> {out:?}"
            );
            // …and never one step more than needed (a step that still fits after
            // being reduced by one would render a needlessly small preview).
            if s > 1 {
                let up = Region::whole(size, s - 1).out;
                assert!(up[0].max(up[1]) > THUMB_PX, "{size:?} over-decimated");
            }
        }
    }
}
