//! Background decode pool plumbing and texture preparation.
//!
//! The UI submits per-pane decode jobs and drains finished frames each update;
//! sequence length is discovered lazily (one page of lookahead while browsing,
//! or driven to the end by "Load all").

use super::*;

impl CimApp {
    pub(super) fn pump_decoder(&mut self) {
        let clock = self.clock;
        for d in self.decoder.drain() {
            self.inflight.remove(&(d.id, d.frame));
            match d.result {
                Ok(Some(frame)) => {
                    if let Some(p) = self.panes.iter_mut().find(|p| p.id == d.id) {
                        p.media.insert(d.frame, frame);
                        p.media.touch(d.frame, clock); // freshly decoded → most recent
                        p.error = None; // a good frame clears any stale error
                    }
                }
                Ok(None) => {
                    // Frontier probe found no page here: a TIFF has reached its
                    // end; a concatenation rolls over to the next file.
                    if let Some(p) = self.panes.iter_mut().find(|p| p.id == d.id) {
                        p.media.frontier_ended();
                    }
                }
                Err(e) => {
                    if let Some(p) = self.panes.iter_mut().find(|p| p.id == d.id) {
                        p.error = Some(format!("Frame {}: {e}", d.frame + 1));
                    }
                }
            }
        }
    }

    pub(super) fn request(&mut self, idx: usize, frame: usize) {
        let id = self.panes[idx].id;
        if self.inflight.contains(&(id, frame)) {
            return;
        }
        if let Some(req) = self.panes[idx].media.decode_job(frame) {
            self.decoder.request(id, frame, req);
            self.inflight.insert((id, frame));
        }
    }

    pub(super) fn load_all(&mut self) {
        for p in &mut self.panes {
            p.eager = true;
        }
        self.status = "Queued all frames for background decoding…".into();
        self.decoding_all = true;
    }

    /// While "Load all" is active, keep every eager pane requesting its missing
    /// known frames plus one frontier probe, so an unknown-length sequence loads
    /// fully and reveals its end. A pane clears its flag once every frame is
    /// resident and its end has been found.
    pub(super) fn drive_eager(&mut self) {
        for i in 0..self.panes.len() {
            if !self.panes[i].eager {
                continue;
            }
            let known = self.panes[i].media.frame_count();
            let mut pending = false;
            for f in 0..known {
                if self.panes[i].media.resident(f).is_none() {
                    self.request(i, f);
                    pending = true;
                }
            }
            if !self.panes[i].media.at_end() {
                self.request(i, known); // probe for a next page
                pending = true;
            }
            if !pending {
                self.panes[i].eager = false;
            }
        }
    }

    /// Walk lazy length-discovery forward until a pending `--frame`/replay seek
    /// becomes reachable, then land the shared timeline on it. Frames are only
    /// discovered contiguously (one page past the frontier at a time), so a
    /// requested frame beyond the known end can't be shown until every page up
    /// to it has been probed. Until then the timeline rides the frontier so the
    /// user sees load progress; once the length passes the target (or the real
    /// end is found first) it snaps to the requested frame.
    pub(super) fn drive_seek(&mut self) {
        let Some(target) = self.pending_seek else {
            return;
        };
        // Manual playback fights an automatic seek — let the user win.
        if self.playing || self.panes.is_empty() {
            self.pending_seek = None;
            return;
        }
        let i = self.current.min(self.panes.len() - 1);
        let known = self.panes[i].media.frame_count();
        if known > target {
            self.shared_frame = target;
            self.pending_seek = None;
        } else if self.panes[i].media.at_end() {
            // Sequence ended before the target — clamp to its last frame.
            self.shared_frame = known - 1;
            self.pending_seek = None;
        } else {
            // Ride the frontier and probe the next page; `ensure_lookahead`
            // (triggered by this frame position) issues the actual request.
            self.shared_frame = known - 1;
            self.request(i, known);
        }
    }

    /// Keep the next page discovered for panes the user is browsing, so stepping
    /// forward and the timeline length stay ahead of the cursor without ever
    /// decoding past what's actually being viewed.
    pub(super) fn ensure_lookahead(&mut self) {
        for i in 0..self.panes.len() {
            if self.panes[i].eager || self.panes[i].media.at_end() {
                continue;
            }
            let known = self.panes[i].media.frame_count();
            // Probe one page beyond the frame currently shown.
            if self.frame_disp(i) + 2 > known {
                self.request(i, known);
            }
        }
    }

    /// Evict least-recently-viewed frames once resident memory exceeds the
    /// budget. Each pane's currently shown frame is protected so the view never
    /// blanks, and an over-budget "Load all" is stopped rather than thrashing.
    pub(super) fn enforce_cache_budget(&mut self) {
        let mut total: usize = self.panes.iter().map(|p| p.media.resident_bytes()).sum();
        if total <= CACHE_BUDGET_BYTES {
            return;
        }

        // The sequence(s) can't all fit — a running "Load all" would just fight
        // eviction forever, so stop it and tell the user.
        if self.panes.iter().any(|p| p.eager) {
            for p in &mut self.panes {
                p.eager = false;
            }
            self.decoding_all = false;
            self.status = "Memory budget reached — keeping the most-recent frames only".into();
        }

        // Gather evictable frames (resident, not currently shown), oldest first.
        let mut cands: Vec<(u64, usize, usize, usize)> = Vec::new(); // (used, pane, frame, bytes)
        for i in 0..self.panes.len() {
            let shown = self.frame_disp(i);
            for (frame, used, bytes) in self.panes[i].media.resident_frames() {
                if frame != shown {
                    cands.push((used, i, frame, bytes));
                }
            }
        }
        cands.sort_unstable_by_key(|c| c.0);
        for (_, i, frame, bytes) in cands {
            if total <= CACHE_BUDGET_BYTES {
                break;
            }
            self.panes[i].media.evict(frame);
            total -= bytes;
        }
    }

    /// Clear the "decoding…" status once the whole batch has landed.
    pub(super) fn poll_decoding_all(&mut self) {
        if self.decoding_all && !self.panes.iter().any(|p| p.eager) && self.inflight.is_empty() {
            self.decoding_all = false;
            if self.status == "Queued all frames for background decoding…" {
                self.status.clear();
            }
        }
    }

    // ---- textures --------------------------------------------------------

    pub(super) fn tex_options(&self) -> TextureOptions {
        let magnification = match self.config.vis.interp {
            Interpolation::Nearest => egui::TextureFilter::Nearest,
            Interpolation::Bilinear => egui::TextureFilter::Linear,
        };
        TextureOptions {
            magnification,
            minification: egui::TextureFilter::Linear,
            ..Default::default()
        }
    }

    /// Ensure pane `idx` shows the best texture available for its current frame.
    /// Returns `(texture, loading)`: if the target frame is resident it uploads
    /// and returns it (`loading = false`); otherwise it queues a decode and
    /// returns the *previously shown* texture with `loading = true`, so the pane
    /// keeps displaying the last frame while the new one decodes.
    pub(super) fn prepare(&mut self, ctx: &egui::Context, idx: usize) -> (Option<TextureId>, bool) {
        let f = self.frame_disp(idx);
        if let Some(frame) = self.panes[idx].media.resident(f) {
            self.panes[idx].media.touch(f, self.clock); // viewing keeps it hot
            let need = match &self.panes[idx].tex {
                Some(t) => t.shown != f,
                None => true,
            };
            if need {
                // Only run the (expensive) render + upload when the texture is
                // stale. Bounds are memoized on the frame; render into a reused
                // scratch buffer via the LUT path.
                let opts = self.tex_options();
                // Full-range mapping to 8-bit; contrast/detail operators (the
                // proprietary C++) then transform the rendered RGBA in place.
                let (lo, hi) = frame.display_bounds(false);
                frame.render_into(lo, hi, &mut self.render_scratch);
                let [w, h] = frame.size;
                if self.panes[idx].contrast == ContrastMode::LutAlpha {
                    crate::imageproc::lut_alpha(&mut self.render_scratch, w, h);
                }
                if self.panes[idx].details {
                    crate::imageproc::details_enhanced(&mut self.render_scratch, w, h);
                }
                let img = ColorImage::from_rgba_unmultiplied(frame.size, &self.render_scratch);
                let p = &mut self.panes[idx];
                match &mut p.tex {
                    Some(t) => {
                        t.handle.set(img, opts);
                        t.shown = f;
                    }
                    None => {
                        let handle = ctx.load_texture(format!("m{}", p.id), img, opts);
                        p.tex = Some(CachedTex { handle, shown: f });
                    }
                }
            }
            (Some(self.panes[idx].tex.as_ref().unwrap().handle.id()), false)
        } else {
            self.request(idx, f);
            let last = self.panes[idx].tex.as_ref().map(|t| t.handle.id());
            (last, true)
        }
    }
}
