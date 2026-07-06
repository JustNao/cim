//! Background tone-render pool.
//!
//! The LUT render plus the proprietary C++ operators (LUT_ALPHA,
//! DETAILS_ENHANCED) can be heavy, so panes that use them build their display
//! RGBA on this pool instead of the UI thread. The UI submits a job for a pane's
//! current `(frame, tone-signature)`, keeps showing its last texture with a
//! spinner, and uploads the finished RGBA when it lands — mirroring how the
//! decode pool keeps painting responsive. Jobs are addressed by the stable pane
//! `id`, so results still route correctly after a reorder / close.

use std::sync::{mpsc, Arc, Mutex};
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
    /// `Some(blend)` runs LUT_ALPHA then mixes back toward the plain linear image
    /// (`blend` = the operator's weight); `None` skips it (non-LUT_ALPHA tones and
    /// masks).
    pub lut_blend: Option<f32>,
    pub details: bool,
}

pub struct RenderDone {
    pub id: u64,
    pub frame: usize,
    pub sig: u64,
    pub size: [usize; 2],
    pub rgba: Vec<u8>,
}

pub struct RenderPool {
    job_tx: mpsc::Sender<RenderJob>,
    done_rx: mpsc::Receiver<RenderDone>,
}

impl RenderPool {
    pub fn new(threads: usize) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<RenderJob>();
        let (done_tx, done_rx) = mpsc::channel::<RenderDone>();
        let job_rx = Arc::new(Mutex::new(job_rx));

        for _ in 0..threads.max(1) {
            let job_rx = Arc::clone(&job_rx);
            let done_tx = done_tx.clone();
            thread::spawn(move || loop {
                // Hold the lock only for the hand-off, then render unlocked so
                // other workers process queued jobs (different panes) in parallel.
                let job = match job_rx.lock().unwrap().recv() {
                    Ok(j) => j,
                    Err(_) => break, // sender dropped: app is shutting down
                };
                if done_tx.send(render(job)).is_err() {
                    break;
                }
            });
        }

        Self { job_tx, done_rx }
    }

    pub fn request(&self, job: RenderJob) {
        let _ = self.job_tx.send(job);
    }

    /// Take every finished render available right now (non-blocking).
    pub fn drain(&self) -> Vec<RenderDone> {
        self.done_rx.try_iter().collect()
    }
}

/// The heavy part, run on a worker: build the display RGBA (LUT render) and,
/// for a 16-bit frame with the proprietary library loaded, apply the tone
/// operators on a 16-bit render before downscaling to 8 bits. Mirrors the live
/// path in `app::decode::prepare` and the export path in `export::ensure_frame`
/// so all three match pixel-for-pixel.
fn render(job: RenderJob) -> RenderDone {
    let size = job.data.size;
    let [w, h] = size;
    let mut rgba = Vec::new();
    // The proprietary operators run on a 16-bit render (so they see full native
    // precision) and only for 16-bit frames with the library loaded. Everything
    // else takes the plain 8-bit LUT render.
    let use_ops = crate::imageproc::is_available()
        && job.data.is_u16()
        && (job.lut_blend.is_some() || job.details);
    if use_ops {
        let mut buf16 = Vec::new();
        job.data.render_into_u16(job.lo, job.hi, &mut buf16);
        crate::imageproc::apply_operators(&mut buf16, w, h, job.lut_blend, job.details);
        rgba.resize(buf16.len(), 0);
        for (o, &s) in rgba.iter_mut().zip(&buf16) {
            *o = (s >> 8) as u8;
        }
    } else {
        job.data.render_into(job.lo, job.hi, &mut rgba);
    }
    RenderDone {
        id: job.id,
        frame: job.frame,
        sig: job.sig,
        size,
        rgba,
    }
}
