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
    /// preview honours. A still or a temporally unsynced pane doesn't track the
    /// scrubber at all, though (its frame wouldn't move as the cursor did), so
    /// those fall back to the pane that actually drives the timeline.
    pub(super) fn preview_pane(&self) -> Option<usize> {
        if self.panes.is_empty() {
            return None;
        }
        let c = self.current.min(self.panes.len() - 1);
        if self.panes[c].media.frame_count() > 1 && self.panes[c].sync_temporal {
            return Some(c);
        }
        let l = self.loop_control();
        (l < self.panes.len()).then_some(l)
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
        let f = crate::tone::synced_index(
            t,
            self.panes[idx].media.frame_count(),
            self.panes[idx].sync_temporal,
            self.panes[idx].frame,
        );
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
        let f = crate::tone::synced_index(
            t,
            self.panes[idx].media.frame_count(),
            self.panes[idx].sync_temporal,
            self.panes[idx].frame,
        );
        // The tone the preview *was* rendered with, so the note under it matches
        // the image rather than the pane's current setting.
        let subst = self.panes[idx]
            .media
            .resident(f)
            .is_some_and(|fr| self.preview_substitutes(idx, &fr));
        let key = self.preview_key(idx, f, subst);
        let tex = self.thumb_cache.get(&key).cloned();
        let size = tex
            .as_ref()
            .map(|t| t.size_vec2())
            .unwrap_or(Vec2::new(PREVIEW_PLACEHOLDER, PREVIEW_PLACEHOLDER * 0.6));

        // Sit just above the scrubber, centred on the cursor, and stay on screen.
        let screen = ctx.screen_rect();
        let pad = 6.0;
        let box_w = size.x + pad * 2.0;
        let x = (anchor.x - box_w / 2.0).clamp(screen.left() + 4.0, screen.right() - box_w - 4.0);
        let pos = Pos2::new(x, anchor.y - size.y - 30.0);

        egui::Area::new(egui::Id::new("timeline_preview"))
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
                            match &tex {
                                Some(tex) => {
                                    ui.image((tex.id(), size));
                                }
                                None => {
                                    // No spinner, as everywhere else here: an
                                    // empty plate that fills in when it lands.
                                    let (r, _) = ui.allocate_exact_size(size, Sense::hover());
                                    ui.painter().rect_filled(r, 0.0, PANE_BG);
                                }
                            }
                            ui.monospace(format!("{t}"));
                            if subst {
                                ui.weak(t!("preview.linear"));
                            }
                        });
                    });
            });
    }
}
