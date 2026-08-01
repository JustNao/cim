//! Auto-reload file watching (the header's "Auto-reload" toggle): sign the pane's
//! source file(s) in the background and reload once a change has settled, so a
//! file still being written externally isn't read half-finished.
//!
//! Two rules keep the watch off the interactive path:
//! - the signing itself runs on the [`crate::watcher::FileWatcher`] worker thread,
//!   never inline (a signature is real file I/O — tens of ms on a network share);
//! - a new signature is only *requested* every `WATCH_POLL`, not every repaint.
//!   The two were previously conflated: `WATCH_POLL` only paced the idle wake-up,
//!   so as soon as the user panned or zoomed (input-driven repaints at display
//!   rate) the watch signed its source 60–140 times a second, on the UI thread.

use super::*;

impl CimApp {
    /// The file(s) a pane's source is made of, for the watcher to sign. `None` for
    /// a Compute pane (no file — it uses its own Auto-refresh).
    fn watch_paths(source: &Source) -> Option<Vec<PathBuf>> {
        match source {
            Source::File(p) => Some(vec![p.clone()]),
            Source::Sequence { files, .. } => Some(files.clone()),
            Source::Computed => None,
        }
    }

    /// How often a source made of `files` files is re-signed.
    ///
    /// Signing costs one `stat` per file — so the metadata path, cheap per call,
    /// is what actually scales badly: a 500-file run at `WATCH_POLL` aims 2500
    /// filesystem calls a second at the server, and on a **shared** network mount
    /// that is a real cost borne by everyone. Back the interval off in proportion
    /// to the file count so the call *rate* stays roughly flat, capped by
    /// `WATCH_POLL_MAX` so the watch still feels like one.
    ///
    /// Deliberately not the alternative — signing only a *subset* of a long run's
    /// files. That would keep the 200 ms cadence but silently stop noticing a
    /// change to any file left out, trading correctness for latency. Polling all
    /// of them less often loses neither.
    fn watch_interval(files: usize) -> f64 {
        let scale = (files as f64 / crate::watcher::SAMPLE_MAX_FILES as f64).max(1.0);
        (WATCH_POLL.as_secs_f64() * scale).min(WATCH_POLL_MAX.as_secs_f64())
    }

    /// Re-baseline pane `i`'s watch to whatever is on disk *now*, discarding any
    /// signature already in flight (which measured the previous contents). Used
    /// when the watch is switched on and after a reload, so neither event makes
    /// the watch immediately fire again. The baseline is established
    /// asynchronously: `loaded = None` means "adopt the next signature", and
    /// bumping the generation rejects the in-flight one.
    pub(super) fn rebaseline_watch(&mut self, i: usize) {
        self.watch_gen += 1;
        let w = &mut self.panes[i].watch;
        w.loaded = None;
        w.seen = None;
        w.inflight = None;
    }

    /// Apply any signature the watcher has finished, then (at most every
    /// `WATCH_POLL`) request a fresh one for each watching pane. A pane whose
    /// contents changed and then stayed unchanged for `WATCH_DEBOUNCE` is
    /// reloaded. Runs before `refresh_textures`, so a reloaded frame re-renders
    /// and commits in step with the other panes instead of flashing.
    pub(super) fn poll_watches(&mut self, now: f64) {
        let mut to_reload: Vec<usize> = Vec::new();
        for done in self.watcher.drain() {
            let Some(i) = self.panes.iter().position(|p| p.id == done.id) else {
                continue; // pane closed while the signature was in flight
            };
            if self.panes[i].watch.inflight != Some(done.gen) {
                continue; // superseded by a reload / toggle: measured stale contents
            }
            self.panes[i].watch.inflight = None;
            let Some(sig) = done.sig else {
                continue; // unreadable this tick (mid-write/rename) — try again later
            };
            // Establish the baseline on the first successful signature.
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
            self.reload(i); // re-baselines the watch to the fresh contents
        }

        // Rate-limit the requests themselves. One signature per pane in flight at
        // a time, so a slow share can never queue up a backlog.
        if now - self.watch_polled_at < WATCH_POLL.as_secs_f64() {
            return;
        }
        self.watch_polled_at = now;
        for i in 0..self.panes.len() {
            if !self.panes[i].watch.on || self.panes[i].watch.inflight.is_some() {
                continue;
            }
            let Some(paths) = Self::watch_paths(&self.panes[i].source) else {
                continue;
            };
            // A long run signs far more slowly than a lone file — see
            // `watch_interval`. The global gate above only caps how often we get
            // *here*; this is the per-source rate.
            if now - self.panes[i].watch.polled_at < Self::watch_interval(paths.len()) {
                continue;
            }
            self.panes[i].watch.polled_at = now;
            self.watch_gen += 1;
            let gen = self.watch_gen;
            self.panes[i].watch.inflight = Some(gen);
            self.watcher.request(self.panes[i].id, gen, paths);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_backs_off_with_the_file_count() {
        let poll = WATCH_POLL.as_secs_f64();
        let cap = WATCH_POLL_MAX.as_secs_f64();

        // A small source keeps the full cadence: one or a few stats per poll is
        // nothing, and this is the case auto-reload actually exists for.
        for files in [0, 1, 2, crate::watcher::SAMPLE_MAX_FILES] {
            assert_eq!(CimApp::watch_interval(files), poll, "{files} files");
        }

        // Past that it backs off in proportion, so the *rate* of filesystem
        // calls stays flat rather than scaling with the run length.
        let files = crate::watcher::SAMPLE_MAX_FILES * 4;
        assert_eq!(CimApp::watch_interval(files), poll * 4.0);
        let rate = |n: usize| n as f64 / CimApp::watch_interval(n);
        assert!((rate(files) - rate(crate::watcher::SAMPLE_MAX_FILES)).abs() < 1e-9);

        // ...but never past the cap, so a watch stays a watch.
        assert_eq!(CimApp::watch_interval(100_000), cap);
    }
}
