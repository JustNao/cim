//! Compute panes: generated media derived from other panes (mean / std of a
//! stack, per-pixel diff of two). Holds the recompute engine and the
//! auto-refresh signature check; the in-pane form is canvas/compute_ui.rs.

use super::*;

impl CimApp {
    /// Panes usable as a Compute source for `kind`: any non-compute pane, but
    /// the reductions (mean/std) also need ≥2 frames. `Diff` accepts stills.
    pub(super) fn compute_sources(&self, kind: Reduce) -> Vec<(u64, String)> {
        self.panes
            .iter()
            .filter(|p| p.compute.is_none())
            .filter(|p| matches!(kind, Reduce::Diff) || p.media.frame_count() > 1)
            .map(|p| (p.id, p.media.name().to_string()))
            .collect()
    }

    // ---- compute panes ---------------------------------------------------
    fn pane_idx(&self, id: u64) -> Option<usize> {
        self.panes.iter().position(|p| p.id == id)
    }

    /// Build the `@compute:<kind>:<srcs>[:auto]` view-command token for Compute
    /// pane `p`, or `None` if a source is no longer open (a dangling index would
    /// replay wrong). Sources are emitted as **pane indices** (0-based over the
    /// whole pane list), matching the positional per-pane flags.
    pub(super) fn compute_token(&self, p: &Pane) -> Option<String> {
        let c = p.compute.as_ref()?;
        let a = self.pane_idx(c.source_id?)?;
        let srcs = if matches!(c.kind, Reduce::Diff) {
            let b = self.pane_idx(c.source_b?)?;
            format!("{a},{b}")
        } else {
            a.to_string()
        };
        let mut tok = format!("@compute:{}:{}", c.kind.token(), srcs);
        if c.auto {
            tok.push_str(":auto");
        }
        Some(tok)
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
        let default_src = self
            .panes
            .get(prev)
            .filter(|p| p.compute.is_none())
            .map(|p| p.id);
        self.panes[i].compute = Some(Compute {
            kind: Reduce::Mean,
            source_id: default_src,
            source_b: None,
            computed: false,
            auto: false,
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
    /// given `kind` and auto-refresh flag, its sources left unset (the caller
    /// wires them once every pane exists). Returns the new pane's index.
    pub(super) fn add_configured_compute_pane(&mut self, kind: Reduce, auto: bool) -> usize {
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
            auto,
            last_sig: 0,
            saving: false,
            save_name: "computed.tif".into(),
            status: String::new(),
        });
        self.set_compute_tone_defaults(i);
        i
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
            .ok_or_else(|| "Source no longer available".to_string())?;
        let base = src.media.name().to_string();
        let cnt = src.media.frame_count();
        let frames: Vec<std::sync::Arc<media::FrameData>> =
            (0..cnt).filter_map(|f| src.media.resident(f)).collect();
        let used = frames.len();
        let fr =
            media::reduce_frames(&frames, kind).ok_or_else(|| "No source frames in memory".to_string())?;
        let name = format!("{} · {}", kind.label(), base);
        let status = format!("{} of {used} frame(s) in memory", kind.label());
        Ok((fr, name, status))
    }

    /// Per-pixel difference of two sources' *current* frames → (frame, name,
    /// status). Both current frames must be resident and share size/channels.
    fn compute_diff(
        &self,
        a_id: Option<u64>,
        b_id: Option<u64>,
    ) -> Result<(media::FrameData, String, String), String> {
        let a_id = a_id.ok_or_else(|| "Pick source A".to_string())?;
        let b_id = b_id.ok_or_else(|| "Pick source B".to_string())?;
        let ia = self.pane_idx(a_id).ok_or_else(|| "Source A no longer available".to_string())?;
        let ib = self.pane_idx(b_id).ok_or_else(|| "Source B no longer available".to_string())?;
        let (fa, fb) = (self.frame_disp(ia), self.frame_disp(ib));
        let a = self.panes[ia]
            .media
            .resident(fa)
            .ok_or_else(|| "A's current frame not in memory".to_string())?;
        let b = self.panes[ib]
            .media
            .resident(fb)
            .ok_or_else(|| "B's current frame not in memory".to_string())?;
        let fr = media::diff_frames(&a, &b)
            .ok_or_else(|| "A and B differ in size / channels".to_string())?;
        let name = format!(
            "Diff · {} − {}",
            self.panes[ia].media.name(),
            self.panes[ib].media.name()
        );
        let status = format!("Diff of frame {} − {}", fa + 1, fb + 1);
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
        let result = match kind {
            Reduce::Diff => self.compute_diff(a, b),
            _ => self.compute_reduce(a, kind),
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

    /// A cheap signature of a Compute pane's inputs, so auto-refresh recomputes
    /// only when they change: the shown frames for `Diff`, the source's resident
    /// count for the reductions (which grows as playback decodes more frames).
    fn compute_sig(&self, idx: usize) -> u64 {
        let Some(c) = self.panes[idx].compute.as_ref() else {
            return 0;
        };
        let frame_sig = |id: Option<u64>| -> u64 {
            id.and_then(|id| self.pane_idx(id))
                .map(|i| self.frame_disp(i) as u64 + 1)
                .unwrap_or(0)
        };
        match c.kind {
            Reduce::Diff => (frame_sig(c.source_id) << 32) ^ frame_sig(c.source_b),
            _ => c
                .source_id
                .and_then(|id| self.pane_idx(id))
                .map(|i| self.panes[i].media.resident_count() as u64)
                .unwrap_or(0),
        }
    }

    /// Recompute every auto-refresh Compute pane whose inputs changed this frame.
    pub(super) fn refresh_auto_compute(&mut self) {
        for i in 0..self.panes.len() {
            let Some(c) = self.panes[i].compute.as_ref() else {
                continue;
            };
            if c.auto && self.compute_sig(i) != c.last_sig {
                self.recompute_pane(i);
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
            self.set_compute_status(idx, "Nothing computed to save".into());
            return;
        };
        match media::save_frame(&frame, Path::new(name)) {
            Ok(()) => {
                if let Some(c) = self.panes[idx].compute.as_mut() {
                    c.saving = false;
                }
                self.set_compute_status(idx, format!("Saved {name}"));
            }
            Err(e) => self.set_compute_status(idx, format!("Save failed: {e}")),
        }
    }

    fn set_compute_status(&mut self, idx: usize, msg: String) {
        if let Some(c) = self.panes[idx].compute.as_mut() {
            c.status = msg;
        }
    }
}
