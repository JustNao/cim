//! Compute panes: generated media derived from other panes (mean / std of a
//! stack, per-pixel add / subtract of two). Holds the recompute engine and the
//! auto-refresh signature check; the in-pane form is canvas/compute_ui.rs.
//!
//! A reduction is a still. A binary op is a **generated sequence**
//! (`Media::Computed`): one `A ± B` frame per timeline position, computed from
//! whatever input frames are in memory — the shown frame first, synchronously,
//! then the rest a slice per update (`drive_binary_compute`). Its frames sit in
//! the shared frame cache like any decoded frame, so they count against the
//! cache budget and are evicted by the same LRU.
//!
//! A Compute result is itself a valid source, so panes chain: reduce a sequence
//! to its mean, subtract that mean from the sequence, then take the std of the
//! difference. Chains recompute in dependency order (`refresh_auto_compute`)
//! and can't be wired into a cycle (`compute_sources` / `compute_source_id`,
//! both gated on `depends_on`).

use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;

/// UI-thread time a single update may spend filling generated sequences beyond
/// their shown frames. The shown (and about-to-show) frames are computed
/// regardless; this only paces the background fill, so a long sequence fills
/// over a few dozen updates instead of freezing one.
const FILL_BUDGET: Duration = Duration::from_millis(12);

impl CimApp {
    /// Panes usable as a Compute source for pane `idx` under `kind`: any pane
    /// except the pane itself and anything that (transitively) already depends
    /// on it, since that would close a recompute cycle. The binary ops accept
    /// stills — a mean/std result among them — while the reductions need a
    /// real stack, so they also require ≥2 frames; an add/sub result is a
    /// sequence, so it feeds either.
    pub(super) fn compute_sources(&self, idx: usize, kind: Reduce) -> Vec<(u64, String)> {
        let me = self.panes[idx].id;
        self.panes
            .iter()
            .filter(|p| p.id != me && !self.depends_on(p.id, me))
            .filter(|p| kind.is_binary() || p.media.frame_count() > 1)
            .map(|p| (p.id, p.media.name().to_string()))
            .collect()
    }

    // ---- compute panes ---------------------------------------------------
    fn pane_idx(&self, id: u64) -> Option<usize> {
        self.panes.iter().position(|p| p.id == id)
    }

    /// The Compute sources of pane `id`, if it is a Compute pane.
    fn compute_inputs(&self, id: u64) -> [Option<u64>; 2] {
        match self
            .pane_idx(id)
            .and_then(|i| self.panes[i].compute.as_ref())
        {
            Some(c) => [c.source_id, c.source_b],
            None => [None, None],
        }
    }

    /// Whether pane `id` reads pane `target`, directly or through a chain of
    /// Compute panes. `id == target` counts (a pane trivially depends on
    /// itself), so this doubles as the self-source check. The walk is bounded by
    /// the pane count, so even a cycle wired in from a stale view command can't
    /// spin here.
    fn depends_on(&self, id: u64, target: u64) -> bool {
        let mut seen: Vec<u64> = Vec::new();
        let mut stack = vec![id];
        while let Some(cur) = stack.pop() {
            if cur == target {
                return true;
            }
            if seen.contains(&cur) {
                continue;
            }
            seen.push(cur);
            stack.extend(self.compute_inputs(cur).into_iter().flatten());
        }
        false
    }

    /// Build the `compute:<kind>:<srcs>` view-command token for Compute pane
    /// `p`, or `None` if a source is no longer open (a dangling index would
    /// replay wrong). Sources are emitted as **pane indices** (0-based over the
    /// whole pane list), matching the positional per-pane flags. (No `@` prefix:
    /// a leading `@` is PowerShell's splatting operator and would mangle the arg.)
    pub(super) fn compute_token(&self, p: &Pane) -> Option<String> {
        let c = p.compute.as_ref()?;
        let a = self.pane_idx(c.source_id?)?;
        let srcs = if c.kind.is_binary() {
            let b = self.pane_idx(c.source_b?)?;
            format!("{a},{b}")
        } else {
            a.to_string()
        };
        Some(format!("compute:{}:{}", c.kind.token(), srcs))
    }

    /// Add a new, *unconfigured* Compute pane (from the toolbar "Compute"
    /// button). It shows the in-pane config form (mode + source pickers + a
    /// Compute button); the result appears once that button computes it.
    pub(super) fn add_compute_pane(&mut self) {
        let was_empty = self.panes.is_empty();
        self.add_pane(
            media::Media::still("Compute".into(), media::placeholder_frame()),
            Source::Computed,
        );
        let i = self.panes.len() - 1;
        // Default source A to the previously focused pane when it can be one.
        let prev = self.current.min(i.saturating_sub(1));
        // The default mode is Mean, so only a real stack makes a usable default.
        let default_src = self
            .panes
            .get(prev)
            .filter(|p| prev != i && p.media.frame_count() > 1)
            .map(|p| p.id);
        self.panes[i].compute = Some(Compute {
            kind: Reduce::Mean,
            source_id: default_src,
            source_b: None,
            computed: false,
            armed: false, // the form's Compute button arms it
            last_sig: 0,
            filling: false,
            status: String::new(),
        });
        self.set_compute_tone_defaults(i);
        self.current = i;
        if was_empty {
            self.shared_view.needs_fit = true;
        }
    }

    /// Recreate a Compute pane from a view command: a fresh Compute pane with the
    /// given `kind`, its sources left unset (the caller wires them once every
    /// pane exists). Returns the new pane's index.
    pub(super) fn add_configured_compute_pane(&mut self, kind: Reduce) -> usize {
        self.add_pane(
            media::Media::still("Compute".into(), media::placeholder_frame()),
            Source::Computed,
        );
        let i = self.panes.len() - 1;
        self.panes[i].compute = Some(Compute {
            kind,
            source_id: None,
            source_b: None,
            computed: false,
            // Replayed from a view command: already configured, so it retries
            // until its sources (and any upstream Compute pane) are ready.
            armed: true,
            last_sig: 0,
            filling: false,
            status: String::new(),
        });
        self.set_compute_tone_defaults(i);
        i
    }

    /// Resolve a replayed source **pane index** to a stable id for Compute pane
    /// `idx`: `None` when the index is out of range, names the pane itself, or
    /// names a pane that already reads it (which would close a recompute cycle).
    pub(super) fn compute_source_id(&self, idx: usize, src: usize) -> Option<u64> {
        let me = self.panes[idx].id;
        let id = self.panes.get(src)?.id;
        (!self.depends_on(id, me)).then_some(id)
    }

    /// A Compute result is its own thing (a derived still), so it doesn't follow
    /// the shared Transformations by default — it carries its own tone: a plain
    /// Linear LUT with no clip and no share clip. The user can still opt it into
    /// the synced group or dial in a clip afterward.
    fn set_compute_tone_defaults(&mut self, i: usize) {
        self.panes[i].sync_tone = false;
        self.panes[i].contrast = ContrastMode::Linear;
        self.panes[i].tone.clip.enabled = false;
        self.panes[i].tone.share_clip = false;
    }

    /// Mean/std reduction of a source's resident frames → (frame, name, status).
    fn compute_reduce(
        &self,
        source_id: Option<u64>,
        kind: Reduce,
    ) -> Result<(media::FrameData, String, String), String> {
        let src_id = source_id.ok_or_else(|| "Pick a source sequence".to_string())?;
        let src = self
            .panes
            .iter()
            .find(|p| p.id == src_id)
            .ok_or_else(|| t!("compute.err_source_gone").into_owned())?;
        let base = src.media.name().to_string();
        let cnt = src.media.frame_count();
        let frames: Vec<std::sync::Arc<media::FrameData>> =
            (0..cnt).filter_map(|f| src.media.resident(f)).collect();
        let used = frames.len();
        let fr = media::reduce_frames(&frames, kind)
            .ok_or_else(|| t!("compute.err_no_frames").into_owned())?;
        let name = format!("{} · {}", kind.label(), base);
        let status = t!("compute.status_reduce", kind = kind.label(), n = used).into_owned();
        Ok((fr, name, status))
    }

    /// A fresh, empty generated sequence for an add / subtract of two sources →
    /// (media, status). Its frames are filled in afterwards by
    /// `drive_binary_compute`, the shown one first.
    ///
    /// Result frame `k` combines the frame each input shows at timeline
    /// position `k` (`binary_input_frame`), so a **still** (one frame — a loaded
    /// image, or a mean/std result) pairs with every frame of a sequence.
    fn compute_binary(
        &self,
        kind: Reduce,
        a_id: Option<u64>,
        b_id: Option<u64>,
    ) -> Result<(media::Media, String), String> {
        let a_id = a_id.ok_or_else(|| t!("compute.err_pick", slot = "A").into_owned())?;
        let b_id = b_id.ok_or_else(|| t!("compute.err_pick", slot = "B").into_owned())?;
        let ia = self
            .pane_idx(a_id)
            .ok_or_else(|| t!("compute.err_slot_gone", slot = "A").into_owned())?;
        let ib = self
            .pane_idx(b_id)
            .ok_or_else(|| t!("compute.err_slot_gone", slot = "B").into_owned())?;
        let name = format!(
            "{} · {} {} {}",
            kind.label(),
            self.panes[ia].media.name(),
            kind.sign(),
            self.panes[ib].media.name()
        );
        let len = self.binary_span(ia, ib);
        let media = media::Media::computed(name, self.panes[ia].media.size(), len);
        Ok((media, String::new()))
    }

    /// How many timeline positions a binary op over panes `a` and `b` spans: the
    /// longer input's length, where a temporally unsynced input (pinned to its
    /// own frame) counts as one — the live mirror of the export's timeline.
    fn binary_span(&self, a: usize, b: usize) -> usize {
        let span = |j: usize| {
            let p = &self.panes[j];
            if p.sync_temporal {
                p.media.frame_count()
            } else {
                1
            }
        };
        span(a).max(span(b)).max(1)
    }

    /// The frame input pane `j` contributes to result frame `k`: what it shows
    /// at timeline position `k` (`frame_disp`'s rule, and the export's).
    fn binary_input_frame(&self, j: usize, k: usize) -> usize {
        let p = &self.panes[j];
        crate::tone::synced_index(k, p.media.frame_count(), p.sync_temporal, p.frame)
    }

    /// Whether input pane `j` has discovered far enough for result frame `k`.
    /// A synced input still discovering its length short of `k` would clamp to
    /// its last known frame (`binary_input_frame`), which is a *different*
    /// frame from the one it will show at `k` — so frame `k` must wait rather
    /// than be computed (and marked done) against the wrong input. This is what
    /// a view command's `--frame` hits: the seek discovers only the Control, so
    /// the other input is far behind the timeline when the result is built. An
    /// upstream generated sequence has reached `k` when its own inputs have.
    fn binary_input_reaches(&self, j: usize, k: usize, depth: usize) -> bool {
        let p = &self.panes[j];
        if !p.sync_temporal || k < p.media.frame_count() {
            return true;
        }
        match self.binary_inputs(j) {
            Some((_, a, b)) => {
                depth <= self.panes.len()
                    && self.binary_input_reaches(a, k, depth + 1)
                    && self.binary_input_reaches(b, k, depth + 1)
            }
            None => p.media.at_end(),
        }
    }

    /// Push input pane `j`'s length discovery toward result frame `k` — the
    /// input may not be on screen, so nothing else would (`ensure_lookahead`
    /// only discovers displayed panes). Probes are header-only.
    fn discover_binary_input(&mut self, j: usize, k: usize, depth: usize) {
        if depth > self.panes.len() || self.binary_input_reaches(j, k, depth) {
            return;
        }
        match self.binary_inputs(j) {
            Some((_, a, b)) => {
                self.discover_binary_input(a, k, depth + 1);
                self.discover_binary_input(b, k, depth + 1);
            }
            None => self.probe_ahead(j, FRONTIER_PROBES),
        }
    }

    /// `(kind, A pane, B pane)` of pane `i` when it is a binary Compute pane
    /// whose generated sequence exists and whose sources are both open.
    fn binary_inputs(&self, i: usize) -> Option<(Reduce, usize, usize)> {
        let c = self.panes[i].compute.as_ref()?;
        if !c.kind.is_binary() || !c.armed || !self.panes[i].media.is_computed() {
            return None;
        }
        Some((
            c.kind,
            self.pane_idx(c.source_id?)?,
            self.pane_idx(c.source_b?)?,
        ))
    }

    /// Compute frame `k` of binary pane `i` from its inputs' resident frames and
    /// store it (touched now, so the LRU keeps it over older frames). `false`
    /// when an input frame isn't resident or the two don't combine (size /
    /// channel mismatch — reported on the pane).
    fn compute_binary_frame(
        &mut self,
        i: usize,
        k: usize,
        kind: Reduce,
        a: usize,
        b: usize,
    ) -> bool {
        let (fa, fb) = (self.binary_input_frame(a, k), self.binary_input_frame(b, k));
        let (Some(x), Some(y)) = (
            self.panes[a].media.resident(fa),
            self.panes[b].media.resident(fb),
        ) else {
            return false;
        };
        match media::combine_frames(&x, &y, kind) {
            Some(fr) => {
                let clock = self.clock;
                self.panes[i].media.insert(k, Arc::new(fr));
                self.panes[i].media.touch(k, clock);
                true
            }
            None => {
                self.set_compute_status(i, t!("compute.err_shape_mismatch").into_owned());
                false
            }
        }
    }

    /// Make frame `k` of binary pane `i` resident if it can be: compute it when
    /// both inputs are in memory, else ask for what's missing — a decode for a
    /// file-backed input, the same treatment (recursively) for an upstream
    /// generated sequence. Returns whether the frame is resident afterwards.
    fn ensure_binary_frame(&mut self, i: usize, k: usize, depth: usize) -> bool {
        if self.panes[i].media.resident(k).is_some() {
            return true;
        }
        let Some((kind, a, b)) = self.binary_inputs(i) else {
            return false;
        };
        // A chain is at most `panes.len()` deep (no cycles), which bounds this.
        if depth > self.panes.len() || k >= self.panes[i].media.frame_count() {
            return false;
        }
        let mut ready = true;
        for j in [a, b] {
            if !self.binary_input_reaches(j, k, 0) {
                self.discover_binary_input(j, k, 0);
                ready = false;
                continue;
            }
            let f = self.binary_input_frame(j, k);
            let have = self.panes[j].media.resident(f).is_some()
                || if self.panes[j].media.is_computed() {
                    self.ensure_binary_frame(j, f, depth + 1)
                } else {
                    self.request(j, f); // no-op for a still, deduped when in flight
                    false
                };
            ready &= have;
        }
        ready && self.compute_binary_frame(i, k, kind, a, b)
    }

    /// Whether frame `k` of generated pane `i` is still on its way — every
    /// missing input frame is being decoded (or, upstream, generated) — so the
    /// lock-step commit should wait for it. `false` once it can't arrive: an
    /// input errored or is gone, or both inputs are resident yet don't combine.
    pub(super) fn binary_frame_expected(&self, i: usize, k: usize, depth: usize) -> bool {
        let Some((_, a, b)) = self.binary_inputs(i) else {
            return false;
        };
        if depth > self.panes.len() {
            return false;
        }
        let mut missing = false;
        for j in [a, b] {
            if !self.binary_input_reaches(j, k, 0) {
                // Still discovering toward `k` (unless that stopped on an error).
                if self.panes[j].error.is_some() {
                    return false;
                }
                missing = true;
                continue;
            }
            let f = self.binary_input_frame(j, k);
            if self.panes[j].media.resident(f).is_some() {
                continue;
            }
            missing = true;
            let p = &self.panes[j];
            let coming = p.error.is_none()
                && if p.media.is_computed() {
                    self.binary_frame_expected(j, f, depth + 1)
                } else {
                    p.media.decode_job(f).is_some()
                };
            if !coming {
                return false;
            }
        }
        missing
    }

    /// Advance every binary Compute pane's generated sequence, in two passes:
    ///
    /// 1. **The shown frame** (and the one playback is about to show) is made
    ///    resident right away — computed if its inputs are in memory, their
    ///    decode requested if not — so the pane is instant wherever the timeline
    ///    is, without working through the frames before it.
    /// 2. **The fill**: every other frame whose inputs are already in memory,
    ///    walking forward from the shown frame (the way playback goes), then
    ///    back, within [`FILL_BUDGET`]. Only frames not yet computed since the
    ///    sequence was built — one the budget evicted comes back when shown, not
    ///    by the fill, or the two would chase each other.
    ///
    /// Returns whether fill work was left over, so the caller asks for another
    /// update rather than waiting for input.
    fn drive_binary_compute(&mut self) -> bool {
        let panes: Vec<usize> = (0..self.panes.len())
            .filter(|&i| self.binary_inputs(i).is_some())
            .collect();
        if panes.is_empty() {
            return false;
        }
        // Keep each sequence as long as its inputs' (lazily discovered) timeline.
        for &i in &panes {
            if let Some((_, a, b)) = self.binary_inputs(i) {
                let span = self.binary_span(a, b);
                self.panes[i].media.grow_computed(span);
            }
        }
        // While a seek rides the frontier the shown frame is only a waypoint:
        // asking for it would full-decode every input page the seek passes
        // (which it walks by header alone), so wait for it to land.
        let seeking = self.pending_seek.is_some();
        for &i in panes.iter().filter(|_| !seeking) {
            let (shown, next) = (self.frame_disp(i), self.stage_target(i));
            self.ensure_binary_frame(i, shown, 0);
            if next != shown {
                self.ensure_binary_frame(i, next, 0);
            }
        }

        let deadline = Instant::now() + FILL_BUDGET;
        let mut busy = false;
        for &i in &panes {
            let Some((kind, a, b)) = self.binary_inputs(i) else {
                continue;
            };
            let len = self.panes[i].media.frame_count();
            let start = self.frame_disp(i).min(len - 1);
            let mut out_of_time = false;
            for k in (start..len).chain((0..start).rev()) {
                if self.panes[i].media.computed_done(k)
                    || !self.binary_input_reaches(a, k, 0)
                    || !self.binary_input_reaches(b, k, 0)
                {
                    continue;
                }
                let (fa, fb) = (self.binary_input_frame(a, k), self.binary_input_frame(b, k));
                if self.panes[a].media.resident(fa).is_none()
                    || self.panes[b].media.resident(fb).is_none()
                {
                    continue; // only what's already in memory
                }
                if Instant::now() >= deadline {
                    out_of_time = true;
                    break;
                }
                if !self.compute_binary_frame(i, k, kind, a, b) {
                    break; // a mismatch fails every frame alike; it's on the pane
                }
            }
            busy |= out_of_time;
            let n = self.panes[i].media.resident_count();
            if let Some(c) = self.panes[i].compute.as_mut() {
                c.filling = out_of_time;
                // Keep a mismatch report up; otherwise say how much is in memory.
                if c.status != t!("compute.err_shape_mismatch") {
                    c.status = t!("compute.status_binary", n = n).into_owned();
                }
            }
        }
        busy
    }

    /// Recompute a Compute pane from current memory, replacing its displayed
    /// still. The pane keeps its own (un-synced) tone — Linear LUT, no clip, no
    /// share clip by default (see `add_compute_pane`) — so a recompute never
    /// clobbers a look the user has since dialled in. The input signature is
    /// recorded either way, so auto-refresh doesn't spin on failure.
    pub(super) fn recompute_pane(&mut self, idx: usize) {
        let Some(c) = self.panes[idx].compute.as_ref() else {
            return;
        };
        let (kind, a, b) = (c.kind, c.source_id, c.source_b);
        let result = if kind.is_binary() {
            self.compute_binary(kind, a, b)
        } else {
            self.compute_reduce(a, kind)
                .map(|(fr, name, status)| (media::Media::still(name, fr), status))
        };
        match result {
            Ok((mut m, status)) => {
                // Carry the Scale target over, so a scaled Compute pane's fresh
                // result isn't treated as a Scale change (which drops `tex`).
                let fit = self.panes[idx].media.scale_to();
                m.set_scale_to(fit);
                self.panes[idx].media = m;
                // Bump the data generation rather than clearing `tex`: `stage`
                // re-renders the new result into `pending` while the last frame
                // keeps showing, so an auto-refreshing pane never flashes black
                // (nulling `tex` would blank a large/off-thread render until it
                // lands). The commit swaps in the fresh frame once it's ready.
                self.panes[idx].render_gen = self.panes[idx].render_gen.wrapping_add(1);
                self.panes[idx].hist = None; // recompute for the new result
                self.panes[idx].error = None;
                // Its preview thumbnails show the previous result.
                self.thumb_cache.forget_pane(self.panes[idx].id);

                if let Some(c) = self.panes[idx].compute.as_mut() {
                    c.computed = true; // switch from the config form to the result
                                       // A new generated sequence is empty: hold a downstream
                                       // reduction until the fill has had its go at it.
                    c.filling = kind.is_binary();
                }
                self.set_compute_status(idx, status);
            }
            Err(msg) => self.set_compute_status(idx, msg),
        }
        let sig = self.compute_sig(idx);
        if let Some(c) = self.panes[idx].compute.as_mut() {
            c.last_sig = sig;
        }
    }

    /// A cheap signature of a Compute pane's inputs, so a recompute happens only
    /// when they change. Every source contributes its data generation
    /// (`render_gen`: bumped when a Compute result is rebuilt or a file
    /// reloaded — that's what propagates a recompute along a chain) and its
    /// Scale target. On top of that:
    /// - a **reduction** folds in the source's resident count, which grows as
    ///   playback decodes (or a generated sequence fills) more frames;
    /// - a **binary op** folds in how each input maps the timeline (synced, or
    ///   pinned to which frame) — but *not* the shown frame: its result is a
    ///   whole sequence, rebuilt only when what any of its frames would be
    ///   changes. Moving along the timeline just computes more of it.
    fn compute_sig(&self, idx: usize) -> u64 {
        let Some(c) = self.panes[idx].compute.as_ref() else {
            return 0;
        };
        let mut h = std::collections::hash_map::DefaultHasher::new();
        c.kind.token().hash(&mut h);
        for id in [c.source_id, c.source_b] {
            id.hash(&mut h);
            let Some(p) = id.and_then(|id| self.pane_idx(id)).map(|i| &self.panes[i]) else {
                continue;
            };
            p.render_gen.hash(&mut h);
            p.media.scale_to().hash(&mut h);
            if c.kind.is_binary() {
                p.sync_temporal.hash(&mut h);
                if !p.sync_temporal {
                    p.frame.hash(&mut h);
                }
            } else {
                p.media.resident_count().hash(&mut h);
            }
        }
        h.finish()
    }

    /// A reduction reading a generated sequence that is still filling: it waits,
    /// rather than re-reducing an ever-growing stack every update, and runs once
    /// the fill settles (its source's resident count then says so).
    fn waits_on_fill(&self, idx: usize) -> bool {
        let Some(c) = self.panes[idx].compute.as_ref() else {
            return false;
        };
        !c.kind.is_binary()
            && c.source_id
                .and_then(|id| self.pane_idx(id))
                .and_then(|i| self.panes[i].compute.as_ref())
                .is_some_and(|src| src.filling)
    }

    /// Recompute every Compute pane whose inputs changed this frame (they all
    /// refresh automatically — there is no per-pane toggle), then advance the
    /// generated sequences (`drive_binary_compute`). Returns whether a sequence
    /// still has frames to fill, so the caller schedules another update.
    ///
    /// Chains are handled by iterating to a fixed point rather than by sorting:
    /// a downstream pane's signature folds in its upstream's data generation, so
    /// once the upstream recomputes the downstream is seen as stale on the next
    /// pass. A chain is at most `panes.len()` deep, which bounds the passes.
    pub(super) fn refresh_auto_compute(&mut self) -> bool {
        for _ in 0..self.panes.len() {
            let mut again = false;
            for i in 0..self.panes.len() {
                let Some(c) = self.panes[i].compute.as_ref() else {
                    continue;
                };
                // Only a pane the user (or a view command) has actually asked
                // to compute — an unconfigured one keeps showing its form.
                if c.armed && !self.waits_on_fill(i) && self.compute_sig(i) != c.last_sig {
                    self.recompute_pane(i);
                    again = true;
                }
            }
            if !again {
                break;
            }
        }
        self.drive_binary_compute()
    }

    fn set_compute_status(&mut self, idx: usize, msg: String) {
        if let Some(c) = self.panes[idx].compute.as_mut() {
            c.status = msg;
        }
    }
}
