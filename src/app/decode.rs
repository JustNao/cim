//! Background decode pool plumbing and texture preparation.
//!
//! The UI submits per-pane decode jobs and drains finished frames each update;
//! sequence length is discovered lazily (one page of lookahead while browsing,
//! or driven to the end by "Load all").

use super::*;

impl CimApp {
    pub(super) fn pump_decoder(&mut self) {
        let clock = self.clock;
        let debug = crate::debug::enabled();
        for d in self.decoder.drain() {
            self.inflight.remove(&(d.id, d.frame));
            match d.result {
                Ok(Decoded::Frame(frame)) => {
                    // Only a real decode (not a metadata-only probe) counts.
                    if debug {
                        self.metrics.decode.record(d.elapsed);
                    }
                    // Always-on latency EMA (α = 1/8) driving adaptive prefetch depth.
                    let s = d.elapsed.as_secs_f32();
                    self.decode_ema_secs = if self.decode_ema_secs <= 0.0 {
                        s
                    } else {
                        self.decode_ema_secs + (s - self.decode_ema_secs) / 8.0
                    };
                    if let Some(p) = self.panes.iter_mut().find(|p| p.id == d.id) {
                        p.media.insert(d.frame, frame);
                        p.media.touch(d.frame, clock); // freshly decoded → most recent
                        p.error = None; // a good frame clears any stale error
                    }
                }
                Ok(Decoded::Exists) => {
                    // Metadata-only probe confirmed a page without decoding it:
                    // grow the known length by one empty slot (the seek fast-path).
                    if let Some(p) = self.panes.iter_mut().find(|p| p.id == d.id) {
                        p.media.note_frontier(d.frame);
                    }
                }
                Ok(Decoded::End) => {
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

    /// Like `request`, but a **metadata-only** frontier probe: confirms the page
    /// exists without decoding its pixels. Used by `drive_seek` to fast-forward
    /// length discovery during a seek so the intervening pages aren't
    /// decompressed — only the landed target frame is. Shares the `inflight`
    /// dedupe set with `request`; the two never contend for the same (id, frame)
    /// because a probe only targets the undiscovered frontier.
    pub(super) fn probe(&mut self, idx: usize, frame: usize) {
        let id = self.panes[idx].id;
        if self.inflight.contains(&(id, frame)) {
            return;
        }
        if let Some(req) = self.panes[idx].media.probe_job(frame) {
            self.decoder.request(id, frame, req);
            self.inflight.insert((id, frame));
        }
    }

    /// "Load all": decode every frame of every sequence and drive its frontier to
    /// the end. If the frame cache fills mid-load, `enforce_cache_budget` downgrades
    /// it to offsets-only (headers) so length discovery still finishes.
    pub(super) fn load_all(&mut self) {
        for p in &mut self.panes {
            p.eager = Eager::Full;
        }
        self.load_cache_exhausted = false;
        self.export_load_pending = false; // only the export button sets this
        self.status.set("Queued all frames for background decoding…");
        self.decoding_all = true;
    }

    /// "Load offsets": drive every sequence's frontier to its true end with
    /// **metadata-only** probes (discover the length via headers alone, decoding
    /// no pixels), so the timeline is complete without filling the frame cache. A
    /// pane already doing a full "Load all" keeps it (a superset).
    pub(super) fn load_offsets(&mut self) {
        for p in &mut self.panes {
            if p.eager != Eager::Full {
                p.eager = Eager::Offsets;
            }
        }
        self.status.set("Discovering sequence length (headers only)…");
        self.decoding_all = true;
    }

    /// Cancel any in-progress bulk load ("Load all" / "Load offsets").
    pub(super) fn stop_load(&mut self) {
        for p in &mut self.panes {
            p.eager = Eager::Off;
        }
        self.decoding_all = false;
        self.export_load_pending = false;
        self.status.set("Stopped loading");
    }

    /// Drive the active bulk loads each update. A **Full** pane requests every
    /// missing known frame plus one frontier decode, clearing itself once fully
    /// resident and ended. An **Offsets** pane only probes the frontier (headers,
    /// no pixel decode), clearing itself once the end is found.
    pub(super) fn drive_eager(&mut self) {
        for i in 0..self.panes.len() {
            match self.panes[i].eager {
                Eager::Off => continue,
                Eager::Full => {
                    let known = self.panes[i].media.frame_count();
                    let ff = self.playback.fast_forward.max(1);
                    let mut pending = false;
                    // Decode 1 of every `ff` frames (all of them when ff == 1). The
                    // frames in between are never decoded — they're discovered as
                    // headers only while the frontier advances below.
                    for f in (0..known).step_by(ff) {
                        if self.panes[i].media.resident(f).is_none() {
                            self.request(i, f);
                            pending = true;
                        }
                    }
                    if !self.panes[i].media.at_end() {
                        // Extend the known length. With a stride, skim the frontier
                        // via a metadata-only header probe (the N-1 between decodes
                        // are discovered, not decoded); without one, decode the next
                        // page as before.
                        if ff > 1 {
                            self.probe(i, known);
                        } else {
                            self.request(i, known);
                        }
                        pending = true;
                    }
                    if !pending {
                        self.panes[i].eager = Eager::Off;
                    }
                }
                Eager::Offsets => {
                    if self.panes[i].media.at_end() {
                        self.panes[i].eager = Eager::Off;
                    } else {
                        let known = self.panes[i].media.frame_count();
                        self.probe(i, known); // headers only, no pixel decode
                    }
                }
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
        if self.playback.playing || self.panes.is_empty() {
            self.pending_seek = None;
            return;
        }
        let i = self.control.min(self.panes.len() - 1);
        let known = self.panes[i].media.frame_count();
        if known > target {
            self.shared_frame = target;
            self.pending_seek = None;
        } else if self.panes[i].media.at_end() {
            // Sequence ended before the target — clamp to its last frame.
            self.shared_frame = known - 1;
            self.pending_seek = None;
        } else {
            // Ride the frontier with a metadata-only probe: confirm the next
            // page exists (growing the known length) without decoding it, so a
            // far seek walks IFD headers instead of decompressing every frame it
            // passes. Only the target lands as a real decode, once discovery
            // reaches it (`known > target`, above). `ensure_lookahead` is
            // suppressed during a `pending_seek` so it can't fire a full decode
            // of the same frontier page and defeat this.
            self.shared_frame = known - 1;
            self.probe(i, known);
        }
    }

    /// Keep the next page discovered for panes the user is browsing, so stepping
    /// forward and the timeline length stay ahead of the cursor without ever
    /// decoding past what's actually being viewed. Only panes actually on screen
    /// (see `displayed_indices`) are probed — a loaded-but-hidden sequence would
    /// otherwise keep decoding its frontier and starve the shown pane, making the
    /// UI laggy even when a single media is displayed.
    pub(super) fn ensure_lookahead(&mut self) {
        // During a seek, frontier discovery is `drive_seek`'s job — via a
        // metadata-only probe. Skip lookahead so it can't issue a *full* decode
        // of the frontier page (the panes are frozen and nothing is browsing
        // anyway); it resumes the update after the seek lands.
        if self.panes.is_empty() || self.pending_seek.is_some() {
            return;
        }
        // The control pane drives the shared timeline/scrubber even when it isn't
        // on screen, so it must keep discovering its frontier too.
        let mut targets = self.displayed_indices();
        let ctrl = self.control.min(self.panes.len().saturating_sub(1));
        if !targets.contains(&ctrl) {
            targets.push(ctrl);
        }
        for i in targets {
            if self.panes[i].eager != Eager::Off || self.panes[i].media.at_end() {
                continue; // a bulk load (drive_eager) already drives this pane's frontier
            }
            let known = self.panes[i].media.frame_count();
            if self.catching_up(i) {
                // Target far past the frontier (a sequence behind an advanced
                // timeline): discover with a metadata-only probe so the pages in
                // between aren't decoded — only the target lands (see `stage`).
                self.probe(i, known);
            } else if self.frame_disp(i) + 2 > known {
                // Browsing at the frontier. With a fast-forward stride, skim it by
                // header only (probe) so the frames jumped over aren't decoded — the
                // one landed on decodes on demand in `stage`; otherwise prefetch the
                // next page with a full decode.
                if self.playback.fast_forward > 1 {
                    self.probe(i, known);
                } else {
                    self.request(i, known);
                }
            }
        }
    }

    /// While playing, pre-decode the next few frames for each on-screen pane so
    /// playback overlaps decode with display instead of stalling on decode
    /// latency when it reaches a not-yet-resident frame (worst on the first pass
    /// through a sequence, and amplified when several sequences advance in
    /// lock-step). Follows the same loop-window logic as `advance_playback`;
    /// requests are deduped by `inflight`, and nothing is requested past the known
    /// length — lazy frontier discovery stays with `ensure_lookahead` — so
    /// re-running it every update is cheap.
    pub(super) fn prefetch_playback(&mut self) {
        if !self.playback.playing || self.panes.is_empty() {
            return;
        }
        let tl = self.timeline_len();
        let (lo, hi) = self.loop_bounds(tl);
        let full = self.playback.loop_range.is_none();
        let at_end = self.current_at_end();

        // Same targets as lookahead: on-screen panes plus the control pane (which
        // drives the shared timeline even when it isn't displayed).
        let mut targets = self.displayed_indices();
        let ctrl = self.control.min(self.panes.len() - 1);
        if !targets.contains(&ctrl) {
            targets.push(ctrl);
        }
        // Prefetch the frames playback will actually land on: with a fast-forward
        // stride it steps by `ff`, so prefetch the strided targets (not the frames
        // skimmed over) to match `advance_playback`.
        let ff = self.playback.fast_forward.max(1);
        let depth = prefetch_depth(
            self.decode_ema_secs,
            self.playback.fps,
            self.decode_threads_active,
            targets.len(),
        );

        // Build each pane's ordered list of the next frames it will show, then
        // dispatch them round-robin *by distance* (every pane's +1, then every
        // pane's +2, …). The lock-step commit waits on the slowest pane, and the
        // decode pool is one shared queue — so requesting one pane's whole burst
        // before the next's would front-load the queue and starve the very pane
        // that gates the commit. Interleaving keeps each pane's nearest-needed
        // frame near the front.
        let mut plans: Vec<(usize, Vec<usize>)> = Vec::with_capacity(targets.len());
        for i in targets {
            let known = self.panes[i].media.frame_count();
            let mut frames = Vec::with_capacity(depth);
            if self.panes[i].sync_temporal {
                // Walk the loop window forward from where playback is now, wrapping
                // to the window start when looping — exactly the frames it shows next.
                let mut f = self.playback.prefetch.unwrap_or(self.shared_frame);
                for _ in 0..depth {
                    f = if f < hi {
                        (f + ff).min(hi)
                    } else if full && !at_end {
                        break; // holding at the frontier; discovery is ensure_lookahead's job
                    } else if self.playback.loop_playback {
                        lo // wrap to the window start
                    } else {
                        break; // playback will stop at the window end
                    };
                    if f >= known {
                        break;
                    }
                    frames.push(f);
                }
            } else {
                // Unsynced pane: look ahead on its own timeline (strided too).
                let base = self.panes[i].frame;
                for k in 1..=depth {
                    let f = base + k * ff;
                    if f >= known {
                        break;
                    }
                    frames.push(f);
                }
            }
            plans.push((i, frames));
        }

        for (i, f) in interleave_prefetch(&plans) {
            if self.panes[i].media.resident(f).is_none() {
                self.request(i, f);
            }
        }
    }

    /// Evict least-recently-viewed frames once resident memory exceeds the
    /// budget. Each pane's currently shown frame is protected so the view never
    /// blanks. A running **full** "Load all" that can't fit is **downgraded to
    /// offsets-only** (headers) rather than stopped, so the sequence length still
    /// finishes discovering — decoding just stops adding frames the cache can't hold.
    pub(super) fn enforce_cache_budget(&mut self) {
        let budget = self.cache_budget_bytes();
        let mut total: usize = self.panes.iter().map(|p| p.media.resident_bytes()).sum();
        if total <= budget {
            return;
        }

        // The sequence(s) can't all fit — a full "Load all" would just fight
        // eviction forever. Downgrade it to offsets-only so it keeps discovering
        // the length via headers (no more pixel decode) instead of thrashing.
        if self.panes.iter().any(|p| p.eager == Eager::Full) {
            for p in &mut self.panes {
                if p.eager == Eager::Full {
                    p.eager = Eager::Offsets;
                }
            }
            self.load_cache_exhausted = true;
            self.status
                .set("Frame cache full — continuing with offsets only (headers) for the rest");
        }

        // Evict the globally least-recently-used resident frame (never a pane's
        // currently shown one) until back under budget. Each pane keeps its
        // resident frames in a recency-ordered set, so picking the oldest is a
        // per-pane O(log n) peek + a merge across the (few) panes — no full scan
        // or sort of the thousands of known-but-non-resident slots.
        while total > budget {
            let mut victim: Option<(u64, usize, usize, usize)> = None; // (tick, pane, frame, bytes)
            for i in 0..self.panes.len() {
                let shown = self.frame_disp(i);
                if let Some((tick, frame, bytes)) = self.panes[i].media.lru_evictable(shown) {
                    if victim.is_none_or(|(t, ..)| tick < t) {
                        victim = Some((tick, i, frame, bytes));
                    }
                }
            }
            let Some((_, i, frame, bytes)) = victim else {
                break; // nothing evictable (everything left is a shown frame)
            };
            self.panes[i].media.evict(frame);
            total -= bytes;
        }
    }

    /// Clear the "decoding…" status once the whole batch has landed, and — if an
    /// **export**-initiated "Load all" couldn't fully load because the cache was
    /// too small — warn the user with a modal.
    pub(super) fn poll_decoding_all(&mut self) {
        let active = self.panes.iter().any(|p| p.eager != Eager::Off);
        if self.decoding_all && !active && self.inflight.is_empty() {
            self.decoding_all = false;
            // Clear only our own transient load notes (don't clobber a newer one).
            if self.status.text() == "Queued all frames for background decoding…"
                || self.status.text().starts_with("Discovering sequence length")
                || self.status.text().starts_with("Frame cache full")
            {
                self.status.clear();
            }
            if std::mem::take(&mut self.export_load_pending) && self.load_cache_exhausted {
                self.warn_popup = Some(
                    "The whole sequence couldn't be loaded into memory — the frame \
                     cache is too small to hold every frame at once.\n\nThe length \
                     was fully discovered (headers only), so the export frame range \
                     is correct and the encoder still reads the remaining frames \
                     from disk as it runs. To keep more frames resident, raise the \
                     Frame cache budget in Settings."
                        .into(),
                );
            }
        }
    }

    // ---- textures --------------------------------------------------------

    /// Take finished tone renders off the pool and stage them. A landed render
    /// goes into the pane's **`pending`** slot (not `tex`), so it isn't shown until
    /// `refresh_textures` commits every on-screen pane together; `stage`
    /// re-requests when the result is stale.
    pub(super) fn pump_render(&mut self, ctx: &egui::Context) {
        let debug = crate::debug::enabled();
        for d in self.renderer.drain() {
            self.render_inflight.remove(&d.id);
            if debug {
                self.metrics.lut.record(d.lut_time);
                if !d.ops_time.is_zero() {
                    self.metrics.operators.record(d.ops_time);
                }
            }
            if let Some(idx) = self.panes.iter().position(|p| p.id == d.id) {
                self.upload_tex(ctx, idx, d.size, &d.rgba, d.frame, d.sig);
            }
        }
    }

    /// Bring every on-screen pane's texture up to date and, once they are **all**
    /// ready, flip them to their new frame together. During playback the shared
    /// timeline advances only when this commit lands (`play_prefetch`), so the
    /// frame counter never leads the image and all panes update in step — a slow
    /// proprietary operator paces playback instead of the counter racing ahead.
    ///
    /// No spinner: a pane keeps showing its last committed frame while the next
    /// one decodes / renders, then swaps in atomically.
    pub(super) fn refresh_textures(&mut self, ctx: &egui::Context) {
        // While a length-discovery seek rides the frontier, freeze the display
        // (keep the last committed textures) rather than rendering every frame the
        // probe passes through; `drive_seek` snaps to the target when it's found.
        if self.pending_seek.is_some() {
            return;
        }
        let panes = self.displayed_indices();
        if panes.is_empty() {
            return;
        }
        // Physical pixels per point (OS DPI × UI-scale zoom factor), so decimation
        // is judged against real screen resolution, not view-space points.
        let ppp = ctx.pixels_per_point();
        let mut all_ready = true;
        let mut staged: Vec<(usize, usize)> = Vec::with_capacity(panes.len());
        for &idx in &panes {
            // A pane discovering toward a far target holds its last committed
            // frame (keeps `tex`) instead of flipping through the pages in
            // between — `ensure_lookahead` probes it forward, and it stages
            // normally once the target itself is discovered.
            if self.catching_up(idx) {
                continue;
            }
            let target = self.stage_target(idx);
            if !self.stage(ctx, idx, target, ppp) {
                all_ready = false;
            }
            staged.push((idx, target));
        }
        if !all_ready {
            return;
        }
        // Commit: flip each pane whose *pending* slot holds the target frame to the
        // front. Only then — a bare `pending.is_some()` would also fire on idle
        // repaints (cursor move / pan), where `pending` still holds the previous
        // frame's texture kept for handle reuse, swapping the stale frame back in
        // and making the image flicker between frames. The swap keeps the old
        // texture in `pending` so its handle is reused next frame (no per-frame
        // texture allocation during playback).
        for (idx, target) in staged {
            let sig = self.tone_sig(idx);
            let step = self.want_step(idx, ppp);
            self.panes[idx]
                .tex
                .commit(|t| t.shown == target && t.sig == sig && t.step == step);
        }
        // A committed playback step advances the shared timeline to the frame we
        // just showed — so the counter and the image stay on the same frame.
        if let Some(f) = self.playback.prefetch.take() {
            self.shared_frame = f;
        }
    }

    /// The texture to draw for pane `idx`: the committed one, or — only until the
    /// first commit lands — a freshly staged frame, so a pane isn't blank while its
    /// siblings are still rendering. After that `tex` is always present and holds
    /// until the group flips, so on-screen panes stay in step.
    pub(super) fn pane_texture(&self, idx: usize) -> Option<TextureId> {
        self.panes[idx].tex.id()
    }

    /// The frame `refresh_textures` should stage for pane `idx`. Synced panes chase
    /// the playback prefetch (the candidate next shared frame) if one is in flight,
    /// else the committed shared frame; unsynced panes use their own frame.
    fn stage_target(&self, idx: usize) -> usize {
        let c = self.panes[idx].media.frame_count().max(1);
        if self.panes[idx].sync_temporal {
            self.playback.prefetch.unwrap_or(self.shared_frame).min(c - 1)
        } else {
            self.panes[idx].frame % c
        }
    }

    /// Source pixels per screen pixel for pane `idx` at its current zoom — the
    /// nearest-decimation factor for the synchronous render. `ppp` is the
    /// physical pixels per point (OS DPI × UI-scale zoom), so decimation is judged
    /// against real screen resolution: a pane is only decimated once it is truly
    /// minified below one screen pixel per source pixel.
    ///
    /// Returns `1` (full resolution) for any physical scale ≥ 1, so the entire
    /// ≥1× range **and its whole neighbourhood** render full-resolution — crossing
    /// 1× never changes what's on screen. It rises to 2, 3, … only as the pane is
    /// minified further, where full-resolution pixels the screen can't show would
    /// be pure waste.
    fn stage_step(&self, idx: usize, ppp: f32) -> usize {
        let phys = self.view_ref(idx).zoom * ppp.max(1e-3);
        if phys >= 1.0 {
            1
        } else {
            (1.0 / phys).floor().max(1.0) as usize
        }
    }

    /// The decimation factor pane `idx`'s texture is (re)rendered at: `stage_step`
    /// for a plain LUT pane, forced to `1` for a heavy proprietary-operator pane
    /// (decimating an operator's input would change its output and thrash the
    /// size-keyed instances, so those always render full-resolution). Read by both
    /// `stage` and the lock-step commit so a texture's `step` is compared against
    /// the one the pane wants right now.
    fn want_step(&self, idx: usize, ppp: f32) -> usize {
        let heavy = self.pane_ops_active(idx);
        if heavy {
            1
        } else {
            self.stage_step(idx, ppp)
        }
    }

    /// Render pane `idx`'s texture for frame `target` **without disturbing what's
    /// currently shown** (`tex`): the result lands in `pending`, to be committed
    /// by `refresh_textures`. Returns whether `target` is ready — already in `tex`,
    /// or staged in `pending`.
    ///
    /// The plain LUT render (Linear / Linear+Clip, and masks) is cheap and stays
    /// **synchronous**. The heavy proprietary operators (LUT_ALPHA / details)
    /// render on the [`RenderPool`] and land in `pending` via `pump_render`, so a
    /// slow operator never blocks the UI thread. An errored pane reports ready so
    /// it can't stall a lockstep commit.
    fn stage(&mut self, ctx: &egui::Context, idx: usize, target: usize, ppp: f32) -> bool {
        if self.panes[idx].error.is_some() {
            return true; // can't produce a frame; keep the last texture
        }
        // Cheap, parameter-only signature of everything that changes the toned
        // output (bar the frame itself). With `shown` it tells a still-current
        // texture from a stale one without recomputing the (possibly O(N)) bounds.
        let sig = self.tone_sig(idx);
        // Nearest-decimation factor for this pane's synchronous render (1 for a
        // heavy proprietary-operator pane, which never decimates). Part of the
        // texture identity so zooming below the full-resolution band re-renders.
        let step = self.want_step(idx, ppp);
        // Already committed to the target — nothing to stage.
        if let Some(t) = &self.panes[idx].tex.front {
            if t.shown == target && t.sig == sig && t.step == step {
                return true;
            }
        }
        // Already staged the target (rendered, awaiting the group commit).
        if let Some(t) = &self.panes[idx].tex.pending {
            if t.shown == target && t.sig == sig && t.step == step {
                return true;
            }
        }
        let Some(frame) = self.panes[idx].media.resident(target) else {
            self.request(idx, target); // not decoded yet: queue it, keep showing tex
            return false;
        };
        self.panes[idx].media.touch(target, self.clock); // staging keeps it hot

        let contrast = self.contrast_of(idx);
        // The proprietary operators only run on single-channel 16-bit frames with
        // the library loaded; otherwise LUT_ALPHA / Details fall back to a plain
        // render, so there's nothing heavy to push off-thread.
        let heavy = crate::imageproc::ops_active(
            &frame,
            contrast == ContrastMode::LutAlpha,
            self.details_of(idx),
        );

        if heavy {
            // Render off-thread. One render per pane at a time, so rapid tone /
            // frame changes coalesce instead of piling up jobs.
            let id = self.panes[idx].id;
            if !self.render_inflight.contains(&id) {
                let (lo, hi) = self.tone_bounds(idx, &frame);
                let lut_alpha = contrast == ContrastMode::LutAlpha;
                let details = self.details_of(idx);
                self.renderer.request(crate::renderer::RenderJob {
                    id,
                    frame: target,
                    sig,
                    data: frame.clone(),
                    lo,
                    hi,
                    lut_alpha,
                    details,
                });
                self.render_inflight.insert(id);
            }
            false // lands in `pending` when the render finishes
        } else {
            // Synchronous LUT render (no proprietary operators). Always nearest,
            // at any zoom: the value under the cursor must be a true source
            // sample, never a blend of neighbours. When the pane is minified past
            // the full-resolution band, decimate to ~display resolution so a grid
            // of panes doesn't render/copy/upload far more pixels than the screen
            // can show (each dropped sample is still a true source value).
            let (lo, hi) = self.tone_bounds(idx, &frame);
            let debug = crate::debug::enabled();
            let t = debug.then(std::time::Instant::now);
            let size = frame.render_into_scaled_lut(
                lo,
                hi,
                step,
                &mut self.panes[idx].tex.lut,
                &mut self.render_scratch,
            );
            if let Some(t) = t {
                self.metrics.lut.record(t.elapsed());
            }
            let t = debug.then(std::time::Instant::now);
            let img = ColorImage::from_rgba_unmultiplied(size, &self.render_scratch);
            let name = format!("m{}", self.panes[idx].id);
            set_cached_tex(&mut self.panes[idx].tex.pending, ctx, name, img, target, sig, step);
            if let Some(t) = t {
                self.metrics.upload.record(t.elapsed());
            }
            true
        }
    }

    /// Stage an RGBA buffer as pane `idx`'s **pending** texture, tagged `(f, sig)`
    /// (committed to the front by `refresh_textures`).
    fn upload_tex(&mut self, ctx: &egui::Context, idx: usize, size: [usize; 2], rgba: &[u8], f: usize, sig: u64) {
        let t = crate::debug::enabled().then(std::time::Instant::now);
        let img = ColorImage::from_rgba_unmultiplied(size, rgba);
        let name = format!("m{}", self.panes[idx].id);
        // Heavy proprietary-operator renders run at full resolution (step 1).
        set_cached_tex(&mut self.panes[idx].tex.pending, ctx, name, img, f, sig, 1);
        if let Some(t) = t {
            self.metrics.upload.record(t.elapsed());
        }
    }

    /// Parameter-only hash of a pane's effective tone: everything that changes the
    /// rendered RGBA for a given frame. Deliberately excludes the frame index (the
    /// texture's `shown` covers that) and never touches the pixels, so it's cheap
    /// to compute every frame.
    pub(super) fn tone_sig(&self, idx: usize) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let c = match self.contrast_of(idx) {
            ContrastMode::Linear => 0u8,
            ContrastMode::LutAlpha => 1,
        };
        let tone = self.tone_of(idx);
        c.hash(&mut h);
        // The clip toggle and its percentile both change the Linear mapping.
        tone.clip.enabled.hash(&mut h);
        tone.clip.percent.to_bits().hash(&mut h);
        self.details_of(idx).hash(&mut h);
        let region = self.panes[idx].region_tone;
        region.hash(&mut h);
        if region {
            // Region-tone bounds move with the shared stats region.
            self.stats_gen.hash(&mut h);
        }
        h.finish()
    }

    /// The linear display bounds `[lo, hi]` for pane `idx`'s current tone: the
    /// per-tail percentile clip, the full range / float extent, or — when
    /// region-tone is pinned — the shared stats region's bounds.
    fn tone_bounds(&self, idx: usize, frame: &media::FrameData) -> (f32, f32) {
        let contrast = self.contrast_of(idx);
        let tone = self.tone_of(idx);
        let pct = tone.clip.percent;
        // Clip only the built-in Linear map, and only when its toggle is on;
        // LUT_ALPHA takes the full range and does its own contrast.
        let clip = matches!(contrast, ContrastMode::Linear) && tone.clip.enabled;
        let base = |clip: bool| {
            if clip {
                frame.clip_bounds(pct)
            } else {
                frame.display_bounds(false)
            }
        };
        if self.panes[idx].region_tone {
            self.stats_region
                .and_then(|reg| pixel_bounds(reg, frame.size))
                .map(|(x0, y0, x1, y1)| frame.region_display_bounds(x0, y0, x1, y1, clip, pct))
                .unwrap_or_else(|| base(clip))
        } else {
            base(clip)
        }
    }

    /// Ensure the tinted overlay texture for pane `idx` is current, returning it
    /// to draw over the pane's image. The overlay config is the pane's
    /// *effective* one (`overlay_of` — shared when tone-synced); the mask is
    /// taken from the referenced pane at its currently shown frame, and the
    /// tinted texture is cached in `Pane.overlay_tex`. Returns `None` when
    /// there's no overlay, the mask pane is gone, or this is itself a mask pane.
    ///
    /// The mask is decoded on demand here, so the overlay works even when the
    /// mask pane itself isn't drawn (hidden in the manager, or just reloaded).
    /// While the frame decodes, the last overlay texture keeps showing.
    pub(super) fn prepare_overlay(&mut self, ctx: &egui::Context, idx: usize) -> Option<TextureId> {
        if self.panes[idx].media.is_mask() {
            return None; // don't tint an overlay onto a mask pane itself
        }
        let ov = self.overlay_of(idx)?;
        let (src_id, color, opacity) = (ov.src_id, ov.color, ov.opacity);
        let src = self.panes.iter().position(|p| p.id == src_id)?;
        let f = self.frame_disp(src);
        let Some(frame) = self.panes[src].media.resident(f) else {
            // Not decoded yet: request it and keep the previous overlay texture.
            self.request(src, f);
            return self.panes[idx]
                .overlay_tex
                .as_ref()
                .map(|t| t.handle.id());
        };
        // Never stretch a mismatched overlay onto the base: if the source frame's
        // size differs from this pane's current frame, skip drawing it. (A newly
        // selected mismatched source is rejected up front with an error popup, so
        // this only guards later per-frame size drift in a sequence.)
        if frame.size != self.disp_size(idx) {
            return None;
        }
        self.panes[src].media.touch(f, self.clock); // keep it hot so it isn't evicted

        let rgb = [color.r(), color.g(), color.b()];
        let alpha = (opacity.clamp(0.0, 1.0) * 255.0) as u8;

        let need = match &self.panes[idx].overlay_tex {
            Some(t) => t.shown != f,
            None => true,
        };
        if need {
            let mut buf = Vec::new();
            // A boolean mask tints where true; any other single-channel image
            // tints by normalised intensity (§9).
            if frame.is_mask() {
                frame.render_mask_rgba(rgb, alpha, &mut buf);
            } else {
                frame.render_intensity_rgba(rgb, alpha, &mut buf);
            }
            let img = ColorImage::from_rgba_unmultiplied(frame.size, &buf);
            // Overlay textures don't tone-map (sig 0) and aren't decimated (step 1).
            let name = format!("ov{}_{}", idx, src_id);
            set_cached_tex(&mut self.panes[idx].overlay_tex, ctx, name, img, f, 0, 1);
        }
        Some(self.panes[idx].overlay_tex.as_ref().unwrap().handle.id())
    }
}

/// Set (or create) a cached texture slot from a freshly rendered image, tagging
/// it with the frame it shows and its tone signature. Shared by the pane image,
/// the tinted overlay, and the off-thread render upload, so the set-or-create
/// dance (and the `NEAREST` filtering the tool depends on) lives in one place.
fn set_cached_tex(
    slot: &mut Option<CachedTex>,
    ctx: &egui::Context,
    name: String,
    img: ColorImage,
    shown: usize,
    sig: u64,
    step: usize,
) {
    let opts = TextureOptions::NEAREST;
    match slot {
        Some(t) => {
            t.handle.set(img, opts);
            t.shown = shown;
            t.sig = sig;
            t.step = step;
        }
        None => {
            let handle = ctx.load_texture(name, img, opts);
            *slot = Some(CachedTex { handle, shown, sig, step });
        }
    }
}

/// How many frames per pane to prefetch, adapting to how slow decoding actually
/// is. `PLAY_PREFETCH` is the floor; depth grows toward `PREFETCH_CAP` when the
/// decode work in flight per committed frame — `latency × panes ÷ workers`,
/// relative to the frame interval — exceeds what the floor buffers, so a slow /
/// heavy sequence (or many panes) doesn't chronically under-prefetch, while a
/// cheap one doesn't over-queue. `latency <= 0` (no measurement yet) → the floor.
fn prefetch_depth(latency_secs: f32, fps: f32, workers: usize, panes: usize) -> usize {
    const PREFETCH_CAP: usize = 8;
    if latency_secs <= 0.0 || panes == 0 {
        return PLAY_PREFETCH;
    }
    let interval = 1.0 / fps.max(0.1);
    let workers = workers.max(1) as f32;
    // Frames of decode in flight per committed frame, rounded up, + 1 slack.
    let need = ((latency_secs * panes as f32) / (workers * interval)).ceil() as usize + 1;
    need.clamp(PLAY_PREFETCH, PREFETCH_CAP)
}

/// Flatten per-pane prefetch frame lists into dispatch order, round-robin **by
/// distance**: every pane's nearest frame first, then every pane's next, and so
/// on. On the single shared decode queue this stops one pane's whole burst from
/// starving the pane that gates the lock-step commit (`prefetch_playback`).
fn interleave_prefetch(plans: &[(usize, Vec<usize>)]) -> Vec<(usize, usize)> {
    let max_len = plans.iter().map(|(_, v)| v.len()).max().unwrap_or(0);
    let mut out = Vec::with_capacity(plans.iter().map(|(_, v)| v.len()).sum());
    for k in 0..max_len {
        for (i, frames) in plans {
            if let Some(&f) = frames.get(k) {
                out.push((*i, f));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{interleave_prefetch, prefetch_depth, PLAY_PREFETCH};

    /// Depth is the floor until a latency is known, grows with slow decode / more
    /// panes, shrinks with more workers, and never leaves the `[floor, 8]` band.
    #[test]
    fn prefetch_depth_adapts_and_clamps() {
        // No measurement yet → floor, regardless of panes.
        assert_eq!(prefetch_depth(0.0, 30.0, 4, 6), PLAY_PREFETCH);
        // Fast decode (2 ms) at 30 fps stays at the floor.
        assert_eq!(prefetch_depth(0.002, 30.0, 4, 2), PLAY_PREFETCH);
        // Slow decode (40 ms) at 30 fps with 4 panes / 2 workers pushes above it.
        assert!(prefetch_depth(0.040, 30.0, 2, 4) > PLAY_PREFETCH);
        // Never exceeds the cap even when pathologically slow.
        assert_eq!(prefetch_depth(5.0, 30.0, 1, 8), 8);
        // More workers reduce (or hold) the depth for the same work.
        assert!(prefetch_depth(0.040, 30.0, 6, 4) <= prefetch_depth(0.040, 30.0, 2, 4));
    }

    /// Dispatch is round-robin by prefetch distance, and panes whose lists run
    /// short simply drop out of later rounds (no padding, no reordering).
    #[test]
    fn prefetch_interleaves_by_distance() {
        // Pane 0 wants 3 frames, pane 1 wants 2, pane 2 wants 3.
        let plans = vec![
            (0, vec![10, 11, 12]),
            (1, vec![20, 21]),
            (2, vec![30, 31, 32]),
        ];
        assert_eq!(
            interleave_prefetch(&plans),
            vec![
                (0, 10), (1, 20), (2, 30), // distance 1: all panes
                (0, 11), (1, 21), (2, 31), // distance 2: all panes
                (0, 12), (2, 32),          // distance 3: pane 1 has dropped out
            ]
        );
    }

    /// An empty plan set (or all-empty lists) yields nothing.
    #[test]
    fn prefetch_interleave_handles_empty() {
        assert!(interleave_prefetch(&[]).is_empty());
        assert!(interleave_prefetch(&[(0, vec![]), (1, vec![])]).is_empty());
    }
}
