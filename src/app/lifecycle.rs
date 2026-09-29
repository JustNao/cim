//! Media lifecycle: opening (dialog / paths / CLI inputs), adding & removing
//! panes, reloading from disk, and the view-state replay / "View cmd" round
//! trip (`apply_view_state` / `view_command`).

use super::*;

use std::sync::{Arc, OnceLock};

/// An open whose media load on a background thread, one input per rayon job,
/// with each first frame decoded before it lands. Started from `main` before
/// the window exists, so the startup decode overlaps creating it; a drop or a
/// dialog open starts one the same way and never blocks the UI thread.
pub struct Preload {
    thread: std::thread::JoinHandle<Loaded>,
    /// Woken when the load finishes. Filled once the UI exists, which may be
    /// after the thread finished — `CimApp::new` checks for that.
    waker: Arc<OnceLock<egui::Context>>,
}

/// Loaded items in input order, and each failed input's error.
struct Loaded {
    items: Vec<OpenItem>,
    errors: Vec<String>,
}

impl Preload {
    pub fn start(inputs: Vec<cli::Input>) -> Self {
        let waker: Arc<OnceLock<egui::Context>> = Arc::default();
        let wake = Arc::clone(&waker);
        let thread = std::thread::spawn(move || {
            let loaded = load_inputs(inputs);
            if let Some(ctx) = wake.get() {
                ctx.request_repaint();
            }
            loaded
        });
        Self { thread, waker }
    }

    pub(super) fn set_waker(&self, ctx: &egui::Context) {
        let _ = self.waker.set(ctx.clone());
        if self.thread.is_finished() {
            ctx.request_repaint();
        }
    }

    fn join(self) -> Loaded {
        self.thread
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    }
}

fn load_inputs(inputs: Vec<cli::Input>) -> Loaded {
    use rayon::prelude::*;
    let results: Vec<Result<OpenItem, String>> =
        crate::cpu::install(|| inputs.into_par_iter().map(load_input).collect());
    let mut loaded = Loaded {
        items: Vec::new(),
        errors: Vec::new(),
    };
    for r in results {
        match r {
            Ok(item) => loaded.items.push(item),
            Err(e) => loaded.errors.push(e),
        }
    }
    loaded
}

fn load_input(input: cli::Input) -> Result<OpenItem, String> {
    let (res, source) = match input {
        cli::Input::Single(p) => (media::load_ready(&p), Source::File(p)),
        cli::Input::Sequence { token, files } => (
            media::load_sequence_ready(&files, token.clone()),
            Source::Sequence { token, files },
        ),
        // No media to load: its sources are wired by `commit_open` once every
        // pane exists.
        cli::Input::Compute { kind, a, b } => return Ok(OpenItem::Compute { kind, a, b }),
    };
    res.map(|m| OpenItem::Media(m, source))
        .map_err(|e| t!("error.open_failed", err = e).into_owned())
}

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
        // Names the user gave panes (renamed from their header).
        for (i, name) in &vs.names {
            if let Some(p) = self.panes.get_mut(*i) {
                let name = name.trim();
                p.custom_name = (!name.is_empty()).then(|| name.to_owned());
            }
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
                p.visual.contrast = match t {
                    cli::Tone::Linear => ContrastMode::Linear,
                    cli::Tone::LutAlpha => ContrastMode::LutAlpha,
                    cli::Tone::Boost => ContrastMode::Boost,
                    cli::Tone::Colormap(pal) => {
                        p.visual.tone.palette = *pal;
                        ContrastMode::Colormap
                    }
                };
                p.sync_tone = false;
            }
        }
        // Per-pane Linear clip (`--clip`): a toggle + percentile. Like --tone this
        // is per-pane, so unsync the panes it sets.
        if let Some(clips) = &vs.clips {
            for (p, c) in self.panes.iter_mut().zip(clips) {
                match c {
                    cli::ClipSpec::Off => p.visual.tone.clip.enabled = false,
                    cli::ClipSpec::On(pct) => {
                        p.visual.tone.clip.enabled = true;
                        p.visual.tone.clip.percent = *pct;
                    }
                }
                p.sync_tone = false;
            }
        }
        // Per-pane "Share clip" (`--share-clip`): lock the bounds to the Control
        // media's. Per-pane like --tone/--clip, so unsync the panes it sets.
        if let Some(shares) = &vs.share_clip {
            for (p, s) in self.panes.iter_mut().zip(shares) {
                p.visual.tone.share_clip = *s;
                p.sync_tone = false;
            }
        }
        if let Some(details) = &vs.details {
            for (p, d) in self.panes.iter_mut().zip(details) {
                p.visual.details = *d;
                p.sync_tone = false;
            }
        }
        // Per-pane rotation. Rotation rides the *Geometry* sync, so unsync the
        // panes it sets (otherwise a synced pane would ignore its own angle and
        // follow the shared one); the following --gsync re-syncs and re-seeds.
        if let Some(rots) = &vs.rotations {
            for (p, &r) in self.panes.iter_mut().zip(rots) {
                p.rotation = wrap180(r);
                p.sync_geometry = false;
            }
        }
        // Visualization sync flags (`--tsync`), applied *after* the per-pane
        // tone/detail flags (which unsync the panes they set). Re-seed the shared
        // set from the first synced pane so panes that follow it show the look.
        if let Some(sync) = &vs.tsync {
            if let Some(k) = sync.iter().position(|&s| s) {
                if let Some(p) = self.panes.get(k) {
                    // The overlay isn't part of the view command, so it stays.
                    let overlay = self.shared_visual.overlay;
                    self.shared_visual = Visual {
                        overlay,
                        ..p.visual
                    };
                }
            }
            for (p, &s) in self.panes.iter_mut().zip(sync) {
                p.sync_tone = s;
            }
        }
        // Geometry sync flags (`--gsync`), applied *after* --rotate (which unsyncs
        // the panes it sets). Re-seed the shared rotation from the first synced
        // pane so panes that follow it show the captured angle.
        if let Some(sync) = &vs.gsync {
            if let Some(k) = sync.iter().position(|&s| s) {
                if let Some(p) = self.panes.get(k) {
                    self.shared_rotation = p.rotation;
                }
            }
            for (p, &s) in self.panes.iter_mut().zip(sync) {
                p.sync_geometry = s;
            }
        }
        if let Some(vis) = &vs.visible {
            for (p, &v) in self.panes.iter_mut().zip(vis) {
                p.visible = v;
            }
        }
        if let Some(scale) = &vs.scale {
            for (p, &s) in self.panes.iter_mut().zip(scale) {
                p.scale = s;
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
    /// fall back to it.
    pub(super) fn view_command(&self) -> String {
        let mut parts: Vec<String> = vec!["cim".into()];
        for p in &self.panes {
            // A numbered sequence goes back out as its compact token, so a replay
            // reopens it as one pane. Paths are made absolute: the command may
            // be run from a launcher whose working directory is elsewhere.
            // `absolute_path` is lexical, so a token's `%0Xu…,START,END` tail
            // survives it.
            match &p.source {
                Source::File(path) => parts.push(quote_path(&absolute_path(path))),
                Source::Sequence { token, .. } => parts.push(quote_arg(
                    &absolute_path(Path::new(token)).display().to_string(),
                )),
                // A Compute pane: re-emit it as a `@compute` token referencing
                // its sources by pane index, so a replay recreates it in place
                // (keeping the positional per-pane flags below aligned). A pane
                // whose source no longer exists is skipped rather than emitting a
                // dangling index.
                Source::Computed => {
                    if let Some(tok) = self.compute_token(p) {
                        parts.push(tok);
                    }
                }
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
        // Renamed panes, one `--name N=TEXT` each (a name may hold commas, so
        // not a positional list).
        for (i, p) in self.panes.iter().enumerate() {
            if let Some(name) = &p.custom_name {
                parts.push(format!("--name {}", quote_arg(&format!("{i}={name}"))));
            }
        }
        if n > 0 {
            // Per-pane tone mode (effective — shared when tone-synced). The mode
            // is Linear for every pane unless another tone is chosen, so omit
            // `--tone` when every pane is Linear.
            let tones: Vec<String> = (0..n)
                .map(|i| match self.visual(i).contrast {
                    ContrastMode::Linear => "linear".to_string(),
                    ContrastMode::LutAlpha => "lutalpha".to_string(),
                    ContrastMode::Boost => "boost".to_string(),
                    ContrastMode::Colormap => {
                        format!("colormap:{}", self.visual(i).tone.palette.token())
                    }
                })
                .collect();
            // Replaying a per-pane transformation flag unsyncs the panes it
            // sets, so emitting one forces `--tsync` below.
            let mut per_pane_transform = false;
            if (0..n).any(|i| tones[i] != "linear") {
                parts.push(format!("--tone {}", tones.join(",")));
                per_pane_transform = true;
            }
            // Per-pane Linear clip (effective): `off` or the per-tail percentile.
            // Omit when every pane is at its depth-appropriate default (on at
            // 0.01% for >8-bit, off for 8-bit).
            let clips: Vec<String> = (0..n)
                .map(|i| {
                    let clip = self.visual(i).tone.clip;
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
                per_pane_transform = true;
            }
            // Per-pane 0/1 flags, each omitted while at its default.
            let flags = |f: &dyn Fn(usize) -> bool| -> String {
                (0..n)
                    .map(|i| if f(i) { "1" } else { "0" })
                    .collect::<Vec<_>>()
                    .join(",")
            };
            if (0..n).any(|i| self.visual(i).tone.share_clip) {
                parts.push(format!(
                    "--share-clip {}",
                    flags(&|i| self.visual(i).tone.share_clip)
                ));
                per_pane_transform = true;
            }
            if (0..n).any(|i| self.visual(i).details) {
                parts.push(format!("--detail {}", flags(&|i| self.visual(i).details)));
                per_pane_transform = true;
            }
            // Rotation rides the *Geometry* sync, so it forces `--gsync`, not
            // `--tsync`.
            let per_pane_geometry = (0..n).any(|i| self.rotation_of(i) != 0.0);
            if per_pane_geometry {
                let rots: Vec<String> = (0..n)
                    .map(|i| format!("{}", (self.rotation_of(i) * 100.0).round() / 100.0))
                    .collect();
                parts.push(format!("--rotate {}", rots.join(",")));
            }
            if self.panes.iter().any(|p| !p.visible) {
                parts.push(format!("--show {}", flags(&|i| self.panes[i].visible)));
            }
            if self.panes.iter().any(|p| p.scale) {
                parts.push(format!("--scale {}", flags(&|i| self.panes[i].scale)));
            }
            // Replaying a per-pane flag unsyncs the panes it sets, so an
            // all-synced session that emitted one needs the sync flags to re-sync.
            if self.panes.iter().any(|p| !p.sync_geometry) || per_pane_geometry {
                parts.push(format!(
                    "--gsync {}",
                    flags(&|i| self.panes[i].sync_geometry)
                ));
            }
            if self.panes.iter().any(|p| !p.sync_tone) || per_pane_transform {
                parts.push(format!("--tsync {}", flags(&|i| self.panes[i].sync_tone)));
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

    pub(super) fn open_dialog(&mut self, ctx: &egui::Context) {
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter("Images & sequences", crate::cli::LOADABLE_EXTS)
            .add_filter("Videos", crate::cli::VIDEO_EXTS)
            .add_filter("All files", &["*"])
            .pick_files()
        {
            self.open_paths(paths, ctx);
        }
    }

    // ---- loading ---------------------------------------------------------
    /// Open plain paths (from the file dialog or a drag-and-drop) — each file
    /// becomes its own pane, while a **dropped directory** opens as one
    /// concatenated sequence of its image files plus one pane per video
    /// (like `cim folder`).
    pub(super) fn open_paths(&mut self, paths: Vec<PathBuf>, ctx: &egui::Context) {
        self.open_inputs(
            paths.into_iter().flat_map(cli::inputs_for_path).collect(),
            ctx,
        );
    }

    /// Open a list of CLI inputs in the background (see [`Preload`]).
    pub(super) fn open_inputs(&mut self, inputs: Vec<cli::Input>, ctx: &egui::Context) {
        let preload = Preload::start(inputs);
        preload.set_waker(ctx);
        self.opening.push_back(preload);
    }

    /// Add the panes of finished background opens, in the order they started.
    pub(super) fn poll_opening(&mut self) {
        while self.opening.front().is_some_and(|p| p.thread.is_finished()) {
            let loaded = self.opening.pop_front().expect("checked above").join();
            if let Some(e) = loaded.errors.into_iter().last() {
                self.error_popup = Some(e);
            }
            self.gate_open(loaded.items);
        }
    }

    /// Add loaded items as panes — unless the result would leave more than
    /// `SEQ_WARN_LIMIT` sequences open, in which case they wait in
    /// `pending_open` behind a confirmation.
    fn gate_open(&mut self, loaded: Vec<OpenItem>) {
        let open_seqs = self.panes.iter().filter(|p| p.media.is_sequence()).count();
        let waiting_seqs = self
            .pending_open
            .as_ref()
            .map(|b| b.iter().filter(|it| it.is_sequence()).count())
            .unwrap_or(0);
        let opening = loaded.iter().filter(|it| it.is_sequence()).count();
        if open_seqs + waiting_seqs + opening > SEQ_WARN_LIMIT {
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
    pub(super) fn commit_open(&mut self, loaded: Vec<OpenItem>) {
        // Pane indices are 0-based over the whole (fresh) pane list, so record the
        // offset in case we're appending to existing panes (a runtime drop never
        // carries Compute items, but keep the arithmetic honest).
        let base = self.panes.len();
        // Pass 1: create every pane in order, so a Compute pane lands at its
        // original index. Its sources (given as indices) are resolved in pass 2,
        // once the panes they point at exist (they may appear *after* it).
        let mut computes: Vec<(usize, usize, Option<usize>)> = Vec::new();
        for item in loaded {
            match item {
                OpenItem::Media(m, source) => self.add_pane(m, source),
                OpenItem::Compute { kind, a, b } => {
                    let i = self.add_configured_compute_pane(kind);
                    computes.push((i, a, b));
                }
            }
        }
        // Pass 2: wire each Compute pane's sources (pane index → stable id) and
        // compute it best-effort (a source frame not yet resident just leaves a
        // status; the auto-refresh recomputes once frames land). Sources may be
        // other Compute panes, so wire them in list order and drop any that
        // would close a cycle — a hand-edited view command can name one, and a
        // cycle would recompute forever.
        for &(i, a, b) in &computes {
            let a_id = self.compute_source_id(i, base + a);
            let b_id = b.and_then(|b| self.compute_source_id(i, base + b));
            if let Some(c) = self.panes[i].compute.as_mut() {
                c.source_id = a_id;
                c.source_b = b_id;
            }
        }
        // Compute only once every source is wired, so a pane reading another
        // Compute pane doesn't run against its placeholder. Order within the
        // batch still isn't dependency order — `refresh_auto_compute` settles
        // the chain on the next update, since a replayed pane is `armed`.
        for &(i, ..) in &computes {
            self.recompute_pane(i);
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
        let mut visual = Visual::default();
        visual.tone.clip.enabled = media.hi_depth();
        // Transformations sync is on by default; the first opened media seeds the
        // shared set (so its depth-appropriate tone becomes the group default).
        if self.panes.is_empty() {
            self.shared_visual = visual;
            self.shared_rotation = 0.0;
        }
        self.panes.push(Pane {
            id,
            source,
            media,
            custom_name: None,
            tex: PaneTex::default(),
            transform: ViewTransform::default(),
            frame: 0,
            sync_spatial: true,
            sync_temporal: true,
            sync_tone: true,
            sync_geometry: true,
            visible: true,
            visual,
            scale: false,
            rotation: 0.0,
            overlay_tex: None,
            region_tone: false,
            stats: None,
            hist: None,
            compute: None,
            render_gen: 0,
            error: None,
            tex_error: None,
            offset_scan: None,
            pan_vel: Default::default(),
            cell: Rect::ZERO,
            region_want: None,
            region_show: None,
            eager: Eager::Off,
            watch: Watch::default(),
            fast_jump: None,
            page_anchor: None,
        });
        // Complete a fast-scannable sequence's length in the background as soon as
        // it opens, so the scrubber shows the true length and any index is
        // instantly seekable without the user pressing "Load offsets".
        let i = self.panes.len() - 1;
        self.request_offset_scan(i);
    }

    pub(super) fn remove_media(&mut self, i: usize) {
        if i >= self.panes.len() {
            return;
        }
        let removed_id = self.panes[i].id;
        self.forget_pane_work(removed_id);
        self.panes.remove(i);
        // Drop any overlay (own or shared) that pointed at the removed mask, and
        // clear cached overlay textures that referenced it.
        for v in std::iter::once(&mut self.shared_visual)
            .chain(self.panes.iter_mut().map(|p| &mut p.visual))
        {
            if v.overlay.is_some_and(|o| o.src_id == removed_id) {
                v.overlay = None;
            }
        }
        for p in &mut self.panes {
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

    /// Re-decode every open JPEG 2000 pane at the level `config.jp2_max_mp` now
    /// implies, from its **kept codestream** — no file is re-read (see
    /// `media::jp2`). The image's pixel size changes with the level, so the
    /// pane is re-fitted and its texture dropped, exactly as a reload would.
    pub(super) fn relevel_jp2_panes(&mut self) {
        crate::media::jp2::set_budget_px(self.config.jp2_max_mp.saturating_mul(1_000_000));
        let budget = crate::media::jp2::budget_px();
        for i in 0..self.panes.len() {
            match self.panes[i].media.jp2_relevel(budget) {
                Ok(false) => {}
                Ok(true) => {
                    // The frame *data* changed, not just its tone, and it
                    // changed size — so unlike a Compute recompute this drops
                    // the texture and re-fits rather than keeping the old view.
                    self.panes[i].tex.clear();
                    self.panes[i].error = None;
                    self.view_mut(i).needs_fit = true;
                }
                Err(e) => self.panes[i].error = Some(e.to_string()),
            }
        }
    }

    /// Re-open a pane's file from disk, picking up external changes while
    /// keeping its current frame (via a fastscan offset jump, else riding the
    /// frontier). Files are opened read-only with shared access, so a persistent
    /// reader never blocks another program from writing them.
    pub(super) fn reload(&mut self, i: usize) {
        if i >= self.panes.len() {
            return;
        }
        // A Compute pane has no file to reload; refresh it from current memory.
        if matches!(self.panes[i].source, Source::Computed) {
            self.recompute_pane(i);
            return;
        }
        // The frame the user is viewing, captured before the media is swapped so
        // we can land back on it below (the fresh media starts length 1).
        let mut target = self.frame_disp(i);
        // For a media built from several files, capture *which file* (and which
        // page within it) that frame is — the durable identity of what the user
        // is watching. The global index isn't: a re-listed folder or a file that
        // changed length shifts every index after it.
        let anchor_file = self.panes[i]
            .media
            .local_file(target)
            .map(|(p, page)| (p.to_path_buf(), page));
        // A pane opened from a **folder** re-lists it, so files added or removed
        // since it was opened join or leave the timeline — the point of reloading
        // a folder. A numbered token states its own range, so it reopens as given.
        if let Source::Sequence { token, files } = &mut self.panes[i].source {
            if let Some(fresh) = cli::folder_files(token) {
                *files = fresh;
            }
        }
        let loaded = match &self.panes[i].source {
            Source::File(p) => media::load(p),
            Source::Sequence { token, files } => media::load_sequence(files, token.clone()),
            Source::Computed => unreachable!(),
        };
        match loaded {
            Ok(m) => {
                let id = self.panes[i].id;
                self.forget_pane_work(id);
                self.panes[i].media = m;
                // New data behind the same pane: a Compute pane reading it
                // sees the generation move and recomputes.
                self.panes[i].render_gen = self.panes[i].render_gen.wrapping_add(1);
                self.panes[i].tex.clear();
                self.panes[i].stats = None; // recompute region stats from fresh data
                self.panes[i].hist = None; // recompute histogram from fresh data
                self.panes[i].error = None;
                self.panes[i].fast_jump = None; // re-measure the (possibly new) layout
                                                // Overlays tinted from the old contents.
                self.drop_overlays_from(id);
                // Land back on what the user was viewing. For a media spanning
                // several files (a folder, a concatenated run) that is a *file*
                // and a page within it, not a global index — the re-listing above
                // may have shifted every index, so the old one no longer names the
                // same frame. Ask for the file: `locate_file` answers from what
                // the fresh media already knows (a still run knows its whole file
                // list up front, so it's free), and beyond that `fast_jump_to_file`
                // page-counts the files before it by binary search and decodes the
                // page from its own file — headers, not a decode sweep. The index
                // it lands on becomes the new `target`, which the seek below uses.
                let clock = self.clock;
                let mut landed = None;
                if let Some((path, page)) = &anchor_file {
                    landed = self.panes[i].media.locate_file(path, *page);
                    if landed.is_none() {
                        // Carry the anchor this pane left last time: for a run
                        // rewritten in place it pins the page in two header reads
                        // instead of a walk. It is only reused for the same page
                        // of the same file (`PageAnchor::pins`).
                        let last = self.panes[i].page_anchor.take();
                        match media::fast_jump_to_file(
                            &mut self.panes[i].media,
                            path,
                            *page,
                            last.as_ref(),
                        ) {
                            Ok((f, anchor)) => {
                                landed = Some(f);
                                self.panes[i].page_anchor = anchor;
                            }
                            Err(_) => self.panes[i].page_anchor = None,
                        }
                    }
                }
                // Single-file media (one multi-page TIFF), or a file-anchored jump
                // that couldn't be made: land on the same global index instead.
                // The fresh media only knows its first page, so try a stride-
                // predicted fast jump, then a byte-offset jump: the page's old
                // offset re-checked against the fresh file (two header reads for
                // a file overwritten in place), or a one-off chain walk to build
                // that anchor. Failing both, `seek_to` below rides the frontier.
                if let Some(f) = landed {
                    target = f;
                    self.panes[i].media.touch(target, clock);
                } else if target > 0 {
                    if media::fast_jump(&mut self.panes[i].media, target).is_ok() {
                        self.panes[i].media.touch(target, clock);
                    } else {
                        let last = self.panes[i].page_anchor.take();
                        match media::offset_jump(&mut self.panes[i].media, target, last.as_ref()) {
                            Ok(anchor) => {
                                self.panes[i].page_anchor = Some(anchor);
                                self.panes[i].media.touch(target, clock);
                            }
                            Err(_) => self.panes[i].page_anchor = None,
                        }
                    }
                }
                if self.panes[i].sync_temporal {
                    // This pane follows the shared timeline. When it drives the
                    // loop, re-seek it to `target`: instant if the fast jump (or an
                    // already-known length) covers it, else `seek_to` arms
                    // `pending_seek` so `drive_seek` walks offsets back to it. A
                    // synced pane that doesn't drive the loop follows `shared_frame`
                    // anyway; `catching_up` grows its length to it if needed.
                    if self.loop_control() == i {
                        self.seek_to(target);
                    }
                } else {
                    // Unsynced: this pane shows its own frame index directly.
                    self.panes[i].frame = target;
                }
                // Re-baseline any file watch to the freshly-loaded contents so it
                // doesn't immediately fire again on the change we just picked up.
                // The new baseline is whatever the *next* background signature
                // reports; this also supersedes one still in flight against the
                // contents we just replaced.
                self.rebaseline_watch(i);
                // Re-complete the (fresh, length-1) media's offsets in the
                // background under a new generation — this also supersedes any
                // scan still in flight against the old contents.
                self.request_offset_scan(i);
            }
            Err(e) => self.panes[i].error = Some(t!("error.reload_failed", err = e).into_owned()),
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
