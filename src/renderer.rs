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
//!   and no reliance on the proprietary code being reentrant. They live in
//!   [`Worker`], built lazily and rebuilt when a frame's dimensions change.
//!
//! **The GPU tone map runs here too** ([`How::Gpu`]), for the first reason and
//! for a plainer one: it is not free. The compute dispatch is asynchronous, but
//! uploading a frame's samples to VRAM is a multi-megabyte memcpy, and on the UI
//! thread that landed inside `update` and cost more frame time than the faster
//! tone map saved. On a worker it overlaps with everything else — and with the
//! *other* panes' uploads, since `gpu::GpuToneMapper` is shared by `&self` and
//! only its per-pane half ([`gpu::PaneTone`]) lives in the worker, next to the
//! CPU path's `ToneLut` and operator instances for exactly the same reason. The
//! GPU's own execution is still serialised by the one device; the uploads, which
//! are what cost, are not.
//!
//! The UI submits a job for a pane's current `(frame, tone-signature)`, keeps
//! showing its last committed texture, and takes the result when it lands —
//! mirroring how the decode pool keeps painting responsive. Jobs and results are
//! addressed by the stable pane `id`, so they still route correctly after a
//! reorder / close. `forget(id)` drops a pane's worker (on close / reload): its
//! channel closes, the thread exits, and its owned operator instances and
//! uploaded display table are destroyed on that thread — which is why neither
//! has a separate "forget" of its own.

use rayon::iter::IndexedParallelIterator;
use std::collections::HashMap;
use std::sync::{mpsc, Arc};
use std::thread;

use crate::media::FrameData;

pub struct RenderJob {
    pub id: u64,
    pub frame: usize,
    /// Signature of the tone parameters this render was built for; the UI uses it
    /// to tell a still-current texture from a stale one (see `CimApp::tone_sig`).
    pub sig: u64,
    pub data: Arc<FrameData>,
    /// Linear display bounds `[lo, hi] → [0, 255]`, computed on the UI thread.
    pub lo: f32,
    pub hi: f32,
    /// Which processor builds this render, and what it needs to do it.
    pub how: How,
}

/// How one job is rendered — the CPU tail (with the proprietary operators) or
/// the GPU tone map.
///
/// The choice is the UI thread's (`CimApp::stage`), because only it knows
/// whether the app is in GPU mode and whether this pane is eligible. The worker
/// just does as it is told, which keeps the two paths' *dispatch* identical: one
/// pool, one in-flight cap, one `pending` slot, one lock-step commit.
pub enum How {
    Cpu {
        /// Whether to run LUT_ALPHA on the render (non-LUT_ALPHA tones and masks
        /// leave it off).
        lut_alpha: bool,
        details: bool,
    },
    Gpu {
        gpu: Arc<crate::gpu::GpuContext>,
        /// egui's render state, so a new texture can be registered from here.
        rs: eframe::egui_wgpu::RenderState,
        mapper: Arc<crate::gpu::GpuToneMapper>,
        palette: Option<crate::palette::Palette>,
        /// The pane's previous GPU texture, handed back for reuse when it is the
        /// right size — the same recycling the CPU path does through `pending`,
        /// so a playback run neither reallocates the texture nor re-registers
        /// its id with egui.
        recycle: Option<Box<crate::gpu::GpuTex>>,
    },
}

pub struct RenderDone {
    pub id: u64,
    pub frame: usize,
    pub sig: u64,
    pub out: Out,
    /// LUT / tone map time (the gray or 8-bit render), for the `CIM_DEBUG` profiler.
    pub lut_time: std::time::Duration,
    /// Proprietary-operator `apply` time (zero when no operator ran).
    pub ops_time: std::time::Duration,
}

/// What a finished job hands back.
pub enum Out {
    /// The finished display image, rendered **directly** into egui's pixel type
    /// on the worker (see [`RgbaSink`]), so neither the conversion copy nor the
    /// texture-delta queueing costs the UI thread anything but a move.
    Cpu(eframe::egui::ColorImage),
    /// A texture egui already knows about, holding pixels that never left the
    /// card. The UI thread only parks it in the pane's `pending` slot.
    Gpu(Box<crate::gpu::GpuTex>),
    /// The GPU refused the work (frame past the device's limits, device lost).
    /// The UI thread drops the GPU context on seeing this, and every later
    /// render — including this pane's re-request — goes to the CPU.
    GpuFailed(crate::gpu::GpuError),
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
                    //
                    // A GPU job is installed the same way for uniformity; it
                    // does no rayon work, so the wrapper costs it nothing.
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
    /// The GPU counterpart of `lut`: this pane's uploaded display table and the
    /// scratch it is built in. Same reasoning, same owner — see
    /// [`crate::gpu::PaneTone`].
    tone: crate::gpu::PaneTone,
}

impl Worker {
    /// One job, on the pane's own thread.
    fn render(&mut self, job: RenderJob) -> RenderDone {
        let (id, frame, sig) = (job.id, job.frame, job.sig);
        let (out, lut_time, ops_time) = match job.how {
            How::Cpu { lut_alpha, details } => {
                self.render_cpu(&job.data, job.lo, job.hi, lut_alpha, details)
            }
            How::Gpu {
                gpu,
                rs,
                mapper,
                palette,
                recycle,
            } => self.render_gpu(
                &job.data, job.lo, job.hi, &gpu, &rs, &mapper, palette, recycle,
            ),
        };
        RenderDone {
            id,
            frame,
            sig,
            out,
            lut_time,
            ops_time,
        }
    }

    /// The heavy CPU part: build the display RGBA (LUT render) and, for a
    /// single-channel 16-bit frame with the proprietary library loaded, apply the
    /// tone operators on a 16-bit render before downscaling to 8 bits. Mirrors the
    /// export path in `export::ensure_frame` so both match pixel-for-pixel.
    fn render_cpu(
        &mut self,
        data: &FrameData,
        lo: f32,
        hi: f32,
        lut_alpha: bool,
        details: bool,
    ) -> (Out, std::time::Duration, std::time::Duration) {
        let size = data.size;
        let mut pixels = Vec::new();
        // The one shared render tail (plain LUT, or operators on a full-precision
        // 16-bit render) — identical to the export path by construction, which
        // renders the same tail into a byte buffer instead (see `RgbaSink`).
        let (lut_time, ops_time) = self.ops.render_display(
            data,
            (lo, hi),
            lut_alpha,
            details,
            &mut self.lut,
            &mut pixels,
        );
        (
            Out::Cpu(eframe::egui::ColorImage { size, pixels }),
            lut_time,
            ops_time,
        )
    }

    /// The GPU tone map, run **here** rather than on the UI thread.
    ///
    /// This is the whole point of routing GPU renders through the pool: the work
    /// is not free just because the dispatch is asynchronous — uploading a new
    /// frame's samples is a multi-megabyte memcpy, which on the UI thread landed
    /// squarely inside `update` and cost more in frame time than the faster tone
    /// map saved. Here it overlaps with everything else, and — because the mapper
    /// takes `&self` — with the *other panes'* uploads.
    #[allow(clippy::too_many_arguments)]
    fn render_gpu(
        &mut self,
        data: &FrameData,
        lo: f32,
        hi: f32,
        gpu: &crate::gpu::GpuContext,
        rs: &eframe::egui_wgpu::RenderState,
        mapper: &crate::gpu::GpuToneMapper,
        palette: Option<crate::palette::Palette>,
        recycle: Option<Box<crate::gpu::GpuTex>>,
    ) -> (Out, std::time::Duration, std::time::Duration) {
        let zero = std::time::Duration::ZERO;
        let t = std::time::Instant::now();
        // Reuse the pane's previous texture when it still fits this frame.
        let mut tex = match recycle {
            Some(g) if g.size() == data.size => g,
            _ => match crate::gpu::GpuTex::new(rs, gpu, data.size) {
                Ok(g) => Box::new(g),
                Err(e) => return (Out::GpuFailed(e), zero, zero),
            },
        };
        let tone = crate::gpu::Tone { lo, hi, palette };
        let crate::gpu::GpuTex { out, tex: t2, .. } = &mut *tex;
        match mapper.tone(gpu, &mut self.tone, data, tone, out, Some(&*t2)) {
            Ok(()) => (Out::Gpu(tex), t.elapsed(), zero),
            Err(e) => (Out::GpuFailed(e), zero, zero),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::Samples;

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
                lo,
                hi,
                how: How::Cpu { lut_alpha, details },
            });
            let Out::Cpu(image) = done.out else {
                panic!("a CPU job must come back as a CPU image");
            };
            assert_eq!(image.size, [8, 4]);
            assert_eq!(image, reference, "lut_alpha={lut_alpha} details={details}");
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
        let mut serial = Vec::<u8>::new();
        f.render_into_lut(lo, hi, &mut ToneLut::default(), &mut serial);

        let mut par = Vec::<Color32>::new();
        f.render_into_lut(lo, hi, &mut ToneLut::default(), &mut par);
        assert_eq!(par.len() * 4, serial.len());
        for (i, (p, s)) in par.iter().zip(serial.chunks_exact(4)).enumerate() {
            assert_eq!(p.to_array(), s, "grey pixel {i}");
        }

        // Same for the Colormap tone, which takes the `par_rgb` branch.
        let pal = crate::palette::Palette::Viridis;
        let mut serial = Vec::<u8>::new();
        f.render_into_scaled_cmap(
            lo,
            hi,
            1,
            pal.table(),
            pal.id(),
            &mut ToneLut::default(),
            &mut serial,
        );
        let mut par = Vec::<Color32>::new();
        f.render_into_scaled_cmap(
            lo,
            hi,
            1,
            pal.table(),
            pal.id(),
            &mut ToneLut::default(),
            &mut par,
        );
        for (i, (p, s)) in par.iter().zip(serial.chunks_exact(4)).enumerate() {
            assert_eq!(p.to_array(), s, "colormap pixel {i}");
        }
    }
}
