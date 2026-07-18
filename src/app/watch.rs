//! Auto-reload file watching (the header's "Auto-reload" toggle): stat the pane's source
//! file(s) each update and reload once a change has settled, so a file still
//! being written externally isn't read half-finished.

use super::*;

impl CimApp {
    /// On-disk signature of a pane's source for change detection: the total byte
    /// length and latest mtime across its file(s), **plus a small strided sample of
    /// the file bytes**. `None` for a Compute pane (no file) or when any file can't
    /// be read right now (e.g. mid-rename) — the watch then waits for the next poll
    /// rather than acting on torn contents.
    ///
    /// Why the content sample, not mtime alone: the common auto-reload case is a
    /// tool overwriting a **single multi-page TIFF in place** with the same
    /// dimensions (e.g. `tifffile.memmap`). The byte length doesn't change, and an
    /// `mmap`'d writer often doesn't bump the mtime until its dirty pages flush, so
    /// an `(mtime, len)` signature can stay identical while the pixels change. A
    /// `read()` sees the new bytes immediately (same page cache), so sampling a few
    /// windows catches it. The sample is **bounded** (`WATCH_SAMPLE_BYTES` per file,
    /// spread across the file) so a huge TIFF is only touched a few KiB per poll,
    /// never bulk-read. It's applied only when the source is **one or a few files**;
    /// a long numbered run stays on the cheap metadata path (those frames are
    /// written normally, so their mtime moves and length/mtime alone suffice).
    pub(super) fn source_file_sig(source: &Source) -> Option<FileSig> {
        use std::hash::Hasher;
        use std::io::{Read, Seek, SeekFrom};
        let paths: &[PathBuf] = match source {
            Source::File(p) => std::slice::from_ref(p),
            Source::Sequence { files, .. } => files.as_slice(),
            Source::Computed => return None,
        };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let mut total = 0u64;
        // Only sample bytes for a small source (the single-TIFF case); a long run
        // would cost one open+read per file each poll for no benefit.
        let sample = paths.len() <= WATCH_SAMPLE_MAX_FILES;
        let window = (WATCH_SAMPLE_BYTES / WATCH_SAMPLE_WINDOWS) as usize;
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
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
            }) {
                hasher.write_u128(d.as_nanos());
            }
            if sample && len > 0 {
                let mut f = std::fs::File::open(p).ok()?;
                if len <= WATCH_SAMPLE_BYTES {
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
                    for k in 0..WATCH_SAMPLE_WINDOWS {
                        let off = (len - window as u64) * k / (WATCH_SAMPLE_WINDOWS - 1);
                        f.seek(SeekFrom::Start(off)).ok()?;
                        f.read_exact(&mut buf).ok()?;
                        hasher.write(&buf);
                    }
                }
            }
        }
        Some((hasher.finish(), total))
    }

    /// Poll every watched pane's source file(s) and reload those whose contents
    /// have changed and then settled (unchanged for `WATCH_DEBOUNCE`). Runs before
    /// `refresh_textures`, so the reloaded frame re-renders and commits in step
    /// with the other panes instead of flashing. Cheap: one `stat` per file, and
    /// only fires the (heavier) reload once a change has quiesced.
    pub(super) fn poll_watches(&mut self, now: f64) {
        let mut to_reload: Vec<usize> = Vec::new();
        for i in 0..self.panes.len() {
            if !self.panes[i].watch.on {
                continue;
            }
            let Some(sig) = Self::source_file_sig(&self.panes[i].source) else {
                continue; // unreadable this tick (mid-write/rename) — try again later
            };
            // Establish the baseline on the first successful stat.
            let Some(loaded) = self.panes[i].watch.loaded else {
                self.panes[i].watch.loaded = Some(sig);
                self.panes[i].watch.seen = None;
                continue;
            };
            if sig == loaded {
                self.panes[i].watch.seen = None; // unchanged (or reverted)
                continue;
            }
            // Changed from the loaded contents: wait for it to stop changing.
            match self.panes[i].watch.seen {
                Some((seen, t0)) if seen == sig => {
                    if now - t0 >= WATCH_DEBOUNCE {
                        self.panes[i].watch.seen = None;
                        to_reload.push(i);
                    }
                }
                // First sighting of this signature (or it changed again) — (re)arm.
                _ => self.panes[i].watch.seen = Some((sig, now)),
            }
        }
        for i in to_reload {
            self.reload(i); // re-baselines watch_loaded to the fresh contents
        }
    }
}
