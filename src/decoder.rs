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
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use anyhow::Result;

use crate::media::{self, DecodeReq, FrameData, SeqReader};

struct Job {
    id: u64,
    frame: usize,
    req: DecodeReq,
}

pub struct Done {
    pub id: u64,
    pub frame: usize,
    pub result: Result<Decoded>,
    /// Wall-clock spent reading + decoding this job (for the `CIM_DEBUG` profiler).
    pub elapsed: std::time::Duration,
}

/// The outcome of a job.
pub enum Decoded {
    /// A fully decoded frame (a normal decode).
    Frame(Arc<FrameData>),
    /// A metadata-only frontier probe confirmed the page exists but did not
    /// decode it — the caller grows the known length without a resident frame.
    Exists,
    /// The page was past the sequence end (a frontier probe/decode found
    /// nothing): a TIFF's real end, or a concatenation rolls to the next file.
    End,
}

/// Persistent readers, keyed by `(pane id, file index)`. A lone TIFF uses file
/// index 0; a concatenation keeps one reader per file so each file's IFD offset
/// cache stays warm. The outer mutex guards the map (held only briefly), the
/// inner one serialises decodes of one file.
type Readers = Arc<Mutex<HashMap<(u64, usize), Arc<Mutex<SeqReader>>>>>;

pub struct BackgroundDecoder {
    job_tx: mpsc::Sender<Job>,
    done_rx: mpsc::Receiver<Done>,
    readers: Readers,
}

impl BackgroundDecoder {
    /// `ctx` is woken (`request_repaint`) whenever a job finishes, so a landed
    /// frame is picked up (and, during render-gated playback, committed) the
    /// instant it's ready instead of on the next paced repaint — otherwise the
    /// gate waits up to a whole frame interval and playback runs at a fraction of
    /// the requested fps.
    pub fn new(threads: usize, ctx: eframe::egui::Context) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (done_tx, done_rx) = mpsc::channel::<Done>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let readers: Readers = Arc::new(Mutex::new(HashMap::new()));

        for _ in 0..threads.max(1) {
            let job_rx = Arc::clone(&job_rx);
            let done_tx = done_tx.clone();
            let readers = Arc::clone(&readers);
            let ctx = ctx.clone();
            thread::spawn(move || loop {
                // Hold the job lock only for the hand-off, then decode unlocked
                // so other workers can pick up queued jobs in parallel.
                let job = match job_rx.lock().unwrap().recv() {
                    Ok(job) => job,
                    Err(_) => break, // sender dropped: app is shutting down
                };

                let started = std::time::Instant::now();
                let result = match &job.req {
                    // Multi-page TIFF: decode (or, when `probe`, metadata-only
                    // check) `page` through the file's persistent reader (keyed
                    // by pane id + file) so seeks reuse cached IFD offsets.
                    DecodeReq::Tiff {
                        file,
                        page,
                        path,
                        probe,
                    } => {
                        let key = (job.id, *file);
                        let reader = {
                            let mut map = readers.lock().unwrap();
                            match map.get(&key) {
                                Some(r) => Ok(Arc::clone(r)),
                                None => SeqReader::open(path).map(|r| {
                                    let r = Arc::new(Mutex::new(r));
                                    map.insert(key, Arc::clone(&r));
                                    r
                                }),
                            }
                        };
                        match reader {
                            Ok(r) if *probe => r.lock().unwrap().probe(*page).map(|exists| {
                                if exists {
                                    Decoded::Exists
                                } else {
                                    Decoded::End
                                }
                            }),
                            Ok(r) => r.lock().unwrap().decode(*page).map(|f| match f {
                                Some(f) => Decoded::Frame(Arc::new(f)),
                                None => Decoded::End,
                            }),
                            Err(e) => Err(e),
                        }
                    }
                    // Numbered still sequence: each frame is its own file, so
                    // decode it standalone (no persistent reader to keep warm).
                    DecodeReq::File(path) => {
                        media::decode_file(path).map(|f| Decoded::Frame(Arc::new(f)))
                    }
                };
                if done_tx
                    .send(Done {
                        id: job.id,
                        frame: job.frame,
                        result,
                        elapsed: started.elapsed(),
                    })
                    .is_err()
                {
                    break;
                }
                // Wake the UI to drain this result promptly (see `new`).
                ctx.request_repaint();
            });
        }

        Self {
            job_tx,
            done_rx,
            readers,
        }
    }

    pub fn request(&self, id: u64, frame: usize, req: DecodeReq) {
        let _ = self.job_tx.send(Job { id, frame, req });
    }

    /// Drop every persistent reader for `id` (all of a concatenation's files) so
    /// the next decode reopens them. Call when a sequence is reloaded or removed.
    pub fn forget(&self, id: u64) {
        self.readers.lock().unwrap().retain(|(k, _), _| *k != id);
    }

    /// Take every finished frame available right now (non-blocking).
    pub fn drain(&self) -> Vec<Done> {
        self.done_rx.try_iter().collect()
    }
}
