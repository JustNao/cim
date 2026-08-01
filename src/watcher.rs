//! Background source-file signer for the auto-reload watch.
//!
//! Signing a watched source is **file I/O** — a `stat` per file plus, for a small
//! source, a bounded strided read of its bytes (see `sign_paths`) — so it must not
//! run on the UI thread: on a network share a single signature can cost tens of
//! milliseconds, and the watch used to pay that on *every* repaint, which made
//! panning and zooming hitch while a pane was watching.
//!
//! So it runs here instead, mirroring `offsets::OffsetScanner`: one dedicated
//! worker thread pulls sign jobs off a queue, hashes the files (it is handed only
//! the paths — never the pane's `Media` or `Source`), and posts the signature back
//! for the UI thread to compare against its baseline. Results are keyed by pane
//! `id` **and a generation**, because the id is stable across reload: a signature
//! still in flight when the pane is reloaded (or its toggle flipped) would
//! otherwise be adopted as the baseline for contents it didn't measure, so the UI
//! only accepts a result whose generation still matches the pane's request.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

/// Identity of a source's on-disk contents for change detection: a hash folding
/// each file's length, mtime and (for a small source) a bounded sample of its
/// bytes, plus the total length. The byte sample is what catches a change when
/// the mtime doesn't move — e.g. a single multi-page TIFF written via `mmap`,
/// whose mtime Linux may not bump until the dirty pages flush.
pub type FileSig = (u64, u64);

/// Source-file sampling for `sign_paths`: total bytes read per file each poll,
/// split into this many evenly-spaced windows. Bounded so a multi-GB TIFF is only
/// touched a few KiB per poll (never bulk-read/hashed), while still catching an
/// in-place overwrite the mtime hasn't reflected yet.
const SAMPLE_BYTES: u64 = 64 * 1024;
const SAMPLE_WINDOWS: u64 = 16;
/// Only sample bytes when the source is at most this many files (the single- or
/// few-file case). A long numbered run stays on the cheap length+mtime path — its
/// per-frame files are written normally, so their mtime moves.
const SAMPLE_MAX_FILES: usize = 4;

struct SignJob {
    id: u64,
    gen: u64,
    paths: Vec<PathBuf>,
}

/// A finished signature. `sig` is `None` when a file couldn't be read right now
/// (e.g. mid-rename): the watch then simply waits for the next poll rather than
/// acting on torn contents.
pub struct SignDone {
    pub id: u64,
    pub gen: u64,
    pub sig: Option<FileSig>,
}

pub struct FileWatcher {
    job_tx: Sender<SignJob>,
    done_rx: Receiver<SignDone>,
}

impl FileWatcher {
    /// `ctx` is woken when a signature lands so the UI compares it on the next
    /// update (same pattern as the decode/render pools and the offset scanner).
    pub fn new(ctx: eframe::egui::Context) -> Self {
        let (job_tx, job_rx) = channel::<SignJob>();
        let (done_tx, done_rx) = channel::<SignDone>();
        thread::spawn(move || {
            while let Ok(job) = job_rx.recv() {
                let sig = sign_paths(&job.paths);
                if done_tx
                    .send(SignDone {
                        id: job.id,
                        gen: job.gen,
                        sig,
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

    /// Queue a signature of `paths` for pane `id` under generation `gen`.
    pub fn request(&self, id: u64, gen: u64, paths: Vec<PathBuf>) {
        let _ = self.job_tx.send(SignJob { id, gen, paths });
    }

    /// Take every finished signature available right now (non-blocking).
    pub fn drain(&self) -> Vec<SignDone> {
        self.done_rx.try_iter().collect()
    }
}

/// On-disk signature of a watched source: the total byte length and latest mtime
/// across its file(s), **plus a small strided sample of the file bytes**. `None`
/// when any file can't be read right now.
///
/// Why the content sample, not mtime alone: the common auto-reload case is a tool
/// overwriting a **single multi-page TIFF in place** with the same dimensions
/// (e.g. `tifffile.memmap`). The byte length doesn't change, and an `mmap`'d
/// writer often doesn't bump the mtime until its dirty pages flush, so an
/// `(mtime, len)` signature can stay identical while the pixels change. A `read()`
/// sees the new bytes immediately (same page cache), so sampling a few windows
/// catches it. The sample is **bounded** (`SAMPLE_BYTES` per file, spread across
/// the file) so a huge TIFF is only touched a few KiB per poll, never bulk-read.
/// It's applied only when the source is **one or a few files**; a long numbered
/// run stays on the cheap metadata path (those frames are written normally, so
/// their mtime moves and length/mtime alone suffice).
pub fn sign_paths(paths: &[PathBuf]) -> Option<FileSig> {
    use std::hash::Hasher;
    use std::io::{Read, Seek, SeekFrom};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut total = 0u64;
    // Only sample bytes for a small source (the single-TIFF case); a long run
    // would cost one open+read per file each poll for no benefit.
    let sample = paths.len() <= SAMPLE_MAX_FILES;
    let window = (SAMPLE_BYTES / SAMPLE_WINDOWS) as usize;
    let mut buf = vec![0u8; window];
    for p in paths {
        let m = std::fs::metadata(p).ok()?;
        let len = m.len();
        total += len;
        hasher.write_u64(len);
        // mtime is a valid signal when the writer bumps it (buffered writes, or
        // on close); fold it in, but don't rely on it (an mmap writer may lag).
        if let Ok(d) = m.modified().and_then(|mt| {
            mt.duration_since(std::time::UNIX_EPOCH)
                .map_err(std::io::Error::other)
        }) {
            hasher.write_u128(d.as_nanos());
        }
        if sample && len > 0 {
            let mut f = std::fs::File::open(p).ok()?;
            if len <= SAMPLE_BYTES {
                // Small file: fold the whole thing in (still <= the sample cap).
                loop {
                    let n = f.read(&mut buf).ok()?;
                    if n == 0 {
                        break;
                    }
                    hasher.write(&buf[..n]);
                }
            } else {
                // Big file: hash a fixed number of windows spread across it.
                for k in 0..SAMPLE_WINDOWS {
                    let off = (len - window as u64) * k / (SAMPLE_WINDOWS - 1);
                    f.seek(SeekFrom::Start(off)).ok()?;
                    f.read_exact(&mut buf).ok()?;
                    hasher.write(&buf);
                }
            }
        }
    }
    Some((hasher.finish(), total))
}
