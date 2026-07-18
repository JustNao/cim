//! Background offset scanner.
//!
//! The **fast** offset discovery (`media::fastscan`) finds a regular TIFF
//! sequence's whole length by binary-searching each file's page count — a
//! handful of tiny header reads per file, but still **file I/O**, which we don't
//! want on the UI thread now that it runs automatically as soon as a media opens
//! (a many-file concatenation would hitch the paint).
//!
//! So it runs here instead: a single dedicated worker thread pulls scan jobs off
//! a queue, measures the page counts (never touching the pane's `Media` — it's
//! handed only the file paths), and posts the result back for the UI thread to
//! apply (`media::apply_offset_counts`). One thread is plenty: scans are cheap
//! and infrequent, and keeping it off the decode pool means an open's offset scan
//! never occupies a worker that should be decoding the first frame the user is
//! waiting to see. Results are keyed by pane `id` **and a generation**: the id is
//! stable across reload, so a scan still in flight when a pane is reloaded would
//! otherwise stamp stale counts onto the fresh media — the UI only applies a
//! result whose generation still matches the pane's latest request.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use crate::media;

struct ScanJob {
    id: u64,
    gen: u64,
    paths: Vec<PathBuf>,
}

/// A finished scan: the measured per-file page counts, or the reason the layout
/// isn't fast-scannable (in which case the caller simply leaves the sequence to
/// discover its length lazily, as before).
pub struct ScanDone {
    pub id: u64,
    pub gen: u64,
    pub result: Result<Vec<usize>, String>,
}

pub struct OffsetScanner {
    job_tx: Sender<ScanJob>,
    done_rx: Receiver<ScanDone>,
}

impl OffsetScanner {
    /// `ctx` is woken when a scan finishes so the UI drains and applies the
    /// counts on the next update (same pattern as the decode/render pools).
    pub fn new(ctx: eframe::egui::Context) -> Self {
        let (job_tx, job_rx) = channel::<ScanJob>();
        let (done_tx, done_rx) = channel::<ScanDone>();
        thread::spawn(move || {
            while let Ok(job) = job_rx.recv() {
                let result = media::scan_offset_counts(&job.paths);
                if done_tx
                    .send(ScanDone {
                        id: job.id,
                        gen: job.gen,
                        result,
                    })
                    .is_err()
                {
                    break; // receiver dropped: app is shutting down
                }
                ctx.request_repaint();
            }
        });
        Self { job_tx, done_rx }
    }

    /// Queue a scan of `paths` for pane `id` under generation `gen`. The result
    /// carries both back so a reload (which bumps `gen`) can reject a stale scan.
    pub fn request(&self, id: u64, gen: u64, paths: Vec<PathBuf>) {
        let _ = self.job_tx.send(ScanJob { id, gen, paths });
    }

    /// Take every finished scan available right now (non-blocking).
    pub fn drain(&self) -> Vec<ScanDone> {
        self.done_rx.try_iter().collect()
    }
}
