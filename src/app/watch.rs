//! Auto-reload file watching (the header's ◉ toggle): stat the pane's source
//! file(s) each update and reload once a change has settled, so a file still
//! being written externally isn't read half-finished.

use super::*;

impl CimApp {
    /// On-disk signature of a pane's source: the latest mtime across its file(s)
    /// and their total length. `None` for a Compute pane (no file) or when any
    /// file can't be stat-ed right now (e.g. mid-rename) — in which case the
    /// watch simply waits for the next poll rather than reloading torn contents.
    pub(super) fn source_file_sig(source: &Source) -> Option<FileSig> {
        let paths: &[PathBuf] = match source {
            Source::File(p) => std::slice::from_ref(p),
            Source::Sequence { files, .. } => files.as_slice(),
            Source::Computed => return None,
        };
        let mut latest: Option<std::time::SystemTime> = None;
        let mut total = 0u64;
        for p in paths {
            let m = std::fs::metadata(p).ok()?;
            total += m.len();
            let mt = m.modified().ok()?;
            latest = Some(match latest {
                Some(l) if l >= mt => l,
                _ => mt,
            });
        }
        latest.map(|l| (l, total))
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
