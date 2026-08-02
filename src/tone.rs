//! The display-bounds maths shared by the **live view** and the **export**.
//!
//! `agents.md` §10 states the rule this module exists to enforce: what an
//! exported frame shows must be what the panes showed at that timeline position.
//! Both sides therefore have to answer the same three questions — *does this
//! pane clip?*, *over what region?*, *which frame does it show at time `t`?* —
//! and they used to answer them with hand-written mirrors of each other
//! (`export::frame_bounds` "mirroring" `own_tone_bounds`, `src_index` mirroring
//! `frame_disp`, and two byte-identical copies of `pixel_bounds`). A mirror only
//! holds while someone maintains it, and drift here is invisible: the panes and
//! the video simply disagree.
//!
//! So the maths lives here, once. What deliberately stays on each side is the
//! *state*: the view reads live panes, the export reads an owned snapshot, and
//! the export worker must never touch UI-thread-owned `Media` (§10/§14). This
//! module unifies the functions, not the ownership — it borrows nothing but a
//! frame and plain parameters, so both callers can reach it.

use eframe::egui::Rect;

use crate::media::FrameData;
use crate::settings::{ContrastMode, ToneOptions};

/// Clamp an image-space region to a frame's pixel grid, returning the integer
/// half-open bounds `[x0, x1) × [y0, y1)`, or `None` if it doesn't cover at
/// least one pixel (e.g. the region lies entirely outside this frame — pages
/// can differ in size).
pub fn pixel_bounds(reg: Rect, size: [usize; 2]) -> Option<(usize, usize, usize, usize)> {
    let (w, h) = (size[0], size[1]);
    let x0 = (reg.min.x.floor().max(0.0) as usize).min(w);
    let y0 = (reg.min.y.floor().max(0.0) as usize).min(h);
    let x1 = (reg.max.x.ceil().max(0.0) as usize).min(w);
    let y1 = (reg.max.y.ceil().max(0.0) as usize).min(h);
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1, y1))
}

/// The effective per-tail clip percentile for a pane's tone: `Some(pct)` clips
/// that much off each tail, `None` maps the full range.
///
/// LUT_ALPHA always takes the full range — it computes its own contrast — so the
/// clip toggle doesn't apply to it. Linear and Colormap share the same bounds.
pub fn clip_pct(contrast: ContrastMode, tone: &ToneOptions) -> Option<f32> {
    (contrast != ContrastMode::LutAlpha && tone.clip.enabled).then_some(tone.clip.percent)
}

/// Whether a pane renders through the palette rather than the plain LUT.
///
/// Colormap is a *mono* tone: a multi-channel frame falls back to the ordinary
/// render, and a boolean mask paints false/true as black/white, bypassing tone
/// entirely.
pub fn uses_colormap(contrast: ContrastMode, frame: &FrameData) -> bool {
    contrast == ContrastMode::Colormap && frame.color_channels() == 1 && !frame.is_mask()
}

/// Display bounds `[lo, hi]` for `frame` — **the** definition, used by the live
/// render and the export compositor alike.
///
/// A `region` (the export crop or the pinned stats region) restricts the bounds
/// to those pixels: its min/max, or its per-tail percentile when `clip` is set.
/// A region that doesn't cover a pixel of this frame falls back to whole-frame
/// bounds rather than yielding nothing.
pub fn frame_bounds(frame: &FrameData, clip: Option<f32>, region: Option<Rect>) -> (f32, f32) {
    if let Some(reg) = region {
        if let Some((x0, y0, x1, y1)) = pixel_bounds(reg, frame.size) {
            return frame.region_display_bounds(
                x0,
                y0,
                x1,
                y1,
                clip.is_some(),
                clip.unwrap_or(DEFAULT_CLIP_PCT),
            );
        }
    }
    match clip {
        Some(pct) => frame.clip_bounds(pct),
        None => frame.display_bounds(false),
    }
}

/// Percentile handed to `region_display_bounds` when the clip is *off* — unused
/// by it in that case (it takes the region's plain min/max), but the parameter
/// is not optional. Matches the default clip percentile.
const DEFAULT_CLIP_PCT: f32 = 0.01;

/// Which source frame a `count`-frame media shows at timeline position `t`.
///
/// A temporally synced media tracks `t`, **holding on its last frame** when it's
/// shorter than the timeline; an unsynced one stays pinned to its own frame.
/// This is what lets a still pair with every frame of a sequence — the still
/// always shows its only frame — and it must agree between the view
/// (`frame_disp` / `stage_target`) and the export, or a video exports frames the
/// panes never displayed.
pub fn synced_index(t: usize, count: usize, sync_temporal: bool, own_frame: usize) -> usize {
    let c = count.max(1);
    if sync_temporal {
        t.min(c - 1)
    } else {
        own_frame % c
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{FrameData, Samples};
    use eframe::egui::{Pos2, Rect};

    /// A 4×4 u16 ramp 0..15, plus one outlier so a clip has something to cut.
    fn ramp_u16() -> FrameData {
        let mut v: Vec<u16> = (0..16).collect();
        v[15] = 60000;
        FrameData::new([4, 4], 1, Samples::U16(v))
    }

    fn ramp_f32() -> FrameData {
        let v: Vec<f32> = (0..16).map(|i| i as f32 * 0.5).collect();
        FrameData::new([4, 4], 1, Samples::F32(v))
    }

    fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1))
    }

    /// The four combinations of (clip on/off × region some/none) must each equal
    /// the `FrameData` call they stand for. This is the property the export used
    /// to restate by hand.
    #[test]
    fn frame_bounds_matches_the_frame_data_reference() {
        for f in [ramp_u16(), ramp_f32()] {
            assert_eq!(frame_bounds(&f, None, None), f.display_bounds(false));
            assert_eq!(frame_bounds(&f, Some(0.5), None), f.clip_bounds(0.5));

            let reg = rect(0.0, 0.0, 2.0, 2.0);
            assert_eq!(
                frame_bounds(&f, None, Some(reg)),
                f.region_display_bounds(0, 0, 2, 2, false, DEFAULT_CLIP_PCT)
            );
            assert_eq!(
                frame_bounds(&f, Some(0.5), Some(reg)),
                f.region_display_bounds(0, 0, 2, 2, true, 0.5)
            );
        }
    }

    /// A region that misses the frame entirely (pages can differ in size) must
    /// fall back to whole-frame bounds, not produce an empty/degenerate window.
    #[test]
    fn a_region_outside_the_frame_falls_back_to_whole_frame() {
        let f = ramp_u16();
        let away = rect(100.0, 100.0, 120.0, 120.0);
        assert_eq!(frame_bounds(&f, None, Some(away)), f.display_bounds(false));
        assert_eq!(frame_bounds(&f, Some(0.5), Some(away)), f.clip_bounds(0.5));
    }

    #[test]
    fn clip_pct_follows_the_toggle_and_ignores_lut_alpha() {
        let mut tone = ToneOptions::default();
        tone.clip.enabled = true;
        tone.clip.percent = 0.25;
        assert_eq!(clip_pct(ContrastMode::Linear, &tone), Some(0.25));
        assert_eq!(clip_pct(ContrastMode::Colormap, &tone), Some(0.25));
        // LUT_ALPHA computes its own contrast: always the full range.
        assert_eq!(clip_pct(ContrastMode::LutAlpha, &tone), None);
        tone.clip.enabled = false;
        assert_eq!(clip_pct(ContrastMode::Linear, &tone), None);
    }

    #[test]
    fn synced_index_holds_short_media_and_pins_unsynced_ones() {
        // Synced: tracks `t`, then holds on the last frame.
        assert_eq!(synced_index(3, 10, true, 0), 3);
        assert_eq!(synced_index(30, 10, true, 0), 9);
        // A still paired with a sequence shows its only frame at every `t`.
        assert_eq!(synced_index(7, 1, true, 0), 0);
        // Unsynced: pinned to its own frame regardless of `t`.
        assert_eq!(synced_index(7, 10, false, 4), 4);
        // A zero-length media must not divide by zero.
        assert_eq!(synced_index(7, 0, true, 0), 0);
        assert_eq!(synced_index(7, 0, false, 3), 0);
    }
}
