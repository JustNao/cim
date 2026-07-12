//! The shift+right-drag intensity-profile line overlay: hit-testing /
//! endpoint-vs-body dragging and the amber segment drawing. Like the stats
//! region the line lives in image space and replicates on every pane; the
//! plot window itself is app/profile.rs.

use crate::app::*;

impl CimApp {
    /// Process the shift+right-drag profile line for pane `idx` and draw it (the
    /// amber segment + endpoint handles), mapped onto this pane. `coord_area`
    /// maps screen↔image; `clip_rect` bounds the visible side / where a drag may
    /// start. The line itself lives in image space, so it replicates on every
    /// pane and can be edited from any of them.
    pub(super) fn line_overlay_for_pane(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        idx: usize,
        coord_area: Rect,
        clip_rect: Rect,
        hovered: bool,
    ) {
        self.line_input(ctx, idx, coord_area, clip_rect, hovered);
        self.draw_line_overlay(ui, idx, coord_area, clip_rect);
    }

    // ---- right-drag statistics region ------------------------------------
    // ---- shift+right-drag intensity-profile line -------------------------
    /// Draw the profile line and its endpoint handles onto pane `idx`.
    fn draw_line_overlay(&self, ui: &egui::Ui, idx: usize, coord_area: Rect, clip: Rect) {
        let Some(lp) = self.line_profile else { return };
        let sa = self.rot_img_to_screen(idx, lp.a.to_vec2(), coord_area);
        let sb = self.rot_img_to_screen(idx, lp.b.to_vec2(), coord_area);
        let painter = ui.painter_at(clip);
        painter.line_segment([sa, sb], Stroke::new(2.0_f32, LINE_COL));
        for p in [sa, sb] {
            painter.circle_filled(p, 4.0, LINE_COL);
            painter.circle_stroke(p, 4.0, Stroke::new(1.0_f32, Color32::from_black_alpha(180)));
        }
    }

    /// Track the shift+right drag: on press, decide whether it grabs an endpoint,
    /// the body, or draws a new line; follow while held; finalize on release
    /// (a near-zero *new* line is discarded).
    fn line_input(
        &mut self,
        ctx: &egui::Context,
        idx: usize,
        coord_area: Rect,
        hit_rect: Rect,
        hovered: bool,
    ) {
        if self.selecting_region {
            return;
        }
        let down = ctx.input(|i| i.pointer.secondary_down());
        let shift = ctx.input(|i| i.modifiers.shift);
        let pos = ctx.input(|i| i.pointer.interact_pos());
        match self.line_grab {
            None => {
                if !(shift && down && hovered) {
                    return;
                }
                let Some(p) = pos.filter(|p| hit_rect.contains(*p)) else {
                    return;
                };
                let img = self.rot_screen_to_img(idx, p, coord_area).to_pos2();
                // Grab an existing endpoint / body when the press lands on it,
                // otherwise start a fresh line anchored here.
                let grab = match self.line_profile {
                    Some(lp) => {
                        let sa = self.rot_img_to_screen(idx, lp.a.to_vec2(), coord_area);
                        let sb = self.rot_img_to_screen(idx, lp.b.to_vec2(), coord_area);
                        if (p - sa).length() <= LINE_HANDLE {
                            LineGrab::Start
                        } else if (p - sb).length() <= LINE_HANDLE {
                            LineGrab::End
                        } else if dist_to_segment(p, sa, sb) <= LINE_HANDLE {
                            LineGrab::Body
                        } else {
                            LineGrab::New(img)
                        }
                    }
                    None => LineGrab::New(img),
                };
                if let LineGrab::New(anchor) = grab {
                    self.line_profile = Some(LineProfile { a: anchor, b: img });
                }
                self.line_grab = Some(grab);
                self.line_grab_pane = Some(idx);
                self.line_grab_area = coord_area;
                self.line_drag_last = Some(img);
            }
            Some(grab) if self.line_grab_pane == Some(idx) => {
                if !down {
                    self.finalize_line();
                    return;
                }
                let Some(p) = pos else { return };
                let img = self.rot_screen_to_img(idx, p, self.line_grab_area).to_pos2();
                if let Some(lp) = self.line_profile.as_mut() {
                    match grab {
                        LineGrab::Start => lp.a = img,
                        LineGrab::End | LineGrab::New(_) => lp.b = img,
                        LineGrab::Body => {
                            if let Some(last) = self.line_drag_last {
                                let d = img - last;
                                lp.a += d;
                                lp.b += d;
                            }
                        }
                    }
                }
                self.line_drag_last = Some(img);
            }
            _ => {}
        }
    }

    /// End a profile-line drag; a *new* line dragged out to near-zero length is
    /// discarded (so a stray shift+right-click doesn't leave a dot behind).
    fn finalize_line(&mut self) {
        let grab = self.line_grab.take();
        self.line_grab_pane = None;
        self.line_drag_last = None;
        if matches!(grab, Some(LineGrab::New(_))) {
            if let Some(lp) = self.line_profile {
                if (lp.a - lp.b).length() < 2.0 {
                    self.line_profile = None;
                }
            }
        }
    }
}
