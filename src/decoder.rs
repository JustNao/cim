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
//! frames of the same sequence serialise on that sequence's reader **in the
//! order they were queued**, via a ticket per job ([`Slot`]): with a plain
//! mutex one worker can win the reader repeatedly while the frame playback
//! waits on starves (for a video, falling behind the stream costs a seek).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};
use std::thread;

use anyhow::Result;

use crate::media::{self, DecodeReq, FrameData, SeqReader, VideoReader};

struct Job {
    id: u64,
    frame: usize,
    req: DecodeReq,
    /// The cancellation epoch this job was queued under. A worker drops the job
    /// (without decoding) if `epoch` is older than the decoder's current epoch —
    /// how `cancel_pending` discards a whole queued backlog (e.g. the thousands
    /// of frames a "Load all" queues at once) the instant Stop is pressed.
    epoch: u64,
    /// The pane's **Scale** target when queued (`Media::scale_to`): a decoded
    /// frame is nearest-resampled to it here, on the worker, rather than on the
    /// UI thread when it lands.
    scale: Option<[usize; 2]>,
    /// Whole-frame display bounds to compute once decoded (`Some(clip)`, see
    /// `tone::frame_bounds`): they are memoized in the frame, so the UI's
    /// first render of it finds them ready instead of scanning the frame.
    bounds: Option<Option<f32>>,
}

pub struct Done {
    pub id: u64,
    pub frame: usize,
    /// The Scale target the frame was resampled to (`Job::scale`). The UI drops
    /// a frame whose target no longer matches the pane's — it would be the wrong
    /// size, and resampling a resample would compound the nearest pick.
    pub scale: Option<[usize; 2]>,
    pub result: Result<Decoded>,
    /// Wall-clock spent reading + decoding this job (for the `CIM_DEBUG` profiler).
    pub elapsed: std::time::Duration,
    /// The share of `elapsed` spent inside file `read`/`seek` calls (true I/O;
    /// the rest is CPU decompress). Only the persistent-reader TIFF path splits
    /// this out; a standalone `File` job reports zero.
    pub io: std::time::Duration,
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

/// Either persistent reader kind a pane's file can need: a TIFF's `SeqReader`
/// (warm IFD offsets) or a video's `VideoReader` (a streaming ffmpeg child). A
/// key only ever maps to one kind — reload/close call `forget` first.
// Boxing the larger variant would only move a per-open-file, long-lived reader
// (already behind an `Arc<Mutex<_>>`) behind one more pointer.
#[allow(clippy::large_enum_variant)]
enum Reader {
    Tiff(SeqReader),
    Video(VideoReader),
}

/// Persistent readers, keyed by `(pane id, file index)`. A lone TIFF or a video
/// uses file index 0; a concatenation keeps one reader per file so each file's
/// IFD offset cache stays warm. The map mutex is held only for the lookup.
type Readers = Arc<Mutex<HashMap<(u64, usize), Arc<Slot>>>>;

/// One file's persistent reader, used by one job at a time **in queue order**.
///
/// Each job takes a ticket when it leaves the queue (still under the queue
/// lock, so ticket order *is* queue order) and waits for its number. Playback
/// queues frames in the order it shows them, so the reader sees them in that
/// order too, whichever worker picked each up.
struct Slot {
    /// Tickets handed out so far.
    issued: AtomicU64,
    turn: Mutex<Turn>,
    cv: Condvar,
}

struct Turn {
    /// The ticket allowed to use the reader now.
    serving: u64,
    /// Opened by the first job to need it; an open that fails is retried by the
    /// next job.
    reader: Option<Reader>,
}

impl Slot {
    /// Block until `ticket` is served, then hold the reader.
    fn wait_turn(&self, ticket: u64) -> Held<'_> {
        let mut turn = self.turn.lock().unwrap();
        while turn.serving != ticket {
            turn = self.cv.wait(turn).unwrap();
        }
        Held {
            slot: self,
            turn: Some(turn),
        }
    }
}

/// A job's turn at a [`Slot`]'s reader. Dropping it — however the job ends —
/// passes the reader to the next ticket, so one failed decode can't wedge the
/// file's queue.
struct Held<'a> {
    slot: &'a Slot,
    turn: Option<MutexGuard<'a, Turn>>,
}

impl Held<'_> {
    /// The reader, opened with `open` if this is its first use.
    fn reader(&mut self, open: impl FnOnce() -> Result<Reader>) -> Result<&mut Reader> {
        let turn = self.turn.as_mut().expect("held until dropped");
        if turn.reader.is_none() {
            turn.reader = Some(open()?);
        }
        Ok(turn.reader.as_mut().expect("just opened"))
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        if let Some(mut turn) = self.turn.take() {
            turn.serving += 1;
            drop(turn);
            self.slot.cv.notify_all();
        }
    }
}

/// The persistent reader a job decodes through, or `None` for a standalone
/// file (a numbered still run's frame).
fn reader_key(id: u64, req: &DecodeReq) -> Option<(u64, usize)> {
    match req {
        DecodeReq::Tiff { file, .. } => Some((id, *file)),
        DecodeReq::Video { .. } => Some((id, 0)),
        DecodeReq::File(_) => None,
    }
}

/// Take the next ticket for `key`'s reader, creating its (empty) slot on first
/// use. Called under the job-queue lock.
fn take_ticket(readers: &Readers, key: (u64, usize)) -> (Arc<Slot>, u64) {
    let slot = Arc::clone(readers.lock().unwrap().entry(key).or_insert_with(|| {
        Arc::new(Slot {
            issued: AtomicU64::new(0),
            turn: Mutex::new(Turn {
                serving: 0,
                reader: None,
            }),
            cv: Condvar::new(),
        })
    }));
    let ticket = slot.issued.fetch_add(1, Ordering::Relaxed);
    (slot, ticket)
}

pub struct BackgroundDecoder {
    job_tx: mpsc::Sender<Job>,
    done_rx: mpsc::Receiver<Done>,
    readers: Readers,
    /// Bumped by `cancel_pending`; workers skip any job queued under an older
    /// epoch. Jobs stamp the value at submit time (see `request`).
    epoch: Arc<AtomicU64>,
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
        let epoch = Arc::new(AtomicU64::new(0));

        for _ in 0..threads.max(1) {
            let job_rx = Arc::clone(&job_rx);
            let done_tx = done_tx.clone();
            let readers = Arc::clone(&readers);
            let epoch = Arc::clone(&epoch);
            let ctx = ctx.clone();
            thread::spawn(move || loop {
                // Hold the job lock only for the hand-off (and the ticket, so
                // the reader serves jobs in queue order), then decode unlocked
                // so other workers can pick up queued jobs in parallel.
                let (job, ticket) = {
                    let rx = job_rx.lock().unwrap();
                    let job = match rx.recv() {
                        Ok(job) => job,
                        Err(_) => break, // sender dropped: app is shutting down
                    };
                    // A cancelled backlog (Stop / new load) bumps the epoch: drop
                    // the stale job without decoding or reporting it (and before
                    // it takes a ticket, which would then have to be served). The
                    // UI clears `inflight` in step, so a still-wanted frame is
                    // simply re-queued.
                    if job.epoch < epoch.load(Ordering::Relaxed) {
                        continue;
                    }
                    let ticket = reader_key(job.id, &job.req).map(|k| take_ticket(&readers, k));
                    (job, ticket)
                };
                let slot = |t: &Option<(Arc<Slot>, u64)>| {
                    let (slot, n) = t.as_ref().expect("a reader-backed job takes a ticket");
                    (Arc::clone(slot), *n)
                };

                let mut started = std::time::Instant::now();
                let mut io = std::time::Duration::ZERO;
                let result = match &job.req {
                    // Multi-page TIFF: decode (or, when `probe`, metadata-only
                    // check) `page` through the file's persistent reader (keyed
                    // by pane id + file) so seeks reuse cached IFD offsets.
                    DecodeReq::Tiff {
                        page, path, probe, ..
                    } => {
                        let (slot, n) = slot(&ticket);
                        let mut held = slot.wait_turn(n);
                        match (
                            held.reader(|| SeqReader::open(path).map(Reader::Tiff)),
                            *probe,
                        ) {
                            (Ok(Reader::Tiff(reader)), true) => reader.probe(*page).map(|exists| {
                                if exists {
                                    Decoded::Exists
                                } else {
                                    Decoded::End
                                }
                            }),
                            (Ok(Reader::Tiff(reader)), false) => {
                                reader.take_io(); // clear residue from prior probes
                                let res = reader.decode(*page).map(|f| match f {
                                    Some(f) => Decoded::Frame(Arc::new(f)),
                                    None => Decoded::End,
                                });
                                io = reader.take_io(); // this decode's file-I/O share
                                res
                            }
                            (Ok(_), _) => Err(anyhow::anyhow!("reader kind mismatch")),
                            (Err(e), _) => Err(e),
                        }
                    }
                    // Video frame: decode through the file's persistent
                    // streaming ffmpeg reader (sequential requests read straight
                    // off the pipe; a jump respawns the child with a seek).
                    DecodeReq::Video { path, frame } => {
                        let (slot, n) = slot(&ticket);
                        let mut held = slot.wait_turn(n);
                        // Time the decode, not the wait for this one reader: a
                        // video decodes strictly one frame at a time, and
                        // counting the queue inflated the latency the prefetch
                        // depth is sized from.
                        started = std::time::Instant::now();
                        match held.reader(|| VideoReader::open(path).map(Reader::Video)) {
                            Ok(Reader::Video(reader)) => reader.decode(*frame).map(|f| match f {
                                Some(f) => Decoded::Frame(f),
                                None => Decoded::End,
                            }),
                            Ok(_) => Err(anyhow::anyhow!("reader kind mismatch")),
                            Err(e) => Err(e),
                        }
                    }
                    // Numbered still sequence: each frame is its own file, so
                    // decode it standalone (no persistent reader to keep warm).
                    DecodeReq::File(path) => {
                        media::decode_file(path).map(|f| Decoded::Frame(Arc::new(f)))
                    }
                };
                let result = match (result, job.scale) {
                    (Ok(Decoded::Frame(f)), Some(size)) if f.size != size => {
                        Ok(Decoded::Frame(Arc::new(f.resample_nearest(size))))
                    }
                    (r, _) => r,
                };
                if let (Ok(Decoded::Frame(f)), Some(clip)) = (&result, job.bounds) {
                    crate::cpu::install(|| crate::tone::frame_bounds(f, clip, None));
                }
                if done_tx
                    .send(Done {
                        id: job.id,
                        frame: job.frame,
                        scale: job.scale,
                        result,
                        elapsed: started.elapsed(),
                        io,
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
            epoch,
        }
    }

    /// Queue `req` for pane `id`'s `frame`; a decoded frame is resampled to
    /// `scale` (the pane's Scale target) and its `bounds` computed before it is
    /// handed back.
    pub fn request(
        &self,
        id: u64,
        frame: usize,
        req: DecodeReq,
        scale: Option<[usize; 2]>,
        bounds: Option<Option<f32>>,
    ) {
        let epoch = self.epoch.load(Ordering::Relaxed);
        let _ = self.job_tx.send(Job {
            id,
            frame,
            req,
            epoch,
            scale,
            bounds,
        });
    }

    /// Discard every job queued so far (workers skip anything older than the new
    /// epoch). A job already mid-decode still lands — harmless, since a landed
    /// frame is always valid; the caller clears `inflight` so wanted frames are
    /// re-queued under the new epoch. This is how Stop halts a "Load all" whose
    /// entire backlog is already sitting in the queue.
    pub fn cancel_pending(&self) {
        self.epoch.fetch_add(1, Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A file's jobs use its reader in ticket (= queue) order, however the
    /// workers holding them are scheduled: here every later ticket is already
    /// waiting before the first one arrives, the worst case for a plain mutex.
    #[test]
    fn a_reader_serves_jobs_in_queue_order() {
        let readers: Readers = Arc::new(Mutex::new(HashMap::new()));
        let tickets: Vec<_> = (0..8).map(|_| take_ticket(&readers, (1, 0))).collect();
        let order = Arc::new(Mutex::new(Vec::new()));
        let workers: Vec<_> = tickets
            .into_iter()
            .rev() // the last-queued job reaches the reader first
            .map(|(slot, n)| {
                let order = Arc::clone(&order);
                let w = thread::spawn(move || {
                    let _held = slot.wait_turn(n);
                    order.lock().unwrap().push(n);
                });
                thread::sleep(std::time::Duration::from_millis(5));
                w
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        assert_eq!(*order.lock().unwrap(), (0..8).collect::<Vec<u64>>());
        // Another file's queue is independent.
        assert_eq!(take_ticket(&readers, (1, 1)).1, 0);
    }
}
