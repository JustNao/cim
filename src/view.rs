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
    pub fn fit(&mut self, img: [usize; 2], pane: Rect) {
        let iw = img[0].max(1) as f32;
        let ih = img[1].max(1) as f32;
        let sx = pane.width() / iw;
        let sy = pane.height() / ih;
        // Fill the limiting dimension exactly (no shrink margin): the floating
        // header/footer bars overlay the image's edges, so a gap here would leave
        // a sliver of pane background showing between a bar and the image.
        self.zoom = sx.min(sy).max(1e-4);
        self.center = Vec2::new(iw / 2.0, ih / 2.0);
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
