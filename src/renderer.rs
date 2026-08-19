//! Background tone-render pool — **one worker thread per pane**.
//!
//! The LUT render plus the proprietary C++ operators (LUT_ALPHA,
//! DETAILS_ENHANCED) can be heavy, so panes that use them build their display
//! RGBA off the UI thread. Each pane gets its **own** worker thread, spawned
//! lazily on its first job and identified by the stable pane `id`. Two reasons:
//!
//! - **Parallelism.** Different panes render concurrently, so a grid of heavy
//!   panes updates together instead of one-at-a-time behind a single worker.
//! - **Ownership for the C++ operators.** The proprietary operators are
//!   media-specific class instances that are heavy to construct (keyed on the
//!   image dimensions) and are not assumed thread-safe. Pinning each pane's
//!   renders to one thread gives those instances a single owner — no locking,
//!   and no reliance on the proprietary code being reentrant. They will live in
//!   [`Worker`], built lazily and rebuilt when a frame's dimensions change.
//!
//! The UI submits a job for a pane's current `(frame, tone-signature)`, keeps
//! showing its last texture with a spinner, and uploads the finished RGBA when it
//! lands — mirroring how the decode pool keeps painting responsive. Jobs and
//! results are addressed by the stable pane `id`, so they still route correctly
//! after a reorder / close. `forget(id)` drops a pane's worker (on close /
//! reload): its channel closes, the thread exits, and its owned operator
//! instances are destroyed on that thread.

use rayon::iter::IndexedParallelIterator;
use std::collections::HashMap;
use std::sync::{mpsc, Arc};
use std::thread;

use crate::media::FrameData;

/// What a finished render becomes on the UI side — the app's routing decision,
/// carried through the pool so a result never has to be re-classified.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    /// The pane's whole-image base texture (`app::decode::upload_tex`).
    Base,
    /// One adaptive viewport region (`app::roi`). It lands in the region cache
    /// under the key the app filed in `roi_inflight` when it queued the job, so
    /// the key never has to be reconstructed from the geometry on the way back.
    Viewport,
}

pub struct RenderJob {
    pub id: u64,
    pub frame: usize,
    /// Signature of the tone parameters this render was built for; the UI uses it
    /// to tell a still-current texture from a stale one (see `CimApp::tone_sig`).
    pub sig: u64,
    pub data: Arc<FrameData>,
    /// How to map samples to pixels — window, Colormap palette, operators. The
    /// pool renders **every** live tone, Colormap included; leaving the palette
    /// out of the job is what made a large Colormap region come back grey.
    pub tone: crate::imageproc::Display,
    /// The sub-rect and decimation to render. A base job passes
    /// `Region::whole(data.size, step)` — `step` is 1 outside adaptive mode, and
    /// for an operator pane a decimated base means the operators run on the
    /// reduced input, by design. The adaptive path passes its viewport region.
    pub region: crate::media::Region,
    /// Where the result goes (echoed back on [`RenderDone`]).
    pub target: Target,
}

pub struct RenderDone {
    pub id: u64,
    pub frame: usize,
    pub sig: u64,
    /// The finished display image, rendered **directly** into egui's pixel type
    /// on the worker (see [`RgbaSink`]), so neither the conversion copy nor the
    /// texture-delta queueing costs the UI thread anything but a move.
    pub image: eframe::egui::ColorImage,
    /// The frame's native pixel size (the image above is smaller when decimated
    /// or a region) — what the pane's on-screen geometry is sized from.
    pub native: [usize; 2],
    /// The job's region, echoed back: its `step` is the texture's identity, and
    /// its `out` is `image.size`.
    pub region: crate::media::Region,
    /// The job's target, echoed back (see [`RenderJob::target`]).
    pub target: Target,
    /// LUT / tone map time (the gray or 8-bit render), for the `CIM_DEBUG` profiler.
    pub lut_time: std::time::Duration,
    /// Proprietary-operator `apply` time (zero when no operator ran).
    pub ops_time: std::time::Duration,
}

/// Render straight into egui's packed pixel type, skipping the RGBA-bytes
/// round trip (see [`crate::media::RgbaSink`]). Every pixel a render produces is
/// opaque, so `from_rgb` is exactly what `from_rgba_unmultiplied(r, g, b, 255)`
/// used to yield — the texture is byte-identical to the old two-step path.
impl crate::media::RgbaSink for Vec<eframe::egui::Color32> {
    #[inline]
    fn begin(&mut self, px: usize) {
        self.clear();
        self.reserve(px);
    }
    #[inline]
    fn push_rgb(&mut self, rgb: [u8; 3]) {
        self.push(eframe::egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]));
    }
    // One `Color32` per pixel, so a run maps 1:1 onto `Vec::extend` — see the
    // trait's note on why that beats per-pixel pushes.
    #[inline]
    fn extend_gray<I: Iterator<Item = u8>>(&mut self, it: I) {
        self.extend(it.map(eframe::egui::Color32::from_gray));
    }
    #[inline]
    fn extend_rgb<I: Iterator<Item = [u8; 3]>>(&mut self, it: I) {
        self.extend(it.map(|c| eframe::egui::Color32::from_rgb(c[0], c[1], c[2])));
    }

    // One `Color32` per pixel, so rayon can hand each task a disjoint slice of
    // the output addressed by pixel index — the precondition `par_gray` states.
    // `collect_into_vec` sizes this buffer once and lets the worker threads write
    // their own ranges into the spare capacity, so the parallelism covers the
    // *first touch* of those pages too, not just the tone map. That matters more
    // than the mapping itself here: a fresh full-resolution buffer is ~50 MB of
    // never-yet-faulted memory, and faulting it in is the larger half of a render
    // (see the `DEFAULT_MMAP_THRESHOLD_MAX` note in the render docs).
    fn par_gray<I: rayon::iter::IndexedParallelIterator<Item = u8>>(&mut self, it: I) -> bool {
        it.map(eframe::egui::Color32::from_gray)
            .collect_into_vec(self);
        true
    }
    fn par_rgb<I: rayon::iter::IndexedParallelIterator<Item = [u8; 3]>>(&mut self, it: I) -> bool {
        it.map(|c| eframe::egui::Color32::from_rgb(c[0], c[1], c[2]))
            .collect_into_vec(self);
        true
    }
}

pub struct RenderPool {
    /// One job channel per live pane id; the matching worker thread owns that
    /// pane's (future) operator instances. Spawned on first `request`, dropped by
    /// `forget` — dropping the sender makes the worker's `recv` fail so it exits.
    workers: HashMap<u64, mpsc::Sender<RenderJob>>,
    /// Cloned into each worker; results from every pane funnel back here.
    done_tx: mpsc::Sender<RenderDone>,
    done_rx: mpsc::Receiver<RenderDone>,
    /// Woken (`request_repaint`) when a render lands, so the lock-step commit
    /// runs the moment a heavy pane is ready rather than on the next paced
    /// repaint (which, during playback, is a whole frame interval away).
    ctx: eframe::egui::Context,
}

impl RenderPool {
    pub fn new(ctx: eframe::egui::Context) -> Self {
        let (done_tx, done_rx) = mpsc::channel::<RenderDone>();
        Self {
            workers: HashMap::new(),
            done_tx,
            done_rx,
            ctx,
        }
    }

    /// Submit a render for pane `job.id`, spawning that pane's worker thread on
    /// first use. The caller (`prepare`) keeps at most one job per pane in flight,
    /// so a pane's channel never backs up.
    pub fn request(&mut self, job: RenderJob) {
        let id = job.id;
        if !self.workers.contains_key(&id) {
            let (job_tx, job_rx) = mpsc::channel::<RenderJob>();
            let done_tx = self.done_tx.clone();
            let ctx = self.ctx.clone();
            thread::spawn(move || {
                // The worker owns this pane's render state (and, later, its
                // proprietary operator instances) for the life of the thread.
                let mut worker = Worker::default();
                while let Ok(job) = job_rx.recv() {
                    // Render on the budgeted rayon pool: the full-resolution LUT
                    // render splits across cores (`media::render`), and a bare
                    // `par_iter` would take rayon's machine-sized global pool
                    // instead of this instance's share (`crate::cpu`). Installed
                    // per job rather than around the loop so a budget change
                    // applies to the next render, not only to new panes.
                    let done = crate::cpu::install(|| worker.render(job));
                    if done_tx.send(done).is_err() {
                        break; // UI gone: shutting down
                    }
                    // Wake the UI to commit this render promptly (see `ctx`).
                    ctx.request_repaint();
                }
                // Channel closed (`forget` / shutdown): `worker` drops here, on
                // this thread, destroying its operator instances.
            });
            self.workers.insert(id, job_tx);
        }
        let _ = self.workers[&id].send(job);
    }

    /// Drop pane `id`'s worker: its thread finishes any in-progress job, then
    /// exits and destroys its operator instances. Called on pane close / reload
    /// so fresh contents (possibly new dimensions) get a fresh instance.
    pub fn forget(&mut self, id: u64) {
        self.workers.remove(&id);
    }

    /// Take every finished render available right now (non-blocking).
    pub fn drain(&self) -> Vec<RenderDone> {
        self.done_rx.try_iter().collect()
    }
}

/// Per-pane render worker state, owned solely by that pane's thread.
///
/// `ops` holds this pane's proprietary operator instances (LUT_ALPHA / details):
/// each is built lazily on the first job that needs it and rebuilt when a job's
/// image dimensions differ from the cached ones, so the heavy, size-dependent
/// construction is paid once and reused across that pane's frames. Because the
/// worker is the sole owner, the instances need no locking and are destroyed on
/// this thread when the pane's worker is dropped (`RenderPool::forget`).
#[derive(Default)]
struct Worker {
    ops: crate::imageproc::PaneOps,
    /// Cached value→display table, reused across this pane's frames (see
    /// [`crate::media::ToneLut`]) — the worker is the pane's single render thread.
    lut: crate::media::ToneLut,
}

impl Worker {
    /// The heavy part, run on a pane's worker thread: build the display RGBA (LUT
    /// render) and, for a single-channel 16-bit frame with the proprietary library
    /// loaded, apply the tone operators on a 16-bit render before downscaling to 8
    /// bits. Mirrors the live path in `app::decode::prepare` and the export path in
    /// `export::ensure_frame` so all three match pixel-for-pixel.
    fn render(&mut self, job: RenderJob) -> RenderDone {
        let native = job.data.size;
        let mut pixels = Vec::new();
        // The one shared render tail (plain LUT, or operators on a full-precision
        // 16-bit render) — identical to the export path by construction, which
        // renders the same tail into a byte buffer instead (see `RgbaSink`).
        // Whole image or region, full resolution or decimated, it is one call:
        // `Region` carries the difference.
        let (lut_time, ops_time) =
            self.ops
                .render_display(&job.data, job.tone, job.region, &mut self.lut, &mut pixels);
        let image = eframe::egui::ColorImage {
            size: job.region.out,
            pixels,
        };
        RenderDone {
            id: job.id,
            frame: job.frame,
            sig: job.sig,
            image,
            native,
            region: job.region,
            target: job.target,
            lut_time,
            ops_time,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{Region, Samples};

    /// The worker's output must equal the plain LUT render byte-for-byte when
    /// no proprietary library is loaded (the test environment) — including
    /// when the job *asks* for LUT_ALPHA / details, which is the documented
    /// fallback. This locks the live-render half of the "all render paths
    /// match pixel-for-pixel" invariant before the paths are unified.
    #[test]
    fn worker_render_matches_plain_lut_render() {
        let frame = Arc::new(FrameData::new(
            [8, 4],
            1,
            Samples::U16(crate::testutil::gray16_page(8, 4, 7)),
        ));
        let (lo, hi) = (500.0, 60000.0);
        let mut reference = Vec::new();
        frame.render_into(lo, hi, &mut reference);
        // The worker hands back an already-converted `ColorImage` (the
        // conversion copy runs off the UI thread); compare in that space.
        let reference = eframe::egui::ColorImage::from_rgba_unmultiplied([8, 4], &reference);

        let mut worker = Worker::default();
        for (lut_alpha, details) in [(false, false), (true, false), (false, true)] {
            let done = worker.render(RenderJob {
                id: 1,
                frame: 0,
                sig: 9,
                data: frame.clone(),
                tone: crate::imageproc::Display {
                    lo,
                    hi,
                    palette: None,
                    ops: crate::imageproc::Ops { lut_alpha, details },
                },
                region: Region::whole([8, 4], 1),
                target: Target::Base,
            });
            assert_eq!(done.image.size, [8, 4]);
            assert_eq!(
                done.image, reference,
                "lut_alpha={lut_alpha} details={details}"
            );
        }
    }

    /// A decimated / region job renders exactly the plain region render (no
    /// operator library in tests, so the documented fallback), with the result
    /// sized to the region and the native size echoed alongside — the contract
    /// the adaptive region cache and the base-commit identity both rely on.
    #[test]
    fn worker_region_render_matches_region_lut() {
        use crate::media::ToneLut;
        use eframe::egui::Color32;

        let frame = Arc::new(FrameData::new(
            [9, 6],
            1,
            Samples::U16(crate::testutil::gray16_page(9, 6, 11)),
        ));
        let (lo, hi) = (500.0, 60000.0);
        let region = Region {
            origin: [2, 2],
            out: [3, 2],
            step: 2,
        };
        let mut worker = Worker::default();
        let done = worker.render(RenderJob {
            id: 1,
            frame: 0,
            sig: 9,
            data: frame.clone(),
            tone: crate::imageproc::Display {
                lo,
                hi,
                palette: None,
                // Library absent → plain fallback, still the region path.
                ops: crate::imageproc::Ops {
                    lut_alpha: true,
                    details: false,
                },
            },
            region,
            target: Target::Viewport,
        });
        assert_eq!(done.image.size, [3, 2]);
        assert_eq!(done.native, [9, 6]);
        assert_eq!((done.region, done.target), (region, Target::Viewport));

        let mut reference = Vec::<Color32>::new();
        frame.render_lut(lo, hi, region, &mut ToneLut::default(), &mut reference);
        assert_eq!(done.image.pixels, reference);

        // A decimated whole-image job (an adaptive base) sizes to the decimated
        // grid and routes to the pane's base texture.
        let base = Region::whole([9, 6], 2);
        let done = worker.render(RenderJob {
            id: 1,
            frame: 0,
            sig: 9,
            data: frame.clone(),
            tone: crate::imageproc::Display {
                lo,
                hi,
                palette: None,
                ops: crate::imageproc::Ops::default(),
            },
            region: base,
            target: Target::Base,
        });
        assert_eq!(done.image.size, [5, 3]);
        assert_eq!((done.native, done.region.step), ([9, 6], 2));
        assert_eq!(done.target, Target::Base);
    }

    /// A **Colormap** job comes back false-coloured, whole-image or region.
    ///
    /// The reported bug: the pool carried no palette, so `render_display` fell
    /// through to the plain grey LUT. The live paths worked around it by keeping
    /// Colormap renders synchronous — but the adaptive region path had no such
    /// guard, so a region large enough to go off-thread
    /// (`ASYNC_RENDER_PIXELS`) rendered grey. That produced a *band* of zooms
    /// where a Colormap pane lost its palette: wide enough for the region to
    /// exceed 1 MP, but zoomed in enough for `roi_plan` to still engage. The band
    /// moved with `COVER`, which is why pausing and playing gave different edges.
    #[test]
    fn worker_renders_the_colormap_palette() {
        use crate::media::ToneLut;
        use crate::palette::Palette;
        use eframe::egui::Color32;

        let frame = Arc::new(FrameData::new(
            [9, 6],
            1,
            Samples::U16(crate::testutil::gray16_page(9, 6, 11)),
        ));
        let (lo, hi) = (500.0, 60000.0);
        let pal = Palette::Viridis;
        let tone = crate::imageproc::Display {
            lo,
            hi,
            palette: Some(pal),
            // A Colormap pane never runs operators; set them anyway to pin that
            // the palette branch wins regardless.
            ops: crate::imageproc::Ops {
                lut_alpha: true,
                details: true,
            },
        };
        let mut worker = Worker::default();

        for region in [
            Region::whole([9, 6], 1),
            Region::whole([9, 6], 2),
            Region {
                origin: [2, 2],
                out: [3, 2],
                step: 2,
            },
        ] {
            let done = worker.render(RenderJob {
                id: 1,
                frame: 0,
                sig: 9,
                data: frame.clone(),
                tone,
                region,
                target: Target::Viewport,
            });
            let mut want = Vec::<Color32>::new();
            frame.render_cmap(lo, hi, region, pal, &mut ToneLut::default(), &mut want);
            assert_eq!(done.image.pixels, want, "{region:?}");

            // And it is genuinely not the grey render — otherwise this test
            // would pass on the very bug it exists for.
            let mut grey = Vec::<Color32>::new();
            frame.render_lut(lo, hi, region, &mut ToneLut::default(), &mut grey);
            assert_ne!(done.image.pixels, grey, "{region:?} rendered grey");
        }
    }

    /// Past `PAR_MIN_PX` the `Vec<Color32>` sink renders across rayon's pool
    /// instead of serially. Splitting is by pixel index over a contiguous
    /// source, so it must reproduce the serial render exactly — the
    /// pixel-accuracy invariant does not bend for a faster path. Checked for
    /// both the grey map and the Colormap palette, at an image comfortably over
    /// the threshold (1024² = 1 Mpx) so the parallel branch really is taken.
    #[test]
    fn par_render_matches_serial_render() {
        use crate::media::ToneLut;
        use eframe::egui::Color32;

        let f = FrameData::new(
            [1024, 1024],
            1,
            Samples::U16(crate::testutil::gray16_page(1024, 1024, 3)),
        );
        let (lo, hi) = (700.0, 61000.0);

        // The byte sink has no parallel path, so it *is* the serial reference.
        let whole = Region::whole(f.size, 1);
        let mut serial = Vec::<u8>::new();
        f.render_lut(lo, hi, whole, &mut ToneLut::default(), &mut serial);

        let mut par = Vec::<Color32>::new();
        f.render_lut(lo, hi, whole, &mut ToneLut::default(), &mut par);
        assert_eq!(par.len() * 4, serial.len());
        for (i, (p, s)) in par.iter().zip(serial.chunks_exact(4)).enumerate() {
            assert_eq!(p.to_array(), s, "grey pixel {i}");
        }

        // Same for the Colormap tone, which takes the `par_rgb` branch.
        let pal = crate::palette::Palette::Viridis;
        let mut serial = Vec::<u8>::new();
        f.render_cmap(lo, hi, whole, pal, &mut ToneLut::default(), &mut serial);
        let mut par = Vec::<Color32>::new();
        f.render_cmap(lo, hi, whole, pal, &mut ToneLut::default(), &mut par);
        for (i, (p, s)) in par.iter().zip(serial.chunks_exact(4)).enumerate() {
            assert_eq!(p.to_array(), s, "colormap pixel {i}");
        }
    }
}
