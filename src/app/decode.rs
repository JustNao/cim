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

    /// Take finished tone renders off the pool and upload them. Uploads whatever
    /// landed (even if the pane has since advanced) so it shows progress like a
    /// decoded frame; `prepare` re-requests when the result is stale.
    pub(super) fn pump_render(&mut self, ctx: &egui::Context) {
        for d in self.renderer.drain() {
            self.render_inflight.remove(&d.id);
            if let Some(idx) = self.panes.iter().position(|p| p.id == d.id) {
                self.upload_tex(ctx, idx, d.size, &d.rgba, d.frame, d.sig);
            }
        }
    }

    /// Ensure pane `idx` shows the best texture available for its current frame.
    /// Returns `(texture, loading)`. The plain LUT render (Linear / Linear+Clip,
    /// and masks) is cheap and stays **synchronous**. The heavy proprietary
    /// operators (LUT_ALPHA / details) render on the [`RenderPool`] instead: the
    /// pane keeps showing its last texture with a spinner until the result lands
    /// (uploaded by `pump_render`), so a slow operator never blocks the UI thread.
    pub(super) fn prepare(&mut self, ctx: &egui::Context, idx: usize) -> (Option<TextureId>, bool) {
        // While a seek past the frontier is in flight, freeze the display: keep
        // the last texture and show a spinner instead of rendering every frame
        // the frontier probe rides through on the way to the target.
        if self.pending_seek.is_some() {
            let last = self.panes[idx].tex.as_ref().map(|t| t.handle.id());
            return (last, true);
        }
        let f = self.frame_disp(idx);
        let Some(frame) = self.panes[idx].media.resident(f) else {
            // Not decoded yet: queue it and keep the last frame + spinner.
            self.request(idx, f);
            let last = self.panes[idx].tex.as_ref().map(|t| t.handle.id());
            return (last, true);
        };
        self.panes[idx].media.touch(f, self.clock); // viewing keeps it hot

        // Cheap, parameter-only signature of everything that changes the toned
        // output (bar the frame itself). With `shown` it tells a still-current
        // texture from a stale one without recomputing the (possibly O(N)) bounds.
        let sig = self.tone_sig(idx);
        if let Some(t) = &self.panes[idx].tex {
            if t.shown == f && t.sig == sig {
                return (Some(t.handle.id()), false);
            }
        }

        let contrast = self.contrast_of(idx);
        // The proprietary operators only run on 16-bit frames with the library
        // loaded; otherwise LUT_ALPHA / Details fall back to a plain render, so
        // there's nothing heavy to push off-thread.
        let heavy = !frame.is_mask()
            && frame.is_u16()
            && crate::imageproc::is_available()
            && (contrast == ContrastMode::LutAlpha || self.details_of(idx));

        if heavy {
            // Render off-thread. One render per pane at a time, so rapid tone /
            // frame changes coalesce instead of piling up jobs.
            let id = self.panes[idx].id;
            if !self.render_inflight.contains(&id) {
                let (lo, hi) = self.tone_bounds(idx, &frame);
                let tone = self.tone_of(idx);
                let lut_blend = (contrast == ContrastMode::LutAlpha)
                    .then(|| tone.lut_alpha.blend.clamp(0.0, 1.0));
                self.renderer.request(crate::renderer::RenderJob {
                    id,
                    frame: f,
                    sig,
                    data: frame.clone(),
                    lo,
                    hi,
                    lut_blend,
                    details: self.details_of(idx),
                });
                self.render_inflight.insert(id);
            }
            let last = self.panes[idx].tex.as_ref().map(|t| t.handle.id());
            (last, true)
        } else {
            // Synchronous LUT render (no proprietary operators). Always nearest,
            // at any zoom: the value under the cursor must be a true source
            // sample, never a blend of neighbours.
            let (lo, hi) = self.tone_bounds(idx, &frame);
            frame.render_into(lo, hi, &mut self.render_scratch);
            let img = ColorImage::from_rgba_unmultiplied(frame.size, &self.render_scratch);
            let name = format!("m{}", self.panes[idx].id);
            set_cached_tex(&mut self.panes[idx].tex, ctx, name, img, f, sig);
            (Some(self.panes[idx].tex.as_ref().unwrap().handle.id()), false)
        }
    }

    /// Upload an RGBA buffer as pane `idx`'s texture, tagged with `(f, sig)`.
    fn upload_tex(&mut self, ctx: &egui::Context, idx: usize, size: [usize; 2], rgba: &[u8], f: usize, sig: u64) {
        let img = ColorImage::from_rgba_unmultiplied(size, rgba);
        let name = format!("m{}", self.panes[idx].id);
        set_cached_tex(&mut self.panes[idx].tex, ctx, name, img, f, sig);
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
        self.panes[src].media.touch(f, self.clock); // keep it hot so it isn't evicted

        let rgb = [color.r(), color.g(), color.b()];
        let alpha = (opacity.clamp(0.0, 1.0) * 255.0) as u8;

        let need = match &self.panes[idx].overlay_tex {
            Some(t) => t.shown != f,
            None => true,
        };
        if need {
            let mut buf = Vec::new();
            frame.render_mask_rgba(rgb, alpha, &mut buf);
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
