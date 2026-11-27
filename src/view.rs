//! Shared zoom/pan transform. One transform drives every pane so images move
//! together — the whole point of a comparison tool.

use eframe::egui::{Pos2, Rect, Vec2};

#[derive(Clone, Copy)]
pub struct ViewTransform {
    /// Screen pixels per image pixel.
    pub zoom: f32,
    /// Image-space point (in pixels) shown at the centre of each pane.
    pub center: Vec2,
    /// True until the first image loads / a reset is requested, so we know to fit.
    pub needs_fit: bool,
}

impl Default for ViewTransform {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            center: Vec2::ZERO,
            needs_fit: true,
        }
    }
}

impl ViewTransform {
    /// Fit an image of `img` pixels inside `pane`, centred.
    #[cfg(test)]
    pub fn fit(&mut self, img: [usize; 2], pane: Rect) {
        self.fit_within(img, pane, pane);
    }

    /// Fit an image of `img` pixels inside `inner`, a sub-rect of the view
    /// reference `pane` — the part of it no full-width bar (toolbar / frame bar)
    /// covers — centred in `inner`. The view still maps through `pane`, so the
    /// centre is offset by however far `inner`'s centre sits from `pane`'s.
    pub fn fit_within(&mut self, img: [usize; 2], pane: Rect, inner: Rect) {
        let inner = if inner.is_positive() { inner } else { pane };
        let iw = img[0].max(1) as f32;
        let ih = img[1].max(1) as f32;
        let sx = inner.width() / iw;
        let sy = inner.height() / ih;
        // Fill the limiting dimension exactly (no shrink margin): the floating
        // pane header/footer bars overlay the image's edges, so a gap here would
        // leave a sliver of pane background showing between a bar and the image.
        self.zoom = sx.min(sy).max(1e-4);
        self.center = Vec2::new(iw / 2.0, ih / 2.0) + (pane.center() - inner.center()) / self.zoom;
        self.needs_fit = false;
    }

    pub fn actual_size(&mut self, img: [usize; 2]) {
        self.zoom = 1.0;
        self.center = Vec2::new(img[0] as f32 / 2.0, img[1] as f32 / 2.0);
        self.needs_fit = false;
    }

    /// Where image pixel `p` lands on screen, given a pane.
    pub fn img_to_screen(&self, p: Vec2, pane: Rect) -> Pos2 {
        pane.center() + (p - self.center) * self.zoom
    }

    /// Which image pixel is under screen position `s`.
    pub fn screen_to_img(&self, s: Pos2, pane: Rect) -> Vec2 {
        self.center + (s - pane.center()) / self.zoom
    }

    /// The on-screen rect covering the whole image inside `pane`.
    pub fn image_rect(&self, img: [usize; 2], pane: Rect) -> Rect {
        let top_left = self.img_to_screen(Vec2::ZERO, pane);
        let size = Vec2::new(img[0] as f32, img[1] as f32) * self.zoom;
        Rect::from_min_size(top_left, size)
    }

    /// Zoom by `factor`, keeping the image point under `anchor` fixed.
    pub fn zoom_at(&mut self, factor: f32, anchor: Pos2, pane: Rect) {
        let before = self.screen_to_img(anchor, pane);
        self.zoom = (self.zoom * factor).clamp(1e-4, 512.0);
        let after = self.screen_to_img(anchor, pane);
        // Shift centre so `before` stays under the cursor.
        self.center += before - after;
    }

    /// Pan by a screen-space delta.
    pub fn pan(&mut self, screen_delta: Vec2) {
        self.center -= screen_delta / self.zoom;
    }
}

/// The zoom as a magnification factor, as the toolbar shows it: `x2`, `x1.5`,
/// `x1`, and below 1 a decimal to two significant digits — `x0.8`, `x0.33`,
/// `x0.04` — rather than a percentage.
pub fn fmt_zoom(zoom: f32) -> String {
    fn short(v: f32) -> String {
        let r = (v * 100.0).round() / 100.0;
        if r.fract() == 0.0 {
            format!("{}", r as i64)
        } else {
            format!("{r}")
        }
    }
    if zoom >= 0.995 {
        format!("x{}", short(zoom))
    } else {
        // Two significant digits however small: 0.8 → 1 decimal place's worth
        // of leading zeros + 1, 0.04 → 3 decimals.
        let decimals = (1 - zoom.log10().floor() as i32).max(1) as usize;
        let s = format!("{zoom:.decimals$}");
        let s = s.trim_end_matches('0');
        format!("x{}", s.strip_suffix('.').unwrap_or(s))
    }
}

/// Parse a typed zoom: `x2`, `2`, `x1/4`, `1/4`, `0.25` or `25%`. `None` for
/// anything else or a non-positive value; the result is clamped to the view's
/// zoom range.
pub fn parse_zoom(s: &str) -> Option<f32> {
    let s = s.trim();
    let s = s.strip_prefix(['x', 'X', '×']).unwrap_or(s).trim();
    let z = if let Some(p) = s.strip_suffix('%') {
        p.trim().parse::<f32>().ok()? / 100.0
    } else if let Some((n, d)) = s.split_once('/') {
        n.trim().parse::<f32>().ok()? / d.trim().parse::<f32>().ok()?
    } else {
        s.parse::<f32>().ok()?
    };
    (z.is_finite() && z > 0.0).then(|| z.clamp(1e-4, 512.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fit inside a bar-trimmed sub-rect shows the whole image within it, even
    /// though the view still maps through the full pane.
    #[test]
    fn fit_within_keeps_the_image_clear_of_the_bars() {
        let pane = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
        let inner = Rect::from_min_max(Pos2::new(0.0, 40.0), Pos2::new(800.0, 600.0));
        let mut v = ViewTransform::default();
        v.fit_within([400, 400], pane, inner);
        let r = v.image_rect([400, 400], pane);
        assert!((r.height() - inner.height()).abs() < 1e-3);
        assert!((r.min.y - inner.min.y).abs() < 1e-3, "{r:?}");
        assert!((r.max.y - inner.max.y).abs() < 1e-3, "{r:?}");
        assert!((r.center().x - pane.center().x).abs() < 1e-3);
        assert!(!v.needs_fit);
        // An unobscured fit is the plain one.
        let mut plain = ViewTransform::default();
        plain.fit([400, 400], pane);
        let mut same = ViewTransform::default();
        same.fit_within([400, 400], pane, pane);
        assert_eq!((plain.zoom, plain.center), (same.zoom, same.center));
    }

    #[test]
    fn zoom_reads_as_a_factor_and_parses_back() {
        assert_eq!(fmt_zoom(1.0), "x1");
        assert_eq!(fmt_zoom(2.0), "x2");
        assert_eq!(fmt_zoom(1.5), "x1.5");
        assert_eq!(fmt_zoom(0.25), "x0.25");
        assert_eq!(fmt_zoom(0.4), "x0.4");
        assert_eq!(fmt_zoom(0.8), "x0.8");
        assert_eq!(fmt_zoom(1.0 / 3.0), "x0.33");
        assert_eq!(fmt_zoom(0.04), "x0.04");
        assert_eq!(fmt_zoom(0.00417), "x0.0042");
        for z in [1.0, 2.0, 1.5, 0.25, 0.4, 8.0] {
            assert_eq!(parse_zoom(&fmt_zoom(z)), Some(z));
        }
        assert_eq!(parse_zoom("4"), Some(4.0));
        assert_eq!(parse_zoom(" X 3 "), Some(3.0));
        assert_eq!(parse_zoom("1/8"), Some(0.125));
        assert_eq!(parse_zoom("50%"), Some(0.5));
        assert_eq!(parse_zoom("0"), None);
        assert_eq!(parse_zoom("-2"), None);
        assert_eq!(parse_zoom("1/0"), None);
        assert_eq!(parse_zoom("abc"), None);
        assert_eq!(parse_zoom("1e9"), Some(512.0));
    }
}
