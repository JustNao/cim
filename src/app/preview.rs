//! Timeline hover preview: the thumbnail shown above the scrubber for the frame
//! under the cursor.
//!
//! The work is split in two so neither half can hitch the paint. `draw_scrubber`
//! only *records* what the cursor is over ([`Preview::hover`]); [`drive_preview`]
//! — run from `tick`, on the previous frame's record — decides what to fetch and
//! render, and [`draw_preview`] paints whatever the cache holds. A preview being
//! one frame behind the cursor is invisible next to the dwell below.
//!
//! **The cold-frame policy.** A frame already in memory previews immediately: it
//! costs one decimated render on the thumbnail worker. A frame that is *not*
//! resident costs a real decode — on a shared mount ~150 ms of I/O for a single
//! page (see the network-mount notes in the performance docs) — and the cursor
//! crosses hundreds of frames on its way anywhere, so fetching per hovered frame
//! would aim a decode storm at the mount for images nobody asked to see. Four
//! rules keep that bounded:
//!
//! 1. **Dwell.** A non-resident frame is only requested once the cursor has
//!    rested on it for [`PREVIEW_DWELL`]. Sweeping the timeline costs nothing.
//! 2. **One decode in flight.** Superseded rather than cancelled — the decode
//!    pool has no cancellation — so a stale request simply stops being waited
//!    on. It still lands in the frame cache, so nothing is wasted if the user
//!    does seek there.
//! 3. **The ordinary decode path.** `request` / `inflight` / the `SeqCache`, so a
//!    previewed frame is resident afterwards (hover-then-click is instant) and
//!    participates in the LRU like any other. No second decode path to keep in
//!    step with the first.
//! 4. **Resident-only while playing, and always for video.** Playback owns the
//!    decode pool (`prefetch_playback`), and a preview competing with it would
//!    slow the thing that gates the lock-step commit. A `VideoReader` is worse
//!    still: a non-sequential index kills and respawns its ffmpeg child, so a
//!    hover would throw away the streaming position that playback depends on.

use super::*;
use crate::settings::ClipOptions;
use std::sync::Arc;

impl CimApp {
    /// The pane a timeline preview describes: the **focused** pane, since that is
    /// the media the user is working with and whose visualization options the
    /// preview honours. A **still** doesn't track the scrubber at all, though (its
    /// frame wouldn't move as the cursor did), so it falls back to the pane that
    /// actually drives the timeline. A temporally *unsynced* sequence does track
    /// it — the transport drives its own playhead (§8) — so it previews itself.
    pub(super) fn preview_pane(&self) -> Option<usize> {
        if self.panes.is_empty() {
            return None;
        }
        let c = self.current.min(self.panes.len() - 1);
        if self.panes[c].media.frame_count() > 1 {
            return Some(c);
        }
        let l = self.transport();
        (l < self.panes.len()).then_some(l)
    }

    /// The pixel shape the preview reserves for pane `idx`: its **current**
    /// frame's thumbnail size (`thumbs::thumb_size` of the shown frame).
    ///
    /// The box is laid out from this whether or not the thumbnail has landed, so
    /// the plate drawn while one renders is already the shape the image will be —
    /// a fixed placeholder made the box (and its position, which is derived from
    /// its height) jump on every landing, which reads as flicker while the cursor
    /// sweeps the track. Pages of a mixed-size sequence still differ from the
    /// shown frame; a landed thumbnail of another shape is *fitted* into the plate
    /// rather than resizing it (`draw_preview`).
    fn preview_plate(&self, idx: usize) -> Vec2 {
        let [w, h] = crate::thumbs::thumb_size(self.disp_size(idx));
        Vec2::new(w as f32, h as f32)
    }

    /// Whether pane `idx` would run the proprietary operators on `frame`, and so
    /// needs the substituted Linear tone. Split out because it is the *cheap*
    /// half of the tone decision (no percentile scan) and the cache key needs it
    /// before anything else is computed.
    fn preview_substitutes(&self, idx: usize, frame: &media::FrameData) -> bool {
        crate::imageproc::ops_active(frame, self.ops_of(idx))
    }

    /// The cache identity of pane `idx`'s preview of `frame` at timeline `f`.
    ///
    /// The pane's live `tone_sig`, salted when the operator tone was substituted:
    /// the two renders differ, so filing them under one key would show a Linear
    /// preview as though it were the operator one (and vice versa when a library
    /// finishes loading).
    fn preview_key(&self, idx: usize, f: usize, subst: bool) -> crate::thumbs::ThumbKey {
        /// Arbitrary odd constant; only its distinctness matters.
        const SUBSTITUTED: u64 = 0x9E37_79B9_7F4A_7C15;
        let sig = self.tone_sig(idx) ^ if subst { SUBSTITUTED } else { 0 };
        (self.panes[idx].id, f, sig)
    }

    /// Queue the thumbnail render for pane `idx`'s frame `f`, unless it is
    /// already cached or already being rendered.
    fn queue_thumb(&mut self, idx: usize, f: usize, frame: &Arc<media::FrameData>) {
        let subst = self.preview_substitutes(idx, frame);
        let key = self.preview_key(idx, f, subst);
        if self.thumb_cache.contains(&key) || self.preview.inflight.contains(&key) {
            return;
        }
        // The window. A preview whose pane runs the operators is rendered as
        // plain Linear at the default clip instead: the operators are heavy,
        // dimension-keyed C++ instances owned by the pane's render thread, and
        // a thumbnail is not worth building a second set for. It is a different
        // tone from the pane's, which is why it gets its own cache key above and
        // says so under the image (`preview.linear`).
        let window = if subst {
            crate::thumbs::Window::Compute {
                clip: Some(ClipOptions::default().percent),
                region: None,
            }
        } else if self.tone_of(idx).share_clip {
            // "Share clip" pins this pane to the Control media's bounds, which
            // are computed from the Control's *shown* frame — already memoized,
            // so there is nothing to push to the worker.
            match self.control_clip_bounds() {
                Some((lo, hi)) => crate::thumbs::Window::Fixed(lo, hi),
                None => crate::thumbs::Window::Compute {
                    clip: crate::tone::clip_pct(self.contrast_of(idx), &self.tone_of(idx)),
                    region: self.tone_region(idx),
                },
            }
        } else {
            crate::thumbs::Window::Compute {
                clip: crate::tone::clip_pct(self.contrast_of(idx), &self.tone_of(idx)),
                region: self.tone_region(idx),
            }
        };
        let palette = (!subst && crate::tone::uses_colormap(self.contrast_of(idx), frame))
            .then(|| self.tone_of(idx).palette);
        self.thumbs.request(crate::thumbs::ThumbJob {
            key,
            data: frame.clone(),
            window,
            palette,
            step: crate::thumbs::step_for(frame.size),
        });
        self.preview.inflight.insert(key);
    }

    /// Drain landed thumbnails, then act on the frame the cursor was over: render
    /// it if it is resident, else start the dwell that may fetch it.
    pub(super) fn drive_preview(&mut self, ctx: &egui::Context) {
        for done in self.thumbs.drain() {
            self.preview.inflight.remove(&done.key);
            self.thumb_cache.insert(ctx, done.key, done.image);
        }
        let Some((t, _)) = self.preview.hover else {
            // Off the track: stop waiting on any cold fetch. It may still land,
            // and is welcome to — it goes into the frame cache like any decode.
            self.preview.decoding = None;
            return;
        };
        if !self.config.timeline_preview {
            return;
        }
        let Some(idx) = self.preview_pane() else {
            return;
        };
        let f = self.frame_at_timeline(idx, t);
        // Restart the dwell whenever the cursor moves to another frame.
        let now = ctx.input(|i| i.time);
        if self.preview.at != Some((idx, f)) {
            self.preview.at = Some((idx, f));
            self.preview.since = now;
        }
        if let Some(frame) = self.panes[idx].media.resident(f) {
            self.preview.decoding = None;
            self.queue_thumb(idx, f, &frame);
            return;
        }
        // --- cold frame ---------------------------------------------------
        // Rule 4: never compete with playback, and never make a video's
        // streaming reader seek away from where playback left it.
        if self.playback.playing || self.panes[idx].media.is_video() {
            return;
        }
        // Rule 2: at most one preview decode outstanding. A previous one that
        // has already landed (or was dropped by a pool rebuild) frees the slot.
        let id = self.panes[idx].id;
        if let Some(prev) = self.preview.decoding {
            if prev != (id, f) && self.inflight.contains(&prev) {
                return;
            }
        }
        // Rule 1: only once the cursor has actually stopped here. Request the
        // wake-up ourselves — a resting cursor generates no repaints, so nothing
        // else would come back to notice the dwell expiring.
        let waited = now - self.preview.since;
        if waited < PREVIEW_DWELL {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(PREVIEW_DWELL - waited));
            return;
        }
        self.preview.decoding = Some((id, f));
        self.request(idx, f);
    }

    /// Paint the preview box above the scrubber: the thumbnail (or a placeholder
    /// while it is being fetched) and the frame index.
    pub(super) fn draw_preview(&mut self, ctx: &egui::Context) {
        if !self.config.timeline_preview {
            return;
        }
        let Some((t, anchor)) = self.preview.hover else {
            return;
        };
        let Some(idx) = self.preview_pane() else {
            return;
        };
        let f = self.frame_at_timeline(idx, t);
        // The tone the preview *was* rendered with, so the note under it matches
        // the image rather than the pane's current setting.
        let subst = self.panes[idx]
            .media
            .resident(f)
            .is_some_and(|fr| self.preview_substitutes(idx, &fr));
        let key = self.preview_key(idx, f, subst);
        let tex = self.thumb_cache.get(&key).cloned();
        // The plate: the shape this pane's thumbnails come out at, reserved
        // whether or not this one has landed, so nothing resizes when it does.
        let size = self.preview_plate(idx);
        // A landed thumbnail of a different shape (a mixed-size sequence) is
        // fitted inside the plate instead of resizing it.
        let shown = tex.as_ref().map(|t| fit_in(t.size_vec2(), size));

        // Sit just above the scrubber, centred on the cursor, and stay on screen.
        let screen = ctx.screen_rect();
        let pad = 6.0;
        let box_w = size.x + pad * 2.0;
        let x = (anchor.x - box_w / 2.0).clamp(screen.left() + 4.0, screen.right() - box_w - 4.0);
        // Place it by the height it actually measured last time, so the box sits
        // **fully above** the track: overlapping the scrubber's top edge takes the
        // pointer's hover off it, which drops the preview, which un-overlaps it —
        // the box blinking in and out as the cursor moves along the bar. The first
        // paint (and any size change) falls back to an estimate from the plate,
        // which the measured height corrects on the next frame.
        let est = size.y + pad * 2.0 + 22.0;
        let h = self.preview.box_h.max(est);
        let pos = Pos2::new(x, anchor.y - h - PREVIEW_GAP);

        let area = egui::Area::new(egui::Id::new("timeline_preview"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(BAR_FILL)
                    .stroke(Stroke::new(1.0_f32, CHROME_BORDER))
                    .inner_margin(pad)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.vertical_centered(|ui| {
                            // The plate is always allocated at its full shape, so
                            // the box keeps one size; the image (when it has
                            // landed) is painted centred inside it.
                            let (r, _) = ui.allocate_exact_size(size, Sense::hover());
                            if let (Some(tex), Some(fitted)) = (&tex, shown) {
                                let at = egui::Rect::from_center_size(r.center(), fitted);
                                ui.painter().image(
                                    tex.id(),
                                    at,
                                    egui::Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                    Color32::WHITE,
                                );
                            } else {
                                // No spinner, as everywhere else here: an empty
                                // plate that fills in when it lands.
                                ui.painter().rect_filled(r, 0.0, PANE_BG);
                            }
                            ui.monospace(format!("{t}"));
                            if subst {
                                ui.weak(t!("preview.linear"));
                            }
                        });
                    });
            });
        // Remember what it measured, for the next frame's placement (above).
        self.preview.box_h = area.response.rect.height();
    }
}

/// `size` scaled to fit inside `plate` (never enlarged past it), keeping aspect.
fn fit_in(size: Vec2, plate: Vec2) -> Vec2 {
    if size.x <= 0.0 || size.y <= 0.0 {
        return plate;
    }
    let k = (plate.x / size.x).min(plate.y / size.y).min(1.0);
    size * k
}
