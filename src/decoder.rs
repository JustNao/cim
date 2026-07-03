//! Background decode pool.
//!
//! The UI thread submits `(pane id, frame, path)` jobs and drains finished
//! frames each update — decoding never blocks painting. Panes are addressed by
//! a stable `id` (not Vec index) so results still land correctly after the user
//! reorders or closes media.

use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use anyhow::Result;

use crate::media::{decode_tiff_page, FrameData};

struct Job {
    id: u64,
    frame: usize,
    path: PathBuf,
}

pub struct Done {
    pub id: u64,
    pub frame: usize,
    /// `Ok(Some)` decoded frame, `Ok(None)` the page was past the sequence end
    /// (a frontier probe that found nothing), `Err` a genuine decode failure.
    pub result: Result<Option<Arc<FrameData>>>,
}

pub struct BackgroundDecoder {
    job_tx: mpsc::Sender<Job>,
    done_rx: mpsc::Receiver<Done>,
}

impl BackgroundDecoder {
    pub fn new(threads: usize) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (done_tx, done_rx) = mpsc::channel::<Done>();
        let job_rx = Arc::new(Mutex::new(job_rx));

        for _ in 0..threads.max(1) {
            let job_rx = Arc::clone(&job_rx);
            let done_tx = done_tx.clone();
            thread::spawn(move || loop {
                // Hold the lock only for the hand-off, then decode unlocked so
                // other workers can pick up queued jobs in parallel.
                let job = match job_rx.lock().unwrap().recv() {
                    Ok(job) => job,
                    Err(_) => break, // sender dropped: app is shutting down
                };
                let result = decode_tiff_page(&job.path, job.frame).map(|f| f.map(Arc::new));
                if done_tx
                    .send(Done {
                        id: job.id,
                        frame: job.frame,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            });
        }

        Self { job_tx, done_rx }
    }

    pub fn request(&self, id: u64, frame: usize, path: PathBuf) {
        let _ = self.job_tx.send(Job { id, frame, path });
    }

    /// Take every finished frame available right now (non-blocking).
    pub fn drain(&self) -> Vec<Done> {
        self.done_rx.try_iter().collect()
    }
}
