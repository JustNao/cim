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
    /// Whether to run LUT_ALPHA on the render (non-LUT_ALPHA tones and masks
    /// leave it off).
    pub lut_alpha: bool,
    pub details: bool,
}

pub struct RenderDone {
    pub id: u64,
    pub frame: usize,
    pub sig: u64,
    pub size: [usize; 2],
    pub rgba: Vec<u8>,
    /// LUT / tone map time (the gray or 8-bit render), for the `CIM_DEBUG` profiler.
    pub lut_time: std::time::Duration,
    /// Proprietary-operator `apply` time (zero when no operator ran).
    pub ops_time: std::time::Duration,
}

pub struct RenderPool {
    /// One job channel per live pane id; the matching worker thread owns that
    /// pane's (future) operator instances. Spawned on first `request`, dropped by
    /// `forget` — dropping the sender makes the worker's `recv` fail so it exits.
    workers: HashMap<u64, mpsc::Sender<RenderJob>>,
    /// Cloned into each worker; results from every pane funnel back here.
    done_tx: mpsc::Sender<RenderDone>,
    done_rx: mpsc::Receiver<RenderDone>,
}

impl Default for RenderPool {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderPool {
    pub fn new() -> Self {
        let (done_tx, done_rx) = mpsc::channel::<RenderDone>();
        Self {
            workers: HashMap::new(),
            done_tx,
            done_rx,
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
            thread::spawn(move || {
                // The worker owns this pane's render state (and, later, its
                // proprietary operator instances) for the life of the thread.
                let mut worker = Worker::default();
                while let Ok(job) = job_rx.recv() {
                    if done_tx.send(worker.render(job)).is_err() {
                        break; // UI gone: shutting down
                    }
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
}

impl Worker {
    /// The heavy part, run on a pane's worker thread: build the display RGBA (LUT
    /// render) and, for a single-channel 16-bit frame with the proprietary library
    /// loaded, apply the tone operators on a 16-bit render before downscaling to 8
    /// bits. Mirrors the live path in `app::decode::prepare` and the export path in
    /// `export::ensure_frame` so all three match pixel-for-pixel.
    fn render(&mut self, job: RenderJob) -> RenderDone {
        use std::time::{Duration, Instant};
        let size = job.data.size;
        let [w, h] = size;
        let mut rgba = Vec::new();
        let mut lut_time = Duration::ZERO;
        let mut ops_time = Duration::ZERO;
        // The proprietary operators run on a single-channel 16-bit render (so they
        // see full native precision) and only for single-channel 16-bit frames with
        // the library loaded. Everything else takes the plain 8-bit LUT render.
        let use_ops = job.data.is_op_input()
            && ((job.lut_alpha && crate::imageproc::lut_alpha_available())
                || (job.details && crate::imageproc::details_available()));
        if use_ops {
            let mut gray = Vec::new();
            let t = Instant::now();
            job.data.render_into_gray_u16(job.lo, job.hi, &mut gray);
            lut_time = t.elapsed();
            let t = Instant::now();
            self.ops.apply(&mut gray, w, h, job.lut_alpha, job.details);
            ops_time = t.elapsed();
            // Expand the processed grey back to 8-bit RGBA for the texture.
            rgba.resize(gray.len() * 4, 255);
            for (i, &s) in gray.iter().enumerate() {
                let g = (s >> 8) as u8;
                let o = i * 4;
                rgba[o] = g;
                rgba[o + 1] = g;
                rgba[o + 2] = g;
            }
        } else {
            let t = Instant::now();
            job.data.render_into(job.lo, job.hi, &mut rgba);
            lut_time = t.elapsed();
        }
        RenderDone {
            id: job.id,
            frame: job.frame,
            sig: job.sig,
            size,
            rgba,
            lut_time,
            ops_time,
        }
    }
}
