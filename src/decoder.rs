//! Background decode pool.
//!
//! The UI thread submits `(pane id, frame, path)` jobs and drains finished
//! frames each update — decoding never blocks painting. Panes are addressed by
//! a stable `id` (not Vec index) so results still land correctly after the user
//! reorders or closes media.
//!
//! Each sequence keeps one persistent [`SeqReader`] (keyed by pane id) so
//! seeking to a page reuses the crate's cached IFD offsets instead of
//! re-walking the file every decode. Different sequences decode in parallel;
//! frames of the same sequence serialise on that sequence's reader (a single
//! file is read sequentially anyway).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use anyhow::Result;

use crate::media::{FrameData, SeqReader};

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

/// Persistent readers, one per sequence id. The outer mutex guards the map
/// (held only briefly), the inner one serialises decodes of one sequence.
type Readers = Arc<Mutex<HashMap<u64, Arc<Mutex<SeqReader>>>>>;

pub struct BackgroundDecoder {
    job_tx: mpsc::Sender<Job>,
    done_rx: mpsc::Receiver<Done>,
    readers: Readers,
}

impl BackgroundDecoder {
    pub fn new(threads: usize) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (done_tx, done_rx) = mpsc::channel::<Done>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let readers: Readers = Arc::new(Mutex::new(HashMap::new()));

        for _ in 0..threads.max(1) {
            let job_rx = Arc::clone(&job_rx);
            let done_tx = done_tx.clone();
            let readers = Arc::clone(&readers);
            thread::spawn(move || loop {
                // Hold the job lock only for the hand-off, then decode unlocked
                // so other workers can pick up queued jobs in parallel.
                let job = match job_rx.lock().unwrap().recv() {
                    Ok(job) => job,
                    Err(_) => break, // sender dropped: app is shutting down
                };

                // Fetch (or open) this sequence's persistent reader.
                let reader = {
                    let mut map = readers.lock().unwrap();
                    match map.get(&job.id) {
                        Some(r) => Ok(Arc::clone(r)),
                        None => SeqReader::open(&job.path).map(|r| {
                            let r = Arc::new(Mutex::new(r));
                            map.insert(job.id, Arc::clone(&r));
                            r
                        }),
                    }
                };
                let result = match reader {
                    Ok(r) => r.lock().unwrap().decode(job.frame).map(|f| f.map(Arc::new)),
                    Err(e) => Err(e),
                };
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

        Self {
            job_tx,
            done_rx,
            readers,
        }
    }

    pub fn request(&self, id: u64, frame: usize, path: PathBuf) {
        let _ = self.job_tx.send(Job { id, frame, path });
    }

    /// Drop the persistent reader for `id` so the next decode reopens the file.
    /// Call when a sequence is reloaded from disk or removed.
    pub fn forget(&self, id: u64) {
        self.readers.lock().unwrap().remove(&id);
    }

    /// Take every finished frame available right now (non-blocking).
    pub fn drain(&self) -> Vec<Done> {
        self.done_rx.try_iter().collect()
    }
}
