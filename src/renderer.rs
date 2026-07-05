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

/// The heavy part, run on a worker: build the 8-bit RGBA (LUT render) then apply
/// the tone operators in place. Mirrors the live path in `app::decode::prepare`
/// and the export path in `export::ensure_frame` so all three match pixel-for-pixel.
fn render(job: RenderJob) -> RenderDone {
    let size = job.data.size;
    let [w, h] = size;
    let mut rgba = Vec::new();
    job.data.render_into(job.lo, job.hi, &mut rgba);
    if let Some(blend) = job.lut_blend {
        let blend = blend.clamp(0.0, 1.0);
        if blend >= 1.0 {
            crate::imageproc::lut_alpha(&mut rgba, w, h);
        } else {
            // Mix the operator's output back toward the plain linear image.
            let base = rgba.clone();
            crate::imageproc::lut_alpha(&mut rgba, w, h);
            blend_rgba(&mut rgba, &base, blend);
        }
    }
    if job.details {
        crate::imageproc::details_enhanced(&mut rgba, w, h);
    }
    RenderDone {
        id: job.id,
        frame: job.frame,
        sig: job.sig,
        size,
        rgba,
    }
}

/// Blend `out` toward `base` by `1 - t`: `out = t·out + (1 - t)·base` per byte.
fn blend_rgba(out: &mut [u8], base: &[u8], t: f32) {
    let t = t.clamp(0.0, 1.0);
    for (o, &b) in out.iter_mut().zip(base) {
        *o = (b as f32 * (1.0 - t) + *o as f32 * t).round().clamp(0.0, 255.0) as u8;
    }
}
