//! Pane-space geometry shared by drawing, overlays and A/B: the rotation-aware
//! image<->screen mappings (the view is a pure similarity, so rotating about
//! the image centre commutes with it), viewer-aligned region selection, and
//! the small angle/painting helpers.

use crate::app::*;

impl CimApp {
    /// Convert a screen-space rect (drawn in Single view over `area`) into the
    /// export crop, aligned to the viewer (see [`select_region_bounds`]).
    pub(super) fn screen_rect_to_image(&self, r: Rect, area: Rect) -> Option<Rect> {
        let idx = self.current.min(self.panes.len().checked_sub(1)?);
        self.select_region_bounds(idx, r, area)
    }

    /// Convert a screen-space selection rect into the image-space region it
    /// covers for pane `idx`, using the pane's view **without its rotation** —
    /// so the region is aligned to the viewer's axis (exactly the rectangle the
    /// user dragged), not the image's. The rotation is re-applied downstream: the
    /// export samples each pixel through `unrotate`, and the overlays draw the
    /// region back with the same plain view. Because the view is a pure
    /// similarity (no rotation), a screen-axis-aligned rect maps to an
    /// axis-aligned image rect, so two opposite corners suffice.
    ///
    /// Clamped to the image bounds only on an **unrotated** pane, so a rotated
    /// crop can include the background outside the image (the export renders it
    /// as transparent); an unrotated crop drops the background exactly as before.
    /// Shared by the export crop and the right-drag stats region so both convert
    /// a release identically.
    pub(super) fn select_region_bounds(&self, idx: usize, r: Rect, area: Rect) -> Option<Rect> {
        let v = self.view_ref(idx);
        let a = v.screen_to_img(r.min, area);
        let b = v.screen_to_img(r.max, area);
        let mut reg = Rect::from_two_pos(a.to_pos2(), b.to_pos2());
        if self.pane_theta(idx) == 0.0 {
            let [w, h] = self.disp_size(idx);
            reg = reg.intersect(Rect::from_min_max(
                Pos2::ZERO,
                Pos2::new(w as f32, h as f32),
            ));
        }
        (reg.width() >= 1.0 && reg.height() >= 1.0).then_some(reg)
    }

    /// The image-space position under screen point `pos` for pane `idx`, but only
    /// when it lands on a real pixel of that pane (so the shared cursor tracks an
    /// actual source sample). `coord_area` maps screen↔image; `clip` bounds where
    /// the pointer counts as being over this pane.
    pub(super) fn hover_img_pos(
        &self,
        idx: usize,
        coord_area: Rect,
        clip: Rect,
        pos: Pos2,
    ) -> Option<Vec2> {
        if !clip.contains(pos) {
            return None;
        }
        let p = self.rot_screen_to_img(idx, pos, coord_area);
        let [w, h] = self.disp_size(idx);
        (p.x >= 0.0 && p.y >= 0.0 && (p.x as usize) < w && (p.y as usize) < h).then_some(p)
    }

    /// Pane `idx`'s effective display rotation in radians (0 when unrotated).
    pub(in crate::app) fn pane_theta(&self, idx: usize) -> f32 {
        self.rotation_of(idx).to_radians()
    }

    /// Screen position of image point `p` for pane `idx`, including the pane's
    /// rotation about its image centre. Inverse of [`rot_screen_to_img`]. Because
    /// the view is a similarity (uniform scale + translate, no rotation), rotating
    /// in image space about the image centre is the same as rotating the mapped
    /// screen point about the image-centre's screen position — so the drawn mesh
    /// (which rotates the image rect's corners) and every overlay stay aligned.
    pub(super) fn rot_img_to_screen(&self, idx: usize, p: Vec2, area: Rect) -> Pos2 {
        rotate_img_to_screen(
            self.view_ref(idx),
            self.disp_size(idx),
            area,
            p,
            self.pane_theta(idx),
        )
    }

    /// Which image pixel is under screen point `s` for pane `idx`, undoing the
    /// pane's rotation. Inverse of [`rot_img_to_screen`].
    pub(super) fn rot_screen_to_img(&self, idx: usize, s: Pos2, area: Rect) -> Vec2 {
        unrotate_screen_to_img(
            self.view_ref(idx),
            self.disp_size(idx),
            area,
            s,
            self.pane_theta(idx),
        )
    }
}

/// [`CimApp::rot_img_to_screen`] without the app — see
/// [`unrotate_screen_to_img`], of which this is the inverse.
pub(in crate::app) fn rotate_img_to_screen(
    v: &crate::view::ViewTransform,
    disp: [usize; 2],
    area: Rect,
    p: Vec2,
    theta: f32,
) -> Pos2 {
    let s = v.img_to_screen(p, area);
    if theta == 0.0 {
        return s;
    }
    rotate_around(s, v.img_to_screen(center_vec(disp), area), theta)
}

/// [`CimApp::rot_screen_to_img`] without the app — the pure mapping, so the
/// geometry that depends on it (`roi::view_center`) can be unit-tested
/// headlessly and cannot drift from what the painter does.
pub(in crate::app) fn unrotate_screen_to_img(
    v: &crate::view::ViewTransform,
    disp: [usize; 2],
    area: Rect,
    s: Pos2,
    theta: f32,
) -> Vec2 {
    if theta == 0.0 {
        return v.screen_to_img(s, area);
    }
    let pivot = v.img_to_screen(center_vec(disp), area);
    v.screen_to_img(rotate_around(s, pivot, -theta), area)
}

/// Rotate screen point `p` about `pivot` by `theta` radians (screen y is down,
/// so a positive angle turns clockwise on screen).
pub(super) fn rotate_around(p: Pos2, pivot: Pos2, theta: f32) -> Pos2 {
    if theta == 0.0 {
        return p;
    }
    let (s, c) = theta.sin_cos();
    let d = p - pivot;
    pivot + Vec2::new(d.x * c - d.y * s, d.x * s + d.y * c)
}
/// Image-space centre (in pixels) of a frame of the given size.
pub(super) fn center_vec(size: [usize; 2]) -> Vec2 {
    Vec2::new(size[0] as f32 / 2.0, size[1] as f32 / 2.0)
}

/// Paint texture `id` into `rect`, rotated by `theta` radians about the rect's
/// centre. `theta == 0` takes the plain axis-aligned path; otherwise a two-triangle
/// textured mesh with the four corners rotated (clipped by the painter's clip rect,
/// so the image still can't spill past its pane).
pub(super) fn paint_rotated(painter: &egui::Painter, id: TextureId, rect: Rect, theta: f32) {
    paint_rotated_about(painter, id, rect, theta, rect.center());
}

/// [`paint_rotated`] with an explicit rotation `pivot`: what a **sub-rect** of
/// the image (the adaptive viewport region) needs — rotating it about its own
/// centre would tear it away from the base underneath, so it rotates about the
/// whole image's screen pivot (the base rect's centre) and stays contiguous.
pub(super) fn paint_rotated_about(
    painter: &egui::Painter,
    id: TextureId,
    rect: Rect,
    theta: f32,
    pivot: Pos2,
) {
    if theta == 0.0 {
        painter.image(id, rect, uv(), Color32::WHITE);
        return;
    }
    let corners = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
    ];
    let uvs = [
        Pos2::new(0.0, 0.0),
        Pos2::new(1.0, 0.0),
        Pos2::new(1.0, 1.0),
        Pos2::new(0.0, 1.0),
    ];
    let mut mesh = egui::Mesh::with_texture(id);
    for (corner, uv) in corners.into_iter().zip(uvs) {
        mesh.vertices.push(egui::epaint::Vertex {
            pos: rotate_around(corner, pivot, theta),
            uv,
            color: Color32::WHITE,
        });
    }
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    painter.add(egui::Shape::mesh(mesh));
}

/// Shortest distance from point `p` to the segment `a`–`b` (screen space), used
/// to hit-test the profile line's body.
pub(super) fn dist_to_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_sq();
    if len2 <= f32::EPSILON {
        return (p - a).length();
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    (p - (a + ab * t)).length()
}

/// Dim everything in `area` outside `r`, then outline `r` (export-region look).
pub(super) fn dim_outside(painter: &egui::Painter, area: Rect, r: Rect) {
    let dim = Color32::from_black_alpha(120);
    painter.rect_filled(
        Rect::from_min_max(area.min, Pos2::new(area.max.x, r.min.y)),
        0.0,
        dim,
    );
    painter.rect_filled(
        Rect::from_min_max(Pos2::new(area.min.x, r.max.y), area.max),
        0.0,
        dim,
    );
    painter.rect_filled(
        Rect::from_min_max(Pos2::new(area.min.x, r.min.y), Pos2::new(r.min.x, r.max.y)),
        0.0,
        dim,
    );
    painter.rect_filled(
        Rect::from_min_max(Pos2::new(r.max.x, r.min.y), Pos2::new(area.max.x, r.max.y)),
        0.0,
        dim,
    );
    painter.rect_stroke(
        r,
        0.0,
        Stroke::new(2.0_f32, Color32::from_rgb(240, 200, 80)),
    );
}

/// Format a rotation angle for the Transformations text box: whole degrees
/// plainly, otherwise one decimal.
pub(super) fn fmt_angle(v: f32) -> String {
    if v.fract().abs() < 0.05 {
        format!("{}", v.round() as i64)
    } else {
        format!("{v:.1}")
    }
}
