//! Auto-reload file watching (the header's "Auto-reload" toggle): stat the pane's source
//! file(s) each update and reload once a change has settled, so a file still
//! being written externally isn't read half-finished.

use super::*;

impl CimApp {
    /// On-disk signature of a pane's source: a hash of the file bytes across its
    /// file(s) plus their total length. `None` for a Compute pane (no file) or
    /// when any file can't be read right now (e.g. mid-rename, or the writer holds
    /// it with no share-read) — in which case the watch simply waits for the next
    /// poll rather than acting on torn contents.
    ///
    /// Content-based rather than mtime-based on purpose: the common case here is a
    /// tool overwriting an image **in place** with the same dimensions, so neither
    /// the byte length nor (on Windows, while the writer keeps the handle open) the
    /// mtime moves — only the pixels do. Reading + hashing the bytes catches that.
    /// The files being watched are single images / stills, page-cache-hot after the
    /// first read, so hashing them a few times a second is cheap.
    pub(super) fn source_file_sig(source: &Source) -> Option<FileSig> {
        use std::hash::Hasher;
        use std::io::Read;
        let paths: &[PathBuf] = match source {
            Source::File(p) => std::slice::from_ref(p),
            Source::Sequence { files, .. } => files.as_slice(),
            Source::Computed => return None,
        };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let mut total = 0u64;
        let mut buf = [0u8; 64 * 1024];
        for p in paths {
            let mut f = std::fs::File::open(p).ok()?;
            loop {
                let n = f.read(&mut buf).ok()?;
                if n == 0 {
                    break;
                }
                hasher.write(&buf[..n]);
                total += n as u64;
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
