//! Media lifecycle: opening (dialog / paths / CLI inputs), adding & removing
//! panes, reloading from disk, and the view-state replay / "View cmd" round
//! trip (`apply_view_state` / `view_command`).

use super::*;

impl CimApp {
    /// Apply a viewpoint parsed from the command line (see `cli::ViewState`).
    /// Called once after the startup files are opened. Only the fields that were
    /// present on the command line change anything; the rest keep their defaults.
    pub(super) fn apply_view_state(&mut self, vs: cli::ViewState) {
        if let Some(c) = vs.cols {
            self.config.max_columns = c.clamp(1, 8);
        }
        if let Some(m) = vs.mode {
            self.mode = match m {
                cli::ViewMode::Grid => Mode::Grid,
                cli::ViewMode::Single => Mode::Single,
                cli::ViewMode::Ab => Mode::Ab,
            };
        }
        let n = self.panes.len();
        if let Some(p) = vs.pane {
            if n > 0 {
                self.current = p.min(n - 1);
            }
        }
        if let Some((a, b, split)) = vs.ab {
            if n > 0 {
                self.slot_a = a.min(n - 1);
                self.slot_b = b.min(n - 1);
            }
            self.ab_split = split.clamp(0.02, 0.98);
        }
        if let Some(f) = vs.frame {
            // The sequence length isn't discovered yet, so we can't land on `f`
            // now — record it and let `drive_seek` walk discovery up to it.
            self.shared_frame = f;
            self.pending_seek = Some(f);
        }
        // Per-pane tone / detail (each list positional over the panes). These
        // are per-pane, so unsync those panes' Transformations (which default to
        // synced) — otherwise the restored per-pane tone wouldn't take effect.
        if let Some(tones) = &vs.tones {
            for (p, t) in self.panes.iter_mut().zip(tones) {
                p.contrast = match t {
                    cli::Tone::Linear => ContrastMode::Linear,
                    cli::Tone::LutAlpha => ContrastMode::LutAlpha,
                    cli::Tone::Colormap(pal) => {
                        p.tone.palette = *pal;
                        ContrastMode::Colormap
                    }
                };
                p.sync_tone = false;
                // Restored tone re-renders via `tone_sig`; no `tex` nulling (it
                // would flash black for a heavy LUT_ALPHA/details pane).
            }
        }
        // Per-pane Linear clip (`--clip`): a toggle + percentile. Like --tone this
        // is per-pane, so unsync the panes it sets.
        if let Some(clips) = &vs.clips {
            for (p, c) in self.panes.iter_mut().zip(clips) {
                match c {
                    cli::ClipSpec::Off => p.tone.clip.enabled = false,
                    cli::ClipSpec::On(pct) => {
                        p.tone.clip.enabled = true;
                        p.tone.clip.percent = *pct;
                    }
                }
                p.sync_tone = false;
            }
        }
        // Per-pane "Share clip" (`--share-clip`): lock the bounds to the Control
        // media's. Per-pane like --tone/--clip, so unsync the panes it sets.
        if let Some(shares) = &vs.share_clip {
            for (p, s) in self.panes.iter_mut().zip(shares) {
                p.tone.share_clip = *s;
                p.sync_tone = false;
            }
        }
        if let Some(details) = &vs.details {
            for (p, d) in self.panes.iter_mut().zip(details) {
                p.details = *d;
                p.sync_tone = false;
            }
        }
        // Per-pane rotation. Like --tone/--detail these are per-pane, so unsync
        // the panes they set (otherwise a synced pane would ignore its own angle
        // and follow the shared one); the following --tsync re-syncs and re-seeds.
        if let Some(rots) = &vs.rotations {
            for (p, &r) in self.panes.iter_mut().zip(rots) {
                p.rotation = wrap180(r);
                p.sync_tone = false;
            }
        }
        // Transformations sync flags, applied *after* per-pane tone/detail/rotation
        // (which unsync the panes they set). Re-seed the shared set from the first
        // synced pane so panes that follow it show the captured look.
        if let Some(sync) = &vs.tsync {
            if let Some(k) = sync.iter().position(|&s| s) {
                if let Some(p) = self.panes.get(k) {
                    self.shared_contrast = p.contrast;
                    self.shared_tone = p.tone;
                    self.shared_details = p.details;
                    self.shared_rotation = p.rotation;
                }
            }
            for (p, &s) in self.panes.iter_mut().zip(sync) {
                p.sync_tone = s;
                // Effective tone changed → re-renders via `tone_sig`; no nulling.
            }
        }
        if let Some(vis) = &vs.visible {
            for (p, &v) in self.panes.iter_mut().zip(vis) {
                p.visible = v;
            }
        }
        if let Some(c) = vs.control {
            if n > 0 {
                self.control = c.min(n - 1);
            }
        }
        if let Some((lo, hi)) = vs.loop_range {
            self.playback.loop_range = Some((lo, hi));
        }
        // A restored zoom/centre is an explicit view, so suppress the auto-fit
        // that would otherwise run on first draw.
        if vs.zoom.is_some() || vs.center.is_some() {
            if let Some(z) = vs.zoom {
                self.shared_view.zoom = z.clamp(1e-4, 512.0);
            }
            if let Some((x, y)) = vs.center {
                self.shared_view.center = Vec2::new(x, y);
            }
            self.shared_view.needs_fit = false;
        }
    }

    /// Build a `cim …` command line that reopens the current files at the
    /// current shared view. Captures the layout, columns, shared zoom/pan, the
    /// timeline frame, per-pane tone/detail/visibility/Transformations-sync, the
    /// focused and control panes, the loop range and (in A/B) the operands +
    /// split. Anything left at its default is omitted to keep the line short.
    ///
    /// Only the *shared* view is captured — panes with their own view (sync off)
    /// fall back to it. Sequences are listed as their individual files (the
    /// compact `PREFIX%0Xu…,…` token isn't reconstructed).
    pub(super) fn view_command(&self) -> String {
        let mut parts: Vec<String> = vec!["cim".into()];
        for p in &self.panes {
            // Re-emit a numbered sequence as its compact token so a replay
            // reopens it as one sequence (not a pane per file).
            match &p.source {
                Source::File(path) => parts.push(quote_path(path)),
                Source::Sequence { token, .. } => parts.push(quote_arg(token)),
                // A computed image isn't reproducible from a CLI path; skip it.
                Source::Computed => {}
            }
        }
        // Only emit a flag when it differs from the app's default, so the line
        // stays short. Layout:
        if self.mode != Mode::Grid {
            let mode = match self.mode {
                Mode::Grid => "grid",
                Mode::Single => "single",
                Mode::Ab => "ab",
            };
            parts.push(format!("--mode {mode}"));
        }
        if self.config.max_columns != Config::default().max_columns {
            parts.push(format!("--cols {}", self.config.max_columns));
        }
        // Zoom / centre are the point of the command (they capture where you are
        // in the image), so they're always emitted.
        let v = self.shared_view;
        parts.push(format!("--zoom {:.4}", v.zoom));
        parts.push(format!("--center {:.2},{:.2}", v.center.x, v.center.y));
        if self.timeline_len() > 1 && self.shared_frame != 0 {
            parts.push(format!("--frame {}", self.shared_frame));
        }
        let n = self.panes.len();
        if n > 0 {
            // Per-pane tone mode (effective — shared when tone-synced). The mode
            // is Linear for every pane unless LUT_ALPHA is chosen, so omit `--tone`
            // when no pane uses LUT_ALPHA.
            let tones: Vec<String> = (0..n)
                .map(|i| match self.contrast_of(i) {
                    ContrastMode::Linear => "linear".to_string(),
                    ContrastMode::LutAlpha => "lutalpha".to_string(),
                    ContrastMode::Colormap => {
                        format!("colormap:{}", self.tone_of(i).palette.token())
                    }
                })
                .collect();
            if (0..n).any(|i| tones[i] != "linear") {
                parts.push(format!("--tone {}", tones.join(",")));
            }
            // Per-pane Linear clip (effective): `off` or the per-tail percentile.
            // Omit when every pane is at its depth-appropriate default (on at
            // 0.01% for >8-bit, off for 8-bit).
            let clips: Vec<String> = (0..n)
                .map(|i| {
                    let clip = self.tone_of(i).clip;
                    if clip.enabled {
                        format!("{}", (clip.percent * 1000.0).round() / 1000.0)
                    } else {
                        "off".into()
                    }
                })
                .collect();
            let clip_default = |i: usize| -> &str {
                if self.panes[i].media.hi_depth() {
                    "0.01"
                } else {
                    "off"
                }
            };
            if (0..n).any(|i| clips[i].as_str() != clip_default(i)) {
                parts.push(format!("--clip {}", clips.join(",")));
            }
            // Per-pane "Share clip" (effective): 1/0. Omit when no pane shares
            // (the default).
            if (0..n).any(|i| self.tone_of(i).share_clip) {
                let shares: Vec<&str> = (0..n)
                    .map(|i| if self.tone_of(i).share_clip { "1" } else { "0" })
                    .collect();
                parts.push(format!("--share-clip {}", shares.join(",")));
            }
            // Details / show / Transformations-sync — omit when all at default
            // (details off, all visible, all synced).
            if (0..n).any(|i| self.details_of(i)) {
                let details: Vec<&str> = (0..n)
                    .map(|i| if self.details_of(i) { "1" } else { "0" })
                    .collect();
                parts.push(format!("--detail {}", details.join(",")));
            }
            if self.panes.iter().any(|p| !p.visible) {
                let show: Vec<&str> = self
                    .panes
                    .iter()
                    .map(|p| if p.visible { "1" } else { "0" })
                    .collect();
                parts.push(format!("--show {}", show.join(",")));
            }
            if self.panes.iter().any(|p| !p.sync_tone) {
                let ts: Vec<&str> = self
                    .panes
                    .iter()
                    .map(|p| if p.sync_tone { "1" } else { "0" })
                    .collect();
                parts.push(format!("--tsync {}", ts.join(",")));
            }
            // Per-pane effective rotation — omit when every pane is unrotated.
            if (0..n).any(|i| self.rotation_of(i) != 0.0) {
                let rots: Vec<String> = (0..n)
                    .map(|i| {
                        let r = self.rotation_of(i);
                        format!("{}", (r * 100.0).round() / 100.0)
                    })
                    .collect();
                parts.push(format!("--rotate {}", rots.join(",")));
            }
        }
        if let Some((lo, hi)) = self.playback.loop_range {
            parts.push(format!("--loop {lo},{hi}"));
        }
        if n > 0 {
            if self.current != 0 {
                parts.push(format!("--pane {}", self.current.min(n - 1)));
            }
            if self.control != 0 {
                parts.push(format!("--control {}", self.control.min(n - 1)));
            }
            if self.mode == Mode::Ab {
                parts.push(format!(
                    "--ab {},{},{:.3}",
                    self.slot_a, self.slot_b, self.ab_split
                ));
            }
        }
        parts.join(" ")
    }

    pub(super) fn open_dialog(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter("Images & sequences", crate::cli::LOADABLE_EXTS)
            .add_filter("All files", &["*"])
            .pick_files()
        {
            self.open_paths(paths);
        }
    }

    // ---- loading ---------------------------------------------------------
    /// Open plain paths (from the file dialog or a drag-and-drop) — each becomes
    /// its own pane. Sequences only come from the command line (`open_inputs`).
    pub(super) fn open_paths(&mut self, paths: Vec<PathBuf>) {
        self.open_inputs(paths.into_iter().map(cli::Input::Single).collect());
    }

    /// Open a list of CLI inputs: a `Single` becomes one media, a `Sequence`
    /// becomes a single numbered-file sequence media (one pane, not one per file).
    ///
    /// Media are loaded first (cheap — metadata / page 0 only, decoding is lazy),
    /// then gated: if the result would leave **more than `SEQ_WARN_LIMIT`
    /// sequences** open at once, the loaded media are held in `pending_open` and a
    /// resource-warning confirmation is shown instead of adding the panes now (see
    /// the popup in `update`). Otherwise they're added immediately.
    pub(super) fn open_inputs(&mut self, inputs: Vec<cli::Input>) {
        let mut loaded: Vec<(Media, Source)> = Vec::new();
        for input in inputs {
            let (res, source) = match input {
                cli::Input::Single(p) => (media::load(&p), Source::File(p)),
                cli::Input::Sequence { token, files } => (
                    media::load_sequence(&files, token.clone()),
                    Source::Sequence { token, files },
                ),
            };
            match res {
                Ok(m) => loaded.push((m, source)),
                Err(e) => self.error_popup = Some(format!("Failed to open:\n{e}")),
            }
        }

        // Count sequences (multi-frame media) that would be open after this —
        // panes already up, plus any batch already waiting behind the warning, plus
        // the ones now loading. Including `pending_open` keeps a second drop gated
        // instead of slipping panes in while the big batch still waits.
        let open_seqs = self.panes.iter().filter(|p| p.media.is_sequence()).count();
        let waiting_seqs = self
            .pending_open
            .as_ref()
            .map(|b| b.iter().filter(|(m, _)| m.is_sequence()).count())
            .unwrap_or(0);
        let opening = loaded.iter().filter(|(m, _)| m.is_sequence()).count();
        if open_seqs + waiting_seqs + opening > SEQ_WARN_LIMIT {
            // Hold the load behind the warning; `commit_open` finishes it on
            // confirm. Merge with any batch already waiting (rapid drops).
            match &mut self.pending_open {
                Some(pend) => pend.extend(loaded),
                None => self.pending_open = Some(loaded),
            }
            return;
        }
        self.commit_open(loaded);
    }

    /// Add a batch of already-loaded media as panes and re-settle the view
    /// selectors. Shared by the immediate path and the confirmed ">8 sequences"
    /// path (`update`), so both run the same post-open fixups.
    pub(super) fn commit_open(&mut self, loaded: Vec<(Media, Source)>) {
        for (m, source) in loaded {
            self.add_pane(m, source);
        }
        let n = self.panes.len();
        self.current = self.current.min(n.saturating_sub(1));
        self.slot_a = self.slot_a.min(n.saturating_sub(1));
        self.slot_b = self.slot_b.min(n.saturating_sub(1));
        if n >= 2 && self.slot_a == self.slot_b {
            self.slot_b = self.slot_a + 1;
        }
        self.shared_view.needs_fit = true;
        // A view state deferred at startup (behind the warning) applies now that
        // the panes exist.
        if let Some(v) = self.pending_view.take() {
            self.apply_view_state(v);
        }
    }

    /// Push a freshly loaded media as a new pane with default per-pane state.
    pub(super) fn add_pane(&mut self, media: Media, source: Source) {
        let id = self.next_id;
        self.next_id += 1;
        // Always the built-in Linear map; the clip toggle carries the auto-
        // contrast. >8-bit sources need it to be legible, so clip defaults on;
        // 8-bit displays 1:1, so clip defaults off (a plain identity map).
        let contrast = ContrastMode::Linear;
        let mut tone = ToneOptions::default();
        tone.clip.enabled = media.hi_depth();
        // Transformations sync is on by default; the first opened media seeds the
        // shared set (so its depth-appropriate tone becomes the group default).
        if self.panes.is_empty() {
            self.shared_contrast = contrast;
            self.shared_tone = tone;
            self.shared_details = false;
            self.shared_rotation = 0.0;
        }
        self.panes.push(Pane {
            id,
            source,
            media,
            tex: PaneTex::default(),
            transform: ViewTransform::default(),
            frame: 0,
            sync_spatial: true,
            sync_temporal: true,
            sync_tone: true,
            visible: true,
            contrast,
            tone,
            show_opts: false,
            details: false,
            rotation: 0.0,
            overlay: None,
            overlay_tex: None,
            region_tone: false,
            stats: None,
            hist: None,
            compute: None,
            error: None,
            eager: Eager::Off,
            watch: Watch::default(),
        });
    }

    pub(super) fn remove_media(&mut self, i: usize) {
        if i >= self.panes.len() {
            return;
        }
        let removed_id = self.panes[i].id;
        self.decoder.forget(removed_id); // drop its persistent reader
        self.renderer.forget(removed_id); // drop its render thread + operator instances
        self.render_inflight.remove(&removed_id);
        self.panes.remove(i);
        // Drop any overlay (own or shared) that pointed at the removed mask, and
        // clear cached overlay textures that referenced it.
        if self
            .shared_overlay
            .is_some_and(|o| o.src_id == removed_id)
        {
            self.shared_overlay = None;
        }
        for p in &mut self.panes {
            if p.overlay.is_some_and(|o| o.src_id == removed_id) {
                p.overlay = None;
            }
            p.overlay_tex = None;
        }
        let n = self.panes.len();
        let fix = |v: &mut usize| {
            if *v > i {
                *v -= 1;
            }
            *v = (*v).min(n.saturating_sub(1));
        };
        fix(&mut self.current);
        fix(&mut self.control);
        fix(&mut self.slot_a);
        fix(&mut self.slot_b);
    }

    /// Re-open a pane's file from disk, picking up external changes while
    /// keeping its current frame. Files are opened read-only with shared access,
    /// so a persistent reader never blocks another program from writing them.
    pub(super) fn reload(&mut self, i: usize) {
        if i >= self.panes.len() {
            return;
        }
        // A Compute pane has no file to reload; refresh it from current memory.
        if matches!(self.panes[i].source, Source::Computed) {
            self.recompute_pane(i);
            return;
        }
        let loaded = match &self.panes[i].source {
            Source::File(p) => media::load(p),
            Source::Sequence { token, files } => media::load_sequence(files, token.clone()),
            Source::Computed => unreachable!(),
        };
        match loaded {
            Ok(m) => {
                let id = self.panes[i].id;
                self.decoder.forget(id); // reopen the file for its fresh contents
                self.renderer.forget(id); // rebuild the render thread + instances for fresh contents
                self.render_inflight.remove(&id);
                // Drop stale in-flight decodes aimed at the old contents.
                self.inflight.retain(|(pid, _)| *pid != id);
                self.panes[i].media = m;
                self.panes[i].tex.clear();
                self.panes[i].stats = None; // recompute region stats from fresh data
                self.panes[i].hist = None; // recompute histogram from fresh data
                self.panes[i].error = None;
                // If this is a mask, invalidate overlay textures whose effective
                // source is it, so they rebuild from the reloaded contents.
                let shared_src = self.shared_overlay.map(|o| o.src_id);
                for p in &mut self.panes {
                    let eff = if p.sync_tone {
                        shared_src
                    } else {
                        p.overlay.map(|o| o.src_id)
                    };
                    if eff == Some(id) {
                        p.overlay_tex = None;
                    }
                }
                // Frame position is left untouched; frame_disp clamps it if the
                // reloaded file is shorter.
                // Re-baseline any file watch to the freshly-loaded contents so it
                // doesn't immediately fire again on the change we just picked up.
                self.panes[i].watch.loaded = Self::source_file_sig(&self.panes[i].source);
                self.panes[i].watch.seen = None;
            }
            Err(e) => self.panes[i].error = Some(format!("Reload failed: {e}")),
        }
    }

    pub(super) fn reload_all(&mut self) {
        for i in 0..self.panes.len() {
            self.reload(i);
        }
    }
}

/// Render a path for a shell command line, double-quoting it when it contains
/// whitespace so the generated `cim …` command pastes back correctly.
fn quote_path(p: &Path) -> String {
    quote_arg(&p.display().to_string())
}

/// Double-quote a command-line argument when it contains whitespace.
fn quote_arg(s: &str) -> String {
    if s.chars().any(char::is_whitespace) {
        format!("\"{s}\"")
    } else {
        s.to_string()
    }
}
