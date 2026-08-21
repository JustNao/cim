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
                    // The TIFF path splits file I/O from CPU decompress (`d.io`);
                    // a standalone-file job can't and reports it all as decode.
                    if debug {
                        self.metrics.decode.record(d.elapsed.saturating_sub(d.io));
                        if !d.io.is_zero() {
                            self.metrics.read.record(d.io);
                        }
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
                    //
                    // Only at the *true* frontier. The frontier is probed several
                    // pages ahead at once (`probe_ahead`), so a probe past the real
                    // end can land while earlier pages are still in flight —
                    // ending the sequence on it would record a length short of the
                    // pages that do exist. `Decoded::Exists` is already safe this
                    // way (`note_len` only grows at `idx == len`); this is the same
                    // rule for the other outcome. A dropped result costs nothing:
                    // the probe is simply re-issued once the frontier reaches it.
                    if let Some(p) = self.panes.iter_mut().find(|p| p.id == d.id) {
                        p.media.frontier_ended(d.frame);
                    }
                }
                Err(e) => {
                    if let Some(p) = self.panes.iter_mut().find(|p| p.id == d.id) {
                        p.error = Some(t!("error.frame", n = d.frame + 1, err = e).into_owned());
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

    /// Probe the next `count` undiscovered pages at once, rather than one per
    /// update.
    ///
    /// Discovery is inherently serial — `SeqCache::note_len` grows only at
    /// `idx == len`, so page N+1 isn't confirmed until N is. But that does *not*
    /// mean it must cost a **UI round trip** per page, which is what one probe per
    /// update cost: request → worker → `request_repaint` → drain → `note_len` →
    /// next update. That loop latency, not decode speed, is what capped playback
    /// and "Load all" of a not-yet-discovered sequence (~20 fps against 60+ once
    /// the length was known — the pool simply ran dry between frames). Probes are
    /// header-only and pipeline through the file's reader, so a run of them
    /// collapses those round trips into one.
    ///
    /// Over-probing is safe by construction: a result landing ahead of the
    /// frontier is dropped — `note_len` ignores `idx != len`, and `Decoded::End`
    /// is guarded the same way in `pump_decoder` — and simply re-issued when the
    /// frontier reaches it. The cost of a wasted probe is a few hundred bytes.
    ///
    /// A `ConcatSeq` cannot be probed ahead: an undiscovered global index has no
    /// known `(file, page)` until the ones before it land, so `probe_job` returns
    /// `None` past the frontier and this quietly does the single probe it always
    /// did.
    pub(super) fn probe_ahead(&mut self, i: usize, count: usize) {
        if self.panes[i].media.at_end() {
            return;
        }
        let known = self.panes[i].media.frame_count();
        for f in known..known.saturating_add(count.max(1)) {
            self.probe(i, f);
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
        self.status.set_load(t!("status.load_all_queued"));
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
        self.status.set_load(t!("status.discovering_length"));
        self.decoding_all = true;
    }

    /// Queue a background fast-offset scan for pane `i` when its length is still
    /// being discovered and its layout might be fast-scannable (a lone TIFF or a
    /// concatenation). Runs off the UI thread (`crate::offsets`); the result is
    /// applied by `pump_offset_scans`. Media that isn't fast-scannable Errs there
    /// and is left to lazy discovery — no classic probe storm is started, and no
    /// file I/O touches the UI thread (only a cheap variant match here).
    pub(super) fn request_offset_scan(&mut self, i: usize) {
        let Some(p) = self.panes.get_mut(i) else {
            return;
        };
        if p.media.at_end() {
            return; // length already fully known (a still / numbered run / done)
        }
        let Some(paths) = media::offset_paths(&p.media) else {
            return; // not a lazily-discovered TIFF sequence
        };
        self.offset_gen += 1;
        let gen = self.offset_gen;
        p.offset_scan = Some(gen);
        let id = p.id;
        self.scanner.request(id, gen, paths);
    }

    /// Drain finished background offset scans and apply their page counts to the
    /// still-matching pane. A scan whose generation no longer matches (the pane
    /// was reloaded, or closed) is discarded; an `Err` (layout not fast-scannable)
    /// is ignored, leaving the sequence to discover its length lazily.
    pub(super) fn pump_offset_scans(&mut self) {
        for done in self.scanner.drain() {
            let Some(i) = self.panes.iter().position(|p| p.id == done.id) else {
                continue; // pane closed
            };
            if self.panes[i].offset_scan != Some(done.gen) {
                continue; // superseded by a reload / newer scan
            }
            self.panes[i].offset_scan = None;
            if let Ok(counts) = done.result {
                // Skip if lazy discovery already reached the end meanwhile; else
                // apply (a ConcatSeq re-verifies against the discovered prefix).
                if !self.panes[i].media.at_end() {
                    let _ = media::apply_offset_counts(&mut self.panes[i].media, &counts);
                }
            }
        }
    }

    /// Cancel any in-progress bulk load ("Load all" / "Load offsets").
    pub(super) fn stop_load(&mut self) {
        for p in &mut self.panes {
            p.eager = Eager::Off;
        }
        self.decoding_all = false;
        self.export_load_pending = false;
        // Flipping `eager` only stops *queuing* new frames; a "Load all" over an
        // already-length-known (e.g. offsets-loaded) sequence has already queued
        // its whole backlog, and the worker pool would keep grinding through it.
        // Cancel that backlog and drop the now-orphaned inflight markers so the
        // frames the user actually views re-request cleanly.
        self.decoder.cancel_pending();
        self.inflight.clear();
        self.status.set(t!("status.load_stopped"));
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
                        // Extend the known length by **probing** a run of pages,
                        // never by decoding one. Two reasons: a decode landing at
                        // `idx > len` is dropped by `insert` (which only grows the
                        // length contiguously), so speculating with decodes would
                        // throw away whole frame reads; and probing several at once
                        // lets the loop above queue many frames next update instead
                        // of one, which is the difference between the pool running
                        // dry between frames and running back to back.
                        self.probe_ahead(i, FRONTIER_PROBES);
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
                        // Headers only, no pixel decode — and a run of them, so
                        // discovery advances by more than one page per update.
                        self.probe_ahead(i, FRONTIER_PROBES);
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
        let i = self.loop_control();
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
        // The loop-driving pane drives the shared timeline/scrubber even when it
        // isn't on screen, so it must keep discovering its frontier too.
        let mut targets = self.displayed_indices();
        let ctrl = self.loop_control();
        if !targets.contains(&ctrl) {
            targets.push(ctrl);
        }
        // How far past the shown frame the frontier must stay discovered. Browsing
        // needs one page. **While playing, keep a whole prefetch window ahead**:
        // `prefetch_playback` never queues past the known length, so a frontier
        // only a page or two out leaves it able to see exactly one frame — the
        // decode pool then empties between frames and playback runs at the update
        // loop's round-trip latency instead of the decode rate. With a
        // fast-forward stride the window is measured in strides, so the next
        // strided target (`frame + ff`) is always already known and
        // `advance_playback` can skim rather than landing on every frontier frame.
        let ff = self.playback.fast_forward.max(1);
        let margin = if self.playback.playing {
            ff * FRONTIER_PROBES + 1
        } else {
            2
        };
        for i in targets {
            if self.panes[i].eager != Eager::Off || self.panes[i].media.at_end() {
                continue; // a bulk load (drive_eager) already drives this pane's frontier
            }
            let known = self.panes[i].media.frame_count();
            if self.catching_up(i) {
                // Target far past the frontier (a sequence behind an advanced
                // timeline): discover with metadata-only probes so the pages in
                // between aren't decoded — only the target lands (see `stage`).
                self.probe_ahead(i, FRONTIER_PROBES);
            } else if self.frame_disp(i) + margin > known {
                // At the frontier. **While playing** (or skimming with a stride)
                // extend it by header probes, a run at a time: the pages crossed
                // must not be decoded, and the length has to stay far enough ahead
                // for `prefetch_playback` to have anything to queue. Frames that
                // will actually be shown are decoded by prefetch/`stage`.
                //
                // Browsing, keep decoding the single next page instead — stepping
                // frame by frame wants it resident, not merely known.
                if self.playback.playing || ff > 1 {
                    self.probe_ahead(i, FRONTIER_PROBES);
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
        // Same window as `advance_playback`, including its hold at the slowest
        // still-discovering pane's frontier, so prefetch only ever asks for frames
        // playback will actually land on.
        let (tl, at_end) = self.playback_limit();
        let (lo, hi) = self.loop_bounds(tl);
        let full = self.playback.loop_range.is_none();

        // Same targets as lookahead: on-screen panes plus the loop-driving pane
        // (which drives the shared timeline even when it isn't displayed).
        let mut targets = self.displayed_indices();
        let ctrl = self.loop_control();
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
            self.resolve_decode_threads(),
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
                    // Mirror `advance_playback`'s stride decision exactly, so prefetch
                    // requests only the frames playback will actually land on — never a
                    // partial stride onto the undiscovered frontier (which would decode
                    // a frame the stride is meant to skim).
                    f = if f + ff <= hi {
                        f + ff // a full stride fits inside the discovered window
                    } else if f < hi && (!full || at_end) {
                        hi // final short stride onto a real window end
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
            self.status.set_load(t!("status.cache_full"));
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
            if self.status.is_load_note() {
                self.status.clear();
            }
            if std::mem::take(&mut self.export_load_pending) && self.load_cache_exhausted {
                self.warn_popup = Some(t!("warn.cache_too_small").into_owned());
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
            if debug {
                self.metrics.lut.record(d.lut_time);
                if !d.ops_time.is_zero() {
                    self.metrics.operators.record(d.ops_time);
                }
            }
            // A viewport-region render lands in the region cache (it decorates
            // the base, never replacing it); a whole-image render is the pane's
            // next base texture.
            match d.target {
                crate::renderer::Target::Viewport => self.land_region(ctx, d),
                crate::renderer::Target::Base => {
                    self.render_inflight.remove(&d.id);
                    if let Some(idx) = self.panes.iter().position(|p| p.id == d.id) {
                        self.upload_tex(ctx, idx, d);
                    }
                }
            }
        }
    }

    /// Store a finished viewport-region render in the region cache under the key
    /// `render_region` filed when it queued the job. The pane may be gone or the
    /// view moved on — the cache keeps it regardless; a stale region simply stops
    /// being painted and ages out by LRU.
    fn land_region(&mut self, ctx: &egui::Context, d: crate::renderer::RenderDone) {
        // One region render per pane at a time (`render_region`), so the guard —
        // and the key — are keyed on the pane, not on the region's identity. A
        // result with no entry belongs to a pane closed or reloaded mid-flight.
        let Some(key) = self.roi_inflight.remove(&d.id) else {
            return;
        };
        // `roi_plan` refuses a region larger than the backend accepts, so this
        // can only differ if the limit shrank mid-flight; drop it rather than
        // assert inside the upload, and the next update re-plans.
        let side = max_side(ctx);
        if d.image.size[0] > side || d.image.size[1] > side {
            return;
        }
        self.upload_region(ctx, key, d.image);
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
        let now = ctx.input(|i| i.time);
        let max_side = max_side(ctx);
        let mut all_ready = true;
        // `(pane, frame, adaptive)` — the commit re-derives each pane's step and
        // must reach the same answer `stage` did, so it reuses the plan's verdict
        // rather than re-planning against state that may have moved.
        let mut staged: Vec<(usize, usize, bool)> = Vec::with_capacity(panes.len());
        for &idx in &panes {
            // A pane discovering toward a far target holds its last committed
            // frame (keeps `tex`) instead of flipping through the pages in
            // between — `ensure_lookahead` probes it forward, and it stages
            // normally once the target itself is discovered.
            if self.catching_up(idx) {
                continue;
            }
            // A pane whose view hasn't been fitted yet is still at the default
            // zoom of 1, which stages the frame at **full resolution** — for a
            // very large image, gigabytes of texture that the fit is about to
            // make unnecessary. The fit happens in `draw_pane`, i.e. after this,
            // so wait the one frame (and ask for it, since an idle app requests
            // no repaint). Every pane in `displayed_indices` is drawn — and
            // therefore fitted — so this can't spin.
            if self.view_ref(idx).needs_fit {
                ctx.request_repaint();
                continue;
            }
            let target = self.stage_target(idx);
            // Planned **once** per pane per update and handed to everything that
            // needs it: the base's `want_step` (which caps and decimates only for
            // an adaptive pane), the region staging, and the commit below. They
            // would each reach the same answer on their own, but only by
            // recomputing the same geometry three times and staying in step by
            // discipline.
            let plan = self.roi_plan(idx, target, ppp, max_side);
            if !self.stage(ctx, idx, target, ppp, plan.is_some()) {
                all_ready = false;
            }
            // The adaptive viewport region is staged with the base and gates the
            // same commit, so the two layers a pane draws always come from one
            // frame (§7.1). A non-adaptive pane reports ready immediately.
            if !self.stage_region(ctx, idx, target, plan, now) {
                all_ready = false;
            }
            staged.push((idx, target, plan.is_some()));
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
        for (idx, target, adaptive) in staged {
            let sig = self.tone_sig(idx);
            let step = self.want_step(idx, target, ppp, max_side, adaptive);
            self.panes[idx]
                .tex
                .commit(|t| t.shown == target && t.sig == sig && t.step == step);
            // Promote the staged region alongside its base, so drawing never
            // pairs a freshly committed base with the previous frame's region.
            self.panes[idx].region_show = self.panes[idx].region_want;
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
    pub(super) fn stage_target(&self, idx: usize) -> usize {
        crate::tone::synced_index(
            self.playback.prefetch.unwrap_or(self.shared_frame),
            self.panes[idx].media.frame_count(),
            self.panes[idx].sync_temporal,
            self.panes[idx].frame,
        )
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
    pub(super) fn stage_step(&self, idx: usize, ppp: f32) -> usize {
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
    ///
    /// Zoom is not the only floor: a texture also has to **fit the backend's
    /// limit** (`GL_MAX_TEXTURE_SIZE`, 16384 on the software GL these panes run
    /// on over VNC), so an image wider than that is decimated however far in it
    /// is zoomed — `texture_fit_step`. Without it a 25000² tile at 1:1 asked
    /// egui for a 25000² texture and the upload asserted, taking the process
    /// down. A heavy pane keeps `step 1` regardless (decimating changes what the
    /// operator computes); `stage` refuses that upload instead of decimating it.
    ///
    /// **Adaptive rendering** changes both rules for the pane's *base* texture,
    /// since the sharp pixels then come from the viewport region
    /// (`roi::stage_region`). [`roi::base_step`] is the single definition of the
    /// result, and `roi_plan` weighs the mode's cost with that very same
    /// function, so the step this renders at and the step the plan was costed
    /// against cannot disagree.
    ///
    /// Two different triggers, deliberately:
    ///
    /// - A **plain** pane is capped at [`roi::BASE_MAX`] only while `adaptive`
    ///   says the region path is actually carrying it. Its role shrinks to the
    ///   blurry pan fallback, so a 12000² image stops costing a 12000² upload.
    /// - An **operator** pane takes the reduced base while `adaptive` carries it
    ///   *or* when the full-resolution texture would not fit the backend at all
    ///   — the latter is what lets it show a huge image (at `step 1` `stage`
    ///   refuses the upload outright and the pane shows `tex_error`), and a
    ///   `BASE_MAX` base with nothing over it still beats an undisplayable pane.
    ///   Where full resolution *does* fit, an operator pane whose plan declined
    ///   falls back to the classic `step 1` render exactly as it does with the
    ///   setting off. It used to take the cap on the setting alone, which left
    ///   any ordinary-sized operator pane (e.g. 3000x4096, well inside the
    ///   16384 limit) showing a 188x256 base magnified over the whole cell as
    ///   soon as the zoom dropped below `roi_plan`'s engagement point — the
    ///   permanently-blurry failure, and only for the operator tones.
    fn want_step(
        &self,
        idx: usize,
        target: usize,
        ppp: f32,
        max_side: usize,
        adaptive: bool,
    ) -> usize {
        base_texture_step(
            self.staged_size(idx, target),
            self.stage_step(idx, ppp),
            max_side,
            self.pane_ops_active(idx),
            self.config.adaptive_render,
            adaptive,
        )
    }

    /// The pixel size of the frame `stage` is about to render for pane `idx` —
    /// the frame itself when it is resident, else the pane's displayed size.
    /// `want_step` reads it, so `stage` and the commit must agree on it: both
    /// are called with the same `(idx, target)` within one `refresh_textures`,
    /// where residency doesn't change.
    pub(super) fn staged_size(&self, idx: usize, target: usize) -> [usize; 2] {
        match self.panes[idx].media.resident(target) {
            Some(f) => f.size,
            None => self.disp_size(idx),
        }
    }

    /// Render pane `idx`'s texture for frame `target` **without disturbing what's
    /// currently shown** (`tex`): the result lands in `pending`, to be committed
    /// by `refresh_textures`. Returns whether `target` is ready — already in `tex`,
    /// or staged in `pending`.
    ///
    /// The plain LUT render (Linear / Linear+Clip, and masks) of a small or
    /// decimated frame is cheap and stays **synchronous**. The heavy proprietary
    /// operators (LUT_ALPHA / details) — and a plain LUT of a **large**
    /// full-resolution frame (`ASYNC_RENDER_PIXELS`), which is itself tens of
    /// milliseconds — render on the [`RenderPool`] and land in `pending` via
    /// `pump_render`, so neither a slow operator nor a big frame blocks the UI
    /// thread. An errored pane reports ready so it can't stall a lockstep commit.
    fn stage(
        &mut self,
        ctx: &egui::Context,
        idx: usize,
        target: usize,
        ppp: f32,
        adaptive: bool,
    ) -> bool {
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
        let max_side = max_side(ctx);
        let step = self.want_step(idx, target, ppp, max_side, adaptive);
        // Last line of defence before the upload. `want_step` already decimates a
        // large frame into the backend's limit, so this only fires where it
        // *can't*: a heavy proprietary-operator pane, which must render at full
        // resolution (decimating changes what the operator computes). Say so on
        // the pane rather than handing egui a texture whose upload asserts and
        // takes the process down. Recomputed before the early-outs below, so it
        // clears itself as soon as the pane can be drawn again.
        let size = self.staged_size(idx, target);
        let out = decimated_size(size, step);
        self.panes[idx].tex_error =
            (out[0] > max_side || out[1] > max_side).then(|| too_large_msg(size, max_side));
        if self.panes[idx].tex_error.is_some() {
            return true; // like an errored pane: never stall the lock-step commit
        }
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
        // Colormap is a plain (mono-only) palette render, done synchronously; it
        // takes precedence over the proprietary operators (details is ignored).
        let cmap = crate::tone::uses_colormap(contrast, &frame);
        // The proprietary operators only run on single-channel 16-bit frames with
        // the library loaded; otherwise LUT_ALPHA / Details fall back to a plain
        // render, so there's nothing heavy to push off-thread.
        let ops = self.ops_of(idx);
        let heavy = !cmap && crate::imageproc::ops_active(&frame, ops);
        // A plain LUT render of a *large* frame is itself tens of milliseconds —
        // done synchronously it blocks this whole update, which reads as a
        // regular hitch when playback steps while the user pans at 60 Hz. Push
        // it to the render pool (the worker's plain-LUT path is pixel-identical
        // by test), leaving only the texture upload on the UI thread.
        //
        // Measured on the **output** texel count, which is what the render
        // actually writes: a heavily minified pane stays synchronous because its
        // output is genuinely small, while a lightly decimated render of a very
        // large frame is still tens of megapixels and belongs off-thread. This
        // deliberately does *not* require `step == 1` — the worker renders at the
        // job's region (`renderer::RenderJob::region`) and `upload_tex` tags the
        // result with its step, so a decimated result commits like any other. Demanding
        // `step == 1` here put an **adaptive** pane's `BASE_MAX`-capped base
        // render (§7.1) back on the UI thread on every playback frame, where it
        // also lost `fill_lut`'s parallel path (which needs a contiguous grid) —
        // panning while playing went sluggish for exactly that reason.
        let bulk = !heavy && out[0] * out[1] >= ASYNC_RENDER_PIXELS;

        // GPU mode takes those renders itself, Colormap included (as does the
        // CPU render pool now — see below). It is
        // synchronous but not expensive: the dispatch is queued rather than
        // awaited, and a frame already resident in VRAM (the pane is being
        // re-toned rather than stepped) uploads nothing at all — which is the
        // interaction the whole path exists for. Heavy panes are excluded
        // outright: the proprietary operators are CPU code owned by the pane's
        // render thread and cannot be part of this.
        //
        // `step == 1` **is** required here, unlike `bulk` itself: `tone_into`
        // always tone-maps the whole frame at full resolution and tags the
        // texture `step: 1`, so handing it a decimated pane would stage a
        // texture the commit can never match — re-rendering it every frame.
        if bulk && step == 1 && self.gpu.is_some() {
            let (lo, hi) = self.tone_bounds(idx, &frame);
            let tone = crate::gpu::Tone {
                lo,
                hi,
                palette: cmap.then(|| self.tone_of(idx).palette),
            };
            let pane_id = self.panes[idx].id;
            let t = crate::debug::enabled().then(std::time::Instant::now);
            let done = self.gpu.as_mut().expect("checked above").tone_into(
                pane_id,
                &frame,
                tone,
                &mut self.panes[idx].tex.pending,
                target,
                sig,
            );
            match done {
                Ok(()) => {
                    if let Some(t) = t {
                        self.metrics.lut.record(t.elapsed());
                    }
                    return true;
                }
                // The card refused the work (too large for its limits, or the
                // device went away). Hand this session back to the CPU for good
                // and render the frame below — the user sees a correct image,
                // just built the other way, and `CIM_DEBUG` says why.
                Err(e) => {
                    crate::debug::log(&format!("gpu: falling back to the CPU ({e})"));
                    self.gpu = None;
                }
            }
        }

        // Colormap used to be excluded here — the pool had no palette, so a
        // Colormap job would have come back grey. It carries one now
        // (`imageproc::Display`), so a big false-coloured frame goes off-thread
        // like any other.
        if heavy || bulk {
            // Render off-thread. One render per pane at a time, so rapid tone /
            // frame changes coalesce instead of piling up jobs.
            let id = self.panes[idx].id;
            if !self.render_inflight.contains(&id) {
                let (lo, hi) = self.tone_bounds(idx, &frame);
                self.renderer.request(crate::renderer::RenderJob {
                    id,
                    frame: target,
                    sig,
                    data: frame.clone(),
                    tone: crate::imageproc::Display {
                        lo,
                        hi,
                        palette: cmap.then(|| self.tone_of(idx).palette),
                        ops,
                    },
                    // The whole image; `step` is 1 except for an adaptive pane's
                    // capped base (`want_step` — an operator pane then runs on
                    // the reduced input, by design).
                    region: media::Region::whole(frame.size, step),
                    target: crate::renderer::Target::Base,
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
            // Rendered straight into egui's pixel type (see `media::RgbaSink`):
            // the buffer *becomes* the texture's `ColorImage`, so there's no
            // conversion pass between the tone map and the upload. Taking it
            // leaves the scratch empty — it's re-grown by the next render, which
            // reserves exactly once and writes every pixel once.
            let mut pixels = std::mem::take(&mut self.render_scratch);
            // On the budgeted pool: an undecimated render of this size splits
            // across cores (`media::render`), and this is the *synchronous* path
            // on the UI thread, so it must draw from the instance's share rather
            // than rayon's machine-sized global pool (`crate::cpu`).
            let palette = cmap.then(|| self.tone_of(idx).palette);
            let region = media::Region::whole(frame.size, step);
            let lut = &mut self.panes[idx].tex.lut;
            crate::cpu::install(|| match palette {
                // Colormap: false-colour the mono frame through the palette.
                Some(pal) => frame.render_cmap(lo, hi, region, pal, lut, &mut pixels),
                None => frame.render_lut(lo, hi, region, lut, &mut pixels),
            });
            if let Some(t) = t {
                self.metrics.lut.record(t.elapsed());
            }
            let t = debug.then(std::time::Instant::now);
            let img = ColorImage {
                size: region.out,
                pixels,
            };
            let name = format!("m{}", self.panes[idx].id);
            let native = frame.size; // `region.out` above is the decimated texel count
            set_cached_tex(
                &mut self.panes[idx].tex.pending,
                ctx,
                name,
                img,
                target,
                native,
                sig,
                step,
            );
            if let Some(t) = t {
                self.metrics.upload.record(t.elapsed());
            }
            true
        }
    }

    /// Stage a worker-rendered image as pane `idx`'s **pending** texture, tagged
    /// with the result's `(frame, sig, step)` identity (committed to the front by
    /// `refresh_textures`). The worker already did the `ColorImage` conversion
    /// copy, so the UI thread only queues the texture delta here (the recorded
    /// upload time reflects that).
    fn upload_tex(&mut self, ctx: &egui::Context, idx: usize, d: crate::renderer::RenderDone) {
        // A frame that outgrew the backend between the request and its landing
        // is dropped here rather than uploaded (see `stage`).
        let max_side = max_side(ctx);
        if d.image.size[0] > max_side || d.image.size[1] > max_side {
            self.panes[idx].tex_error = Some(too_large_msg(d.image.size, max_side));
            return;
        }
        let t = crate::debug::enabled().then(std::time::Instant::now);
        let name = format!("m{}", self.panes[idx].id);
        set_cached_tex(
            &mut self.panes[idx].tex.pending,
            ctx,
            name,
            d.image,
            d.frame,
            d.native,
            d.sig,
            d.region.step,
        );
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
            ContrastMode::Colormap => 2,
        };
        let tone = self.tone_of(idx);
        c.hash(&mut h);
        // The clip toggle and its percentile both change the Linear mapping.
        tone.clip.enabled.hash(&mut h);
        tone.clip.percent.to_bits().hash(&mut h);
        // "Share clip" locks the bounds to the Control media's; the effective
        // bounds then move with the Control pane's frame and clip settings, so
        // fold that pane's identity in (cheap — its frame Arc pointer, clip and
        // region inputs, not the computed percentile) so this pane re-renders
        // whenever the Control's bounds would change.
        tone.share_clip.hash(&mut h);
        if tone.share_clip {
            if let Some((ci, ptr)) = self.control_frame_key() {
                ci.hash(&mut h);
                ptr.hash(&mut h);
                let ct = self.tone_of(ci);
                self.contrast_of(ci).label().hash(&mut h);
                ct.clip.enabled.hash(&mut h);
                ct.clip.percent.to_bits().hash(&mut h);
                let creg = self.panes[ci].region_tone;
                creg.hash(&mut h);
                if creg {
                    self.stats_gen.hash(&mut h);
                }
            }
        }
        // The Colormap palette changes the rendered colour.
        tone.palette.id().hash(&mut h);
        self.details_of(idx).hash(&mut h);
        let region = self.panes[idx].region_tone;
        region.hash(&mut h);
        if region {
            // Region-tone bounds move with the shared stats region.
            self.stats_gen.hash(&mut h);
        }
        // An export crop (Export panel open) restricts every non-LUT_ALPHA pane's
        // LUT to that region (`own_tone_bounds`), so fold it in to re-render when
        // the crop changes or clears — and, transitively, for Share-clip panes
        // whose Control adopts it.
        if self.export.show && self.contrast_of(idx) != ContrastMode::LutAlpha {
            if let Some(reg) = self.export.region {
                for v in [reg.min.x, reg.min.y, reg.max.x, reg.max.y] {
                    v.to_bits().hash(&mut h);
                }
            }
        }
        // A Compute recompute swaps in new frame data at the same index/tone; the
        // generation makes `stage` re-render it (into `pending`, keeping the last
        // `tex`) instead of treating the texture as still current.
        self.panes[idx].render_gen.hash(&mut h);
        h.finish()
    }

    /// The linear display bounds `[lo, hi]` for pane `idx`'s current tone: the
    /// per-tail percentile clip, the full range / float extent, or — when
    /// region-tone is pinned — the shared stats region's bounds. With "Share
    /// clip" on, the Control media's bounds are used instead so panes lock to
    /// identical bounds.
    pub(super) fn tone_bounds(&self, idx: usize, frame: &media::FrameData) -> (f32, f32) {
        let contrast = self.contrast_of(idx);
        let tone = self.tone_of(idx);
        // "Share clip" locks the bounds to the Control media's own bounds (but
        // not for LUT_ALPHA, which does its own contrast). Falls through to this
        // pane's own bounds when the Control frame isn't resident yet.
        if contrast != ContrastMode::LutAlpha && tone.share_clip {
            if let Some(b) = self.control_clip_bounds() {
                return b;
            }
        }
        self.own_tone_bounds(idx, frame)
    }

    /// A pane's *own* display bounds (its clip / full-range map, or its region
    /// bounds when region-tone is pinned) — ignoring "Share clip", so it can be
    /// read for the Control media itself without recursing.
    fn own_tone_bounds(&self, idx: usize, frame: &media::FrameData) -> (f32, f32) {
        let clip = crate::tone::clip_pct(self.contrast_of(idx), &self.tone_of(idx));
        let region = self.tone_region(idx);
        // On the budgeted pool: an unmemoized bound runs a whole-image percentile
        // scan, which splits across cores, and this is called from the UI thread
        // (`crate::cpu`). Memoized hits never reach the scan, so this is cheap.
        crate::cpu::install(|| crate::tone::frame_bounds(frame, clip, region))
    }

    /// The region pane `idx`'s display bounds are computed over, or `None` for
    /// the whole frame. **Policy, not maths** — the maths is `tone::frame_bounds`
    /// — and the export snapshots the result of this same method
    /// (`export_ui::export_pane`) rather than restating the precedence:
    ///
    /// 1. an **export crop**, while the Export panel is open, so the live view
    ///    previews the region-restricted tone the export composites;
    /// 2. else the pinned **stats region**, when this pane has region-tone on;
    /// 3. else nothing.
    ///
    /// LUT_ALPHA is excluded throughout: it runs over the whole image with its
    /// own contrast.
    pub(super) fn tone_region(&self, idx: usize) -> Option<Rect> {
        if self.contrast_of(idx) == ContrastMode::LutAlpha {
            return None;
        }
        if self.export.show {
            if let Some(reg) = self.export.region {
                return Some(reg);
            }
        }
        if self.panes[idx].region_tone {
            self.stats_region
        } else {
            None
        }
    }

    /// The Control media's currently shown frame (its pane index + `Arc`), if
    /// resident. The source of the shared bounds for any "Share clip" pane.
    fn control_frame(&self) -> Option<(usize, std::sync::Arc<media::FrameData>)> {
        if self.panes.is_empty() {
            return None;
        }
        let c = self.control.min(self.panes.len() - 1);
        let f = self.frame_disp(c);
        self.panes[c].media.resident(f).map(|fr| (c, fr))
    }

    /// A cheap identity key for the Control media's shown frame (pane index +
    /// frame `Arc` pointer), used by `tone_sig` so a "Share clip" pane re-renders
    /// when the Control navigates or reloads.
    pub(super) fn control_frame_key(&self) -> Option<(usize, usize)> {
        self.control_frame()
            .map(|(c, fr)| (c, std::sync::Arc::as_ptr(&fr) as usize))
    }

    /// The Control media's own display bounds `[lo, hi]` for its current frame,
    /// applied to any pane with "Share clip" on. `None` when the Control frame
    /// isn't resident yet (the pane then falls back to its own bounds).
    pub(super) fn control_clip_bounds(&self) -> Option<(f32, f32)> {
        let (c, fr) = self.control_frame()?;
        Some(self.own_tone_bounds(c, &fr))
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
            return self.panes[idx].overlay_tex.as_ref().map(|t| t.image.id());
        };
        // Never stretch a mismatched overlay onto the base: if the source frame's
        // size differs from this pane's current frame, skip drawing it. (A newly
        // selected mismatched source is rejected up front with an error popup, so
        // this only guards later per-frame size drift in a sequence.)
        if frame.size != self.disp_size(idx) {
            return None;
        }
        // An overlay is never decimated (it must line up 1:1 with the base
        // image), so one too large for the backend simply isn't drawn — the
        // same silent skip as the size mismatch above, rather than an assert
        // inside the upload.
        let side = max_side(ctx);
        if frame.size[0] > side || frame.size[1] > side {
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
            // tints by normalised intensity; a colour image draws its own colours
            // with pure black keyed out (§9).
            if frame.color_channels() == 3 {
                frame.render_color_rgba(alpha, &mut buf);
            } else if frame.is_mask() {
                frame.render_mask_rgba(rgb, alpha, &mut buf);
            } else {
                frame.render_intensity_rgba(rgb, alpha, &mut buf);
            }
            let img = ColorImage::from_rgba_unmultiplied(frame.size, &buf);
            // Overlay textures don't tone-map (sig 0) and aren't decimated (step 1);
            // `disp_size` never reads the overlay slot, so its size is unused.
            let name = format!("ov{}_{}", idx, src_id);
            set_cached_tex(
                &mut self.panes[idx].overlay_tex,
                ctx,
                name,
                img,
                f,
                frame.size,
                0,
                1,
            );
        }
        Some(self.panes[idx].overlay_tex.as_ref().unwrap().image.id())
    }
}

/// The largest texture side the render backend accepts — `GL_MAX_TEXTURE_SIZE`
/// on the glow path (16384 on the software GL a VNC session runs on), as egui
/// reports it. eframe refreshes it every frame from the painter; until it has,
/// egui's own conservative default stands, which only ever over-decimates.
pub(super) fn max_side(ctx: &egui::Context) -> usize {
    match ctx.input(|i| i.max_texture_side) {
        0 => FALLBACK_MAX_TEXTURE_SIDE,
        n => n,
    }
}

/// Assumed texture limit should the backend report none at all. Every GL 3+ /
/// WebGL2 implementation guarantees at least this.
const FALLBACK_MAX_TEXTURE_SIDE: usize = 2048;

/// The texel count a `step`-decimated render of `size` produces — the app-side
/// name for [`media::Region::whole`]'s sizing, which is where the rule lives
/// (every `step`-th sample on each axis, so a partial last step still lands).
pub(super) fn decimated_size(size: [usize; 2], step: usize) -> [usize; 2] {
    media::Region::whole(size, step).out
}

/// The decimation step for a pane's whole-image texture — `want_step`'s decision
/// as a pure function, so it can be pinned headlessly (see the doc comment there
/// for why the two operator arms differ).
///
/// `ops` is whether the proprietary operators run on this pane, `setting` the
/// **Adaptive rendering** config flag, and `adaptive` whether `roi_plan` actually
/// engaged for this pane this update — i.e. whether a viewport region is being
/// staged over the base.
pub(super) fn base_texture_step(
    size: [usize; 2],
    stage_step: usize,
    max_side: usize,
    ops: bool,
    setting: bool,
    adaptive: bool,
) -> usize {
    match (ops, setting) {
        // Decimating an operator's input changes its output, so outside adaptive
        // mode a heavy pane stays at full resolution and `stage` refuses an
        // oversized upload rather than decimating it.
        (true, false) => 1,
        // A region is carrying the sharp pixels: take the cheap base.
        (true, true) if adaptive => roi::base_step(stage_step, size, max_side, true),
        // No region over it. Only accept a reduced base where full resolution
        // could not be uploaded at all; otherwise this pane renders classically.
        (true, true) if texture_fit_step(size, max_side) > 1 => {
            roi::base_step(stage_step, size, max_side, true)
        }
        (true, true) => 1,
        (false, _) if adaptive => roi::base_step(stage_step, size, max_side, false),
        (false, _) => stage_step.max(texture_fit_step(size, max_side)),
    }
}

/// The smallest decimation step that brings `size` within `max_side` on both
/// axes — 1 when it already fits.
pub(super) fn texture_fit_step(size: [usize; 2], max_side: usize) -> usize {
    if max_side == 0 {
        return 1;
    }
    size[0].max(size[1]).div_ceil(max_side).max(1)
}

/// The message a pane shows when its image can't be made into a texture at all.
fn too_large_msg(size: [usize; 2], max_side: usize) -> String {
    t!(
        "error.image_too_large",
        w = size[0],
        h = size[1],
        limit = max_side
    )
    .into_owned()
}

/// Set (or create) a cached texture slot from a freshly rendered image, tagging
/// it with the frame it shows and its tone signature. Shared by the pane image,
/// the tinted overlay, and the off-thread render upload, so the set-or-create
/// dance (and the `NEAREST` filtering the tool depends on) lives in one place.
#[allow(clippy::too_many_arguments)]
fn set_cached_tex(
    slot: &mut Option<CachedTex>,
    ctx: &egui::Context,
    name: String,
    img: ColorImage,
    shown: usize,
    size: [usize; 2],
    sig: u64,
    step: usize,
) {
    let opts = TextureOptions::NEAREST;
    // Reuse the egui-managed handle when there is one, so a playback run doesn't
    // allocate a texture per frame. A slot left holding a *GPU* texture (the
    // backend gave up mid-session, or this pane shrank below the GPU threshold)
    // is replaced wholesale — it has no handle to write into, and dropping it
    // releases its registration.
    match slot {
        Some(CachedTex {
            image: TexImage::Managed(handle),
            ..
        }) => {
            handle.set(img, opts);
        }
        _ => {
            *slot = Some(CachedTex {
                image: TexImage::Managed(ctx.load_texture(name, img, opts)),
                shown,
                size,
                sig,
                step,
            });
            return;
        }
    }
    let t = slot.as_mut().expect("just matched a populated slot");
    t.shown = shown;
    t.size = size;
    t.sig = sig;
    t.step = step;
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
    use super::{
        base_texture_step, decimated_size, interleave_prefetch, prefetch_depth, texture_fit_step,
        PLAY_PREFETCH,
    };

    /// An operator pane whose `roi_plan` declined must fall back to the classic
    /// full-resolution render, not sit on the `BASE_MAX`-capped base with nothing
    /// over it. Taking the cap on the *setting* alone left a 3000x4096 LUT_ALPHA /
    /// details pane showing a 188x256 base magnified over the whole cell for every
    /// zoom below the engagement point, while the same pane on Linear was perfect.
    #[test]
    fn an_operator_pane_without_a_region_renders_full_resolution() {
        let fits = [3000, 4096]; // well inside a 16384 backend
        let huge = [25000, 25000]; // cannot be uploaded at step 1

        // Setting on, plan declined, image fits: the classic full-resolution
        // render, exactly as with the setting off.
        assert_eq!(base_texture_step(fits, 1, 16384, true, true, false), 1);
        assert_eq!(base_texture_step(fits, 1, 16384, true, false, false), 1);

        // The one case the reduced base exists for: full resolution could not be
        // uploaded at all, so a blurry pane beats an undisplayable one.
        assert!(texture_fit_step(huge, 16384) > 1);
        assert!(base_texture_step(huge, 1, 16384, true, true, false) > 1);

        // With a region carrying the sharp pixels, the cheap base is taken.
        assert!(base_texture_step(fits, 1, 16384, true, true, true) > 1);

        // A plain pane is unaffected either way, and was never blurry here.
        assert_eq!(base_texture_step(fits, 1, 16384, false, true, false), 1);
        assert_eq!(base_texture_step(fits, 1, 16384, false, false, false), 1);
    }

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
                (0, 10),
                (1, 20),
                (2, 30), // distance 1: all panes
                (0, 11),
                (1, 21),
                (2, 31), // distance 2: all panes
                (0, 12),
                (2, 32), // distance 3: pane 1 has dropped out
            ]
        );
    }

    /// An empty plan set (or all-empty lists) yields nothing.
    #[test]
    fn prefetch_interleave_handles_empty() {
        assert!(interleave_prefetch(&[]).is_empty());
        assert!(interleave_prefetch(&[(0, vec![]), (1, vec![])]).is_empty());
    }

    /// The whole point of the clamp: whatever the step, the texture the backend
    /// is handed fits inside its limit. Checked across a spread of sizes rather
    /// than the one that prompted it, since the arithmetic is the risk (a
    /// `floor` here silently leaves the texture one texel too wide).
    #[test]
    fn the_fit_step_always_brings_a_frame_within_the_limit() {
        for &limit in &[2048usize, 4096, 8192, 16384] {
            for &side in &[
                1usize, 100, 2047, 2048, 2049, 5000, 16384, 16385, 25000, 100_000,
            ] {
                let size = [side, side / 2 + 1];
                let step = texture_fit_step(size, limit);
                let out = decimated_size(size, step);
                assert!(
                    out[0] <= limit && out[1] <= limit,
                    "{side} at limit {limit}: step {step} -> {out:?}"
                );
                // …and no more decimation than that needs.
                if step > 1 {
                    let looser = decimated_size(size, step - 1);
                    assert!(
                        looser[0] > limit || looser[1] > limit,
                        "{side} at limit {limit}: step {step} is one more than needed"
                    );
                }
            }
        }
    }

    /// An image that already fits is left alone — crossing into the clamp must
    /// not disturb the full-resolution band the display staging depends on.
    #[test]
    fn a_frame_within_the_limit_is_not_decimated() {
        assert_eq!(texture_fit_step([16384, 16384], 16384), 1);
        assert_eq!(texture_fit_step([5000, 5000], 16384), 1);
        assert_eq!(texture_fit_step([1, 1], 16384), 1);
        // 25000² over a 16384 limit: every other sample, ~12500² of texture.
        assert_eq!(texture_fit_step([25000, 25000], 16384), 2);
        assert_eq!(decimated_size([25000, 25000], 2), [12500, 12500]);
        // A degenerate limit can't divide by zero.
        assert_eq!(texture_fit_step([100, 100], 0), 1);
    }
}
