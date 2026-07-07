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
            // Ride the frontier and probe the next page; `ensure_lookahead`
            // (triggered by this frame position) issues the actual request.
            self.shared_frame = known - 1;
            self.request(i, known);
        }
    }

    /// Keep the next page discovered for panes the user is browsing, so stepping
    /// forward and the timeline length stay ahead of the cursor without ever
    /// decoding past what's actually being viewed. Only panes actually on screen
    /// (see `displayed_indices`) are probed — a loaded-but-hidden sequence would
    /// otherwise keep decoding its frontier and starve the shown pane, making the
    /// UI laggy even when a single media is displayed.
    pub(super) fn ensure_lookahead(&mut self) {
        if self.panes.is_empty() {
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
        let budget = self.cache_budget_bytes();
        let mut total: usize = self.panes.iter().map(|p| p.media.resident_bytes()).sum();
        if total <= budget {
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
            if total <= budget {
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

    /// Take finished tone renders off the pool and stage them. A landed render
    /// goes into the pane's **`pending`** slot (not `tex`), so it isn't shown until
    /// `refresh_textures` commits every on-screen pane together; `stage`
    /// re-requests when the result is stale.
    pub(super) fn pump_render(&mut self, ctx: &egui::Context) {
        for d in self.renderer.drain() {
            self.render_inflight.remove(&d.id);
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
        let mut all_ready = true;
        let mut targets = Vec::with_capacity(panes.len());
        for &idx in &panes {
            let target = self.stage_target(idx);
            targets.push(target);
            if !self.stage(ctx, idx, target) {
                all_ready = false;
            }
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
        for (&idx, &target) in panes.iter().zip(&targets) {
            let sig = self.tone_sig(idx);
            let p = &mut self.panes[idx];
            let tex_shows = p.tex.as_ref().is_some_and(|t| t.shown == target && t.sig == sig);
            let pending_shows = p.pending.as_ref().is_some_and(|t| t.shown == target && t.sig == sig);
            if !tex_shows && pending_shows {
                std::mem::swap(&mut p.tex, &mut p.pending);
            }
        }
        // A committed playback step advances the shared timeline to the frame we
        // just showed — so the counter and the image stay on the same frame.
        if let Some(f) = self.play_prefetch.take() {
            self.shared_frame = f;
        }
    }

    /// The texture to draw for pane `idx`: the committed one, or — only until the
    /// first commit lands — a freshly staged frame, so a pane isn't blank while its
    /// siblings are still rendering. After that `tex` is always present and holds
    /// until the group flips, so on-screen panes stay in step.
    pub(super) fn pane_texture(&self, idx: usize) -> Option<TextureId> {
        self.panes[idx]
            .tex
            .as_ref()
            .or(self.panes[idx].pending.as_ref())
            .map(|t| t.handle.id())
    }

    /// The frame `refresh_textures` should stage for pane `idx`. Synced panes chase
    /// the playback prefetch (the candidate next shared frame) if one is in flight,
    /// else the committed shared frame; unsynced panes use their own frame.
    fn stage_target(&self, idx: usize) -> usize {
        let c = self.panes[idx].media.frame_count().max(1);
        if self.panes[idx].sync_temporal {
            self.play_prefetch.unwrap_or(self.shared_frame).min(c - 1)
        } else {
            self.panes[idx].frame % c
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
    fn stage(&mut self, ctx: &egui::Context, idx: usize, target: usize) -> bool {
        if self.panes[idx].error.is_some() {
            return true; // can't produce a frame; keep the last texture
        }
        // Cheap, parameter-only signature of everything that changes the toned
        // output (bar the frame itself). With `shown` it tells a still-current
        // texture from a stale one without recomputing the (possibly O(N)) bounds.
        let sig = self.tone_sig(idx);
        // Already committed to the target — nothing to stage.
        if let Some(t) = &self.panes[idx].tex {
            if t.shown == target && t.sig == sig {
                return true;
            }
        }
        // Already staged the target (rendered, awaiting the group commit).
        if let Some(t) = &self.panes[idx].pending {
            if t.shown == target && t.sig == sig {
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
        let heavy = !frame.is_mask()
            && frame.is_op_input()
            && ((contrast == ContrastMode::LutAlpha && crate::imageproc::lut_alpha_available())
                || (self.details_of(idx) && crate::imageproc::details_available()));

        if heavy {
            // Render off-thread. One render per pane at a time, so rapid tone /
            // frame changes coalesce instead of piling up jobs.
            let id = self.panes[idx].id;
            if !self.render_inflight.contains(&id) {
                let (lo, hi) = self.tone_bounds(idx, &frame);
                let tone = self.tone_of(idx);
                let lut_blend = (contrast == ContrastMode::LutAlpha)
                    .then(|| tone.lut_alpha.blend.clamp(0.0, 1.0));
                let details = self.details_of(idx);
                self.renderer.request(crate::renderer::RenderJob {
                    id,
                    frame: target,
                    sig,
                    data: frame.clone(),
                    lo,
                    hi,
                    lut_blend,
                    details,
                });
                self.render_inflight.insert(id);
            }
            false // lands in `pending` when the render finishes
        } else {
            // Synchronous LUT render (no proprietary operators). Always nearest,
            // at any zoom: the value under the cursor must be a true source
            // sample, never a blend of neighbours.
            let (lo, hi) = self.tone_bounds(idx, &frame);
            frame.render_into(lo, hi, &mut self.render_scratch);
            let img = ColorImage::from_rgba_unmultiplied(frame.size, &self.render_scratch);
            let name = format!("m{}", self.panes[idx].id);
            set_cached_tex(&mut self.panes[idx].pending, ctx, name, img, target, sig);
            true
        }
    }

    /// Stage an RGBA buffer as pane `idx`'s **pending** texture, tagged `(f, sig)`
    /// (committed to the front by `refresh_textures`).
    fn upload_tex(&mut self, ctx: &egui::Context, idx: usize, size: [usize; 2], rgba: &[u8], f: usize, sig: u64) {
        let img = ColorImage::from_rgba_unmultiplied(size, rgba);
        let name = format!("m{}", self.panes[idx].id);
        set_cached_tex(&mut self.panes[idx].pending, ctx, name, img, f, sig);
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
            ContrastMode::LinearClip => 1,
            ContrastMode::LutAlpha => 2,
        };
        let tone = self.tone_of(idx);
        c.hash(&mut h);
        tone.clip.percent.to_bits().hash(&mut h);
        tone.lut_alpha.blend.to_bits().hash(&mut h);
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
        let pct = self.tone_of(idx).clip.percent;
        let clip = contrast.clips();
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
            // Overlay textures don't tone-map, so their signature stays 0.
            let name = format!("ov{}_{}", idx, src_id);
            set_cached_tex(&mut self.panes[idx].overlay_tex, ctx, name, img, f, 0);
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
) {
    let opts = TextureOptions::NEAREST;
    match slot {
        Some(t) => {
            t.handle.set(img, opts);
            t.shown = shown;
            t.sig = sig;
        }
        None => {
            let handle = ctx.load_texture(name, img, opts);
            *slot = Some(CachedTex { handle, shown, sig });
        }
    }
}
