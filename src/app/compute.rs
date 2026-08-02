//! Compute panes: generated media derived from other panes (mean / std of a
//! stack, per-pixel add / subtract of two). Holds the recompute engine and the
//! auto-refresh signature check; the in-pane form is canvas/compute_ui.rs.
//!
//! A Compute result is itself a valid source, so panes chain: reduce a sequence
//! to its mean, then subtract that mean from the sequence. Chains recompute in
//! dependency order (`refresh_auto_compute`) and can't be wired into a cycle
//! (`compute_sources` / `compute_source_id`, both gated on `depends_on`).

use super::*;

impl CimApp {
    /// Panes usable as a Compute source for pane `idx` under `kind`: any pane
    /// except the pane itself and anything that (transitively) already depends
    /// on it, since that would close a recompute cycle. The binary ops accept
    /// stills — including another Compute pane's result — while the reductions
    /// (mean/std) need a real stack, so they also require ≥2 frames.
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
            last_sig: 0,
            saving: false,
            save_name: "computed.tif".into(),
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
            last_sig: 0,
            saving: false,
            save_name: "computed.tif".into(),
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

    /// Per-pixel add / subtract of two sources' *current* frames → (frame, name,
    /// status). Both current frames must be resident and share size/channels.
    ///
    /// Each source contributes whatever frame it is showing, so a **still** (one
    /// frame — a loaded image, or another Compute pane's result) pairs with the
    /// sequence's *current* frame: as the sequence advances, auto-refresh
    /// recomputes and the still is applied to each of its frames in turn.
    fn compute_binary(
        &self,
        kind: Reduce,
        a_id: Option<u64>,
        b_id: Option<u64>,
    ) -> Result<(media::FrameData, String, String), String> {
        let a_id = a_id.ok_or_else(|| t!("compute.err_pick", slot = "A").into_owned())?;
        let b_id = b_id.ok_or_else(|| t!("compute.err_pick", slot = "B").into_owned())?;
        let ia = self
            .pane_idx(a_id)
            .ok_or_else(|| t!("compute.err_slot_gone", slot = "A").into_owned())?;
        let ib = self
            .pane_idx(b_id)
            .ok_or_else(|| t!("compute.err_slot_gone", slot = "B").into_owned())?;
        let (fa, fb) = (self.frame_disp(ia), self.frame_disp(ib));
        let a = self.panes[ia]
            .media
            .resident(fa)
            .ok_or_else(|| t!("compute.err_frame_missing", slot = "A").into_owned())?;
        let b = self.panes[ib]
            .media
            .resident(fb)
            .ok_or_else(|| t!("compute.err_frame_missing", slot = "B").into_owned())?;
        let fr = media::combine_frames(&a, &b, kind)
            .ok_or_else(|| t!("compute.err_shape_mismatch").into_owned())?;
        let name = format!(
            "{} · {} {} {}",
            kind.label(),
            self.panes[ia].media.name(),
            kind.sign(),
            self.panes[ib].media.name()
        );
        let status = t!(
            "compute.status_binary",
            sign = kind.sign(),
            a = fa + 1,
            b = fb + 1
        )
        .into_owned();
        Ok((fr, name, status))
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
        };
        match result {
            Ok((fr, name, status)) => {
                self.panes[idx].media = media::Media::still(name, fr);
                // Bump the data generation rather than clearing `tex`: `stage`
                // re-renders the new result into `pending` while the last frame
                // keeps showing, so an auto-refreshing pane never flashes black
                // (nulling `tex` would blank a large/off-thread render until it
                // lands). The commit swaps in the fresh frame once it's ready.
                self.panes[idx].render_gen = self.panes[idx].render_gen.wrapping_add(1);
                self.panes[idx].hist = None; // recompute for the new result

                if let Some(c) = self.panes[idx].compute.as_mut() {
                    c.computed = true; // switch from the config form to the result
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
    /// when they change: the shown frames for the binary ops, the source's
    /// resident count for the reductions (which grows as playback decodes more
    /// frames). A source that is *itself* a Compute pane contributes its data
    /// generation instead, since its result changes without its frame index
    /// moving — that's what propagates a recompute along a chain.
    fn compute_sig(&self, idx: usize) -> u64 {
        let Some(c) = self.panes[idx].compute.as_ref() else {
            return 0;
        };
        let src_sig = |id: Option<u64>, stack: bool| -> u64 {
            let Some(i) = id.and_then(|id| self.pane_idx(id)) else {
                return 0;
            };
            let p = &self.panes[i];
            if p.compute.is_some() {
                p.render_gen + 1
            } else if stack {
                p.media.resident_count() as u64
            } else {
                self.frame_disp(i) as u64 + 1
            }
        };
        if c.kind.is_binary() {
            (src_sig(c.source_id, false) << 32) ^ src_sig(c.source_b, false)
        } else {
            src_sig(c.source_id, true)
        }
    }

    /// Recompute every Compute pane whose inputs changed this frame (they all
    /// refresh automatically — there is no per-pane toggle).
    ///
    /// Chains are handled by iterating to a fixed point rather than by sorting:
    /// a downstream pane's signature folds in its upstream's data generation, so
    /// once the upstream recomputes the downstream is seen as stale on the next
    /// pass. A chain is at most `panes.len()` deep, which bounds the passes.
    pub(super) fn refresh_auto_compute(&mut self) {
        for _ in 0..self.panes.len() {
            let mut again = false;
            for i in 0..self.panes.len() {
                let Some(c) = self.panes[i].compute.as_ref() else {
                    continue;
                };
                // Only once the pane has been computed at least once — an
                // unconfigured pane must keep showing its form until the user
                // presses Compute.
                if c.computed && self.compute_sig(i) != c.last_sig {
                    self.recompute_pane(i);
                    again = true;
                }
            }
            if !again {
                break;
            }
        }
    }

    /// Write the computed image to `name` (relative to the working dir), leaving
    /// the result in memory. Format follows the extension (.tif/.png/.jpg).
    pub(super) fn save_computed(&mut self, idx: usize, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            self.set_compute_status(idx, "Enter a file name".into());
            return;
        }
        let Some(frame) = self.panes[idx].media.resident(0) else {
            self.set_compute_status(idx, t!("compute.nothing_to_save").into_owned());
            return;
        };
        match media::save_frame(&frame, Path::new(name)) {
            Ok(()) => {
                if let Some(c) = self.panes[idx].compute.as_mut() {
                    c.saving = false;
                }
                self.set_compute_status(idx, t!("compute.saved", name = name).into_owned());
            }
            Err(e) => self.set_compute_status(idx, t!("compute.save_failed", err = e).into_owned()),
        }
    }

    fn set_compute_status(&mut self, idx: usize, msg: String) {
        if let Some(c) = self.panes[idx].compute.as_mut() {
            c.status = msg;
        }
    }
}
