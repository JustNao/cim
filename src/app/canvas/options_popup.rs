//! The single global "Transformations" panel (toolbar button / `V`): a floating
//! window whose contents track the currently selected pane (`current`), split
//! into a **Visualization** group (LUT + options, RC/details, Overlay) and a
//! **Geometry** group (rotation), each collapsible and each with its own "Sync"
//! toggle, plus the pane histogram always shown at the bottom.
//! `draw_tone_options` is THE place to add a tone knob (grow the mode's
//! `ToneOptions` sub-struct, add a row, read it in `stage`/`tone_sig`).

use crate::app::*;

use super::transform::*;

impl CimApp {
    /// The global Transformations panel. Its contents follow the selected pane
    /// (`current`); selecting another pane while it's open updates it live. Edits
    /// target the pane's *effective* Transformations — the shared set when the
    /// matching sync group is on (and every synced pane re-renders), otherwise the
    /// pane's own. Nothing is written unless something changed.
    pub(crate) fn draw_transform_panel(&mut self, ctx: &egui::Context) {
        let mut open = self.show_transform;
        // No pane to configure — still show the window (with a hint) so the
        // toolbar toggle has a visible effect.
        if self.panes.is_empty() {
            egui::Window::new("Transformations")
                .open(&mut open)
                .default_pos(ctx.screen_rect().center())
                .pivot(egui::Align2::CENTER_CENTER)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label("No media open.");
                });
            self.show_transform = open;
            return;
        }
        let idx = self.current.min(self.panes.len() - 1);
        let pane_id = self.panes[idx].id;

        // Edit the effective values (shared when the group is synced, else own).
        let mut contrast = self.contrast_of(idx);
        let mut tone = self.tone_of(idx);
        let mut details = self.details_of(idx);
        let mut rotation = self.rotation_of(idx);

        // The proprietary operators (LUT_ALPHA / Details) each need their own
        // loaded library and a single-channel 16-bit frame; gate their controls
        // independently and explain why when disabled.
        let op_input = self.pane_is_op_input(idx);
        let lut_ok = crate::imageproc::lut_alpha_available() && op_input;
        let details_ok = crate::imageproc::details_available() && op_input;
        let lut_hint: &str = if !crate::imageproc::lut_alpha_available() {
            "LUT_ALPHA operator library not found"
        } else {
            "Only available for single-channel 16-bit (uint16) images"
        };
        let details_hint: &str = if !crate::imageproc::details_available() {
            "Details operator library not found"
        } else {
            "Only available for single-channel 16-bit (uint16) images"
        };

        // Overlay: the single-channel media available to tint over this pane — a
        // boolean mask or a grayscale image / sequence — plus the current
        // selection/colour/alpha. Excludes the pane itself; not offered on a mask.
        let sources: Vec<(u64, String)> = self
            .panes
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != idx && self.overlay_source_size(*i).is_some())
            .map(|(_, p)| (p.id, p.media.name().to_string()))
            .collect();
        let self_is_mask = self.panes[idx].media.is_mask();
        let (mut ov_src, mut ov_color, mut ov_alpha) = match self.overlay_of(idx) {
            Some(o) => (Some(o.src_id), o.color, o.opacity),
            None => (None, Color32::from_rgb(240, 60, 60), 0.5),
        };

        // Histogram of this pane's current frame.
        self.ensure_pane_histogram(idx);
        let f = self.frame_disp(idx);
        let have_hist = self.panes[idx].hist.as_ref().map(|h| h.key) == Some((pane_id, f));

        // Whether a group's Sync toggle was flipped this frame — if so, skip that
        // group's edit writeback so enabling sync makes the pane *adopt* the shared
        // set (via `set_sync_*`'s snapshot) rather than pushing its own values into
        // the shared set (the stale effective values read above).
        let mut vis_sync_changed = false;
        let mut geo_sync_changed = false;

        egui::Window::new("Transformations")
            .open(&mut open)
            // Centered on first appearance each run (position then sticks for the
            // session; not persisted across runs — see `persist_egui_memory`).
            .default_pos(ctx.screen_rect().center())
            .pivot(egui::Align2::CENTER_CENTER)
            // `scroll(false)` + `resizable([true, false])` lets the window **shrink**
            // its height to the content — so collapsing a group actually makes the
            // panel shorter (the default Window scroll area would hold it open).
            .scroll(false)
            .resizable([true, false])
            .default_width(290.0)
            .show(ctx, |ui| {
                // The current media's name, so it's clear which pane the panel
                // is acting on (it follows the selection).
                ui.label(
                    egui::RichText::new(format!(
                        "{}  {}",
                        idx + 1,
                        self.panes[idx].media.name()
                    ))
                    .weak(),
                );

                // ---- Visualization group (open by default) -------------------
                // A CollapsingState (not CollapsingHeader) so the group's **Sync**
                // toggle lives on the *right of the header row* — visible and usable
                // while the group is collapsed, without expanding it first.
                let vis_id = ui.make_persistent_id("cim_transform_visualization");
                let mut vis_title_clicked = false;
                let mut vis_header =
                    egui::collapsing_header::CollapsingState::load_with_default_open(
                        ui.ctx(),
                        vis_id,
                        true,
                    )
                    .show_header(ui, |ui| {
                        // Clicking the group *name* (not just the triangle) toggles it.
                        if ui
                            .add(
                                egui::Label::new(egui::RichText::new("Visualization").strong())
                                    .selectable(false)
                                    .sense(egui::Sense::click()),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .clicked()
                        {
                            vis_title_clicked = true;
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let mut sync = self.panes[idx].sync_tone;
                            if ui
                                .checkbox(&mut sync, "Sync")
                                .on_hover_text("Add this pane to the Visualization sync group (tone·details·overlay)")
                                .changed()
                            {
                                self.set_sync_tone(idx, sync);
                                vis_sync_changed = true;
                            }
                        });
                    });
                if vis_title_clicked {
                    vis_header.toggle();
                }
                vis_header.body(|ui| {
                        egui::Grid::new(("vis_grid", pane_id))
                            .num_columns(2)
                            .spacing([8.0, 6.0])
                            .show(ui, |ui| {
                                ui.label("LUT");
                                egui::ComboBox::from_id_salt(("opt_tone", pane_id))
                                    .selected_text(contrast.label())
                                    .width(130.0)
                                    .show_ui(ui, |ui| {
                                        for m in ContrastMode::ORDER {
                                            // LUT_ALPHA needs the library + a 16-bit
                                            // frame; disable it otherwise (unless it's
                                            // already the pane's mode, so it stays
                                            // visible / switchable away).
                                            if m == ContrastMode::LutAlpha
                                                && !lut_ok
                                                && contrast != m
                                            {
                                                ui.add_enabled(
                                                    false,
                                                    egui::SelectableLabel::new(false, m.label()),
                                                )
                                                .on_disabled_hover_text(lut_hint);
                                            } else {
                                                ui.selectable_value(&mut contrast, m, m.label());
                                            }
                                        }
                                    });
                                ui.end_row();

                                draw_tone_options(ui, pane_id, contrast, &mut tone);

                                ui.label("RC");
                                ui.add_enabled(
                                    details_ok,
                                    egui::Checkbox::without_text(&mut details),
                                )
                                .on_hover_text("Rehaussement / sharpening")
                                .on_disabled_hover_text(details_hint);
                                ui.end_row();
                            });

                        // Overlay picker + colour/alpha. Shown for any non-mask pane
                        // (a mask can't itself take an overlay); when no single-channel
                        // media is available to tint, the row still shows with a hint so
                        // the control is discoverable.
                        if !self_is_mask {
                            ui.separator();
                            ui.horizontal(|ui| {
                                ui.label("Overlay");
                                if sources.is_empty() {
                                    ui.label(
                                        egui::RichText::new("no single-channel media")
                                            .weak()
                                            .small(),
                                    );
                                } else {
                                    let sel = ov_src
                                        .and_then(|id| sources.iter().find(|(m, _)| *m == id))
                                        .map(|(_, n)| ellipsize(n, 12))
                                        .unwrap_or_else(|| "None".into());
                                    egui::ComboBox::from_id_salt(("opt_overlay", pane_id))
                                        .selected_text(sel)
                                        .width(120.0)
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(&mut ov_src, None, "None");
                                            for (mid, mname) in &sources {
                                                ui.selectable_value(
                                                    &mut ov_src,
                                                    Some(*mid),
                                                    ellipsize(mname, 18),
                                                );
                                            }
                                        });
                                }
                            });
                            if ov_src.is_some() {
                                ui.horizontal(|ui| {
                                    ui.color_edit_button_srgba(&mut ov_color);
                                    ui.add(
                                        egui::DragValue::new(&mut ov_alpha)
                                            .speed(0.02)
                                            .range(0.0..=1.0)
                                            .fixed_decimals(2)
                                            .prefix("α "),
                                    );
                                });
                            }
                        }
                });

                // ---- Geometry group (collapsed by default) -------------------
                let geo_id = ui.make_persistent_id("cim_transform_geometry");
                let mut geo_title_clicked = false;
                let mut geo_header =
                    egui::collapsing_header::CollapsingState::load_with_default_open(
                        ui.ctx(),
                        geo_id,
                        false,
                    )
                    .show_header(ui, |ui| {
                        // Clicking the group *name* (not just the triangle) toggles it.
                        if ui
                            .add(
                                egui::Label::new(egui::RichText::new("Geometry").strong())
                                    .selectable(false)
                                    .sense(egui::Sense::click()),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .clicked()
                        {
                            geo_title_clicked = true;
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let mut gsync = self.panes[idx].sync_geometry;
                            if ui
                                .checkbox(&mut gsync, "Sync")
                                .on_hover_text("Add this pane to the Geometry sync group (rotation)")
                                .changed()
                            {
                                self.set_sync_geometry(idx, gsync);
                                geo_sync_changed = true;
                            }
                        });
                    });
                if geo_title_clicked {
                    geo_header.toggle();
                }
                geo_header.body(|ui| {
                        // Display rotation (about the image centre). Also editable
                        // directly on the pane with Alt + drag.
                        ui.horizontal(|ui| {
                            ui.label("Rotate");
                            // Drag bar (its numeric readout is the text box).
                            ui.add(
                                egui::Slider::new(&mut rotation, -180.0..=180.0)
                                    .step_by(1.0)
                                    .show_value(false),
                            )
                            .on_hover_text("Rotate the image (Alt + drag on the pane)");

                            // Manual angle entry: a click selects the whole value so
                            // it can be typed straight over; committed on Enter /
                            // focus loss. While the field is focused, keep the buffer
                            // as-typed; otherwise mirror the live angle.
                            if self.rotation_edit_pane != Some(pane_id) {
                                self.rotation_edit = fmt_angle(rotation);
                            }
                            let mut out = egui::TextEdit::singleline(&mut self.rotation_edit)
                                .id(Id::new(("rot_edit", pane_id)))
                                .desired_width(30.0)
                                .show(ui);
                            if out.response.gained_focus() {
                                self.rotation_edit_pane = Some(pane_id);
                                let end = self.rotation_edit.chars().count();
                                out.state.cursor.set_char_range(Some(
                                    egui::text::CCursorRange::two(
                                        egui::text::CCursor::new(0),
                                        egui::text::CCursor::new(end),
                                    ),
                                ));
                                out.state.store(ui.ctx(), out.response.id);
                            }
                            if out.response.lost_focus() {
                                if let Ok(v) = self
                                    .rotation_edit
                                    .trim()
                                    .trim_end_matches('°')
                                    .trim()
                                    .parse::<f32>()
                                {
                                    rotation = wrap180(v);
                                }
                                self.rotation_edit_pane = None;
                            }
                            ui.label("°");
                            if ui
                                .add(egui::Button::new("Reset"))
                                .on_hover_text("Reset to 0°")
                                .clicked()
                            {
                                rotation = 0.0;
                                self.rotation_edit_pane = None;
                            }
                        });
                    });

                // ---- Histogram (always visible, at the bottom) ---------------
                ui.separator();
                ui.strong("Histogram");
                if have_hist {
                    self.draw_histogram(ui, idx);
                } else {
                    ui.label(egui::RichText::new("frame not loaded").weak().small());
                }
            });
        self.show_transform = open;

        // Reconcile the Visualization edits (overlay + tone), unless the Sync
        // toggle was just flipped (then the pane adopts the shared set instead of
        // pushing its own stale effective values into it).
        if !vis_sync_changed {
            // Reconcile the overlay. It rides the Visualization sync: when synced, edit
            // the shared overlay and rebuild every synced pane's tinted texture;
            // otherwise just this pane's. A newly *selected* source must match this
            // pane's pixel size — reject a mismatch with an error popup (colour/alpha
            // edits on the same source skip the check).
            let synced = self.panes[idx].sync_tone;
            let cur = self.overlay_of(idx);
            let new = ov_src.map(|src_id| OverlaySpec {
                src_id,
                color: ov_color,
                opacity: ov_alpha,
            });
            if cur != new {
                let src_changed = new.map(|n| n.src_id) != cur.map(|c| c.src_id);
                let size_ok = match new.filter(|_| src_changed) {
                    Some(spec) => match self.panes.iter().position(|p| p.id == spec.src_id) {
                        Some(src) => {
                            let (base, ov) = (self.disp_size(idx), self.disp_size(src));
                            if base == ov {
                                true
                            } else {
                                let sname = self.panes[src].media.name().to_string();
                                self.error_popup = Some(format!(
                                    "Overlay size mismatch\n\n\
                                 This image is {}×{} but the overlay “{sname}” is {}×{}.\n\
                                 An overlay must match the image dimensions.",
                                    base[0], base[1], ov[0], ov[1],
                                ));
                                false
                            }
                        }
                        None => false, // source vanished; leave the overlay unchanged
                    },
                    None => true, // clearing, or a colour/alpha edit on the same source
                };
                if size_ok {
                    if synced {
                        self.shared_overlay = new;
                        for p in &mut self.panes {
                            if p.sync_tone {
                                p.overlay_tex = None;
                            }
                        }
                    } else {
                        self.panes[idx].overlay = new;
                        self.panes[idx].overlay_tex = None;
                    }
                }
            }

            // Write the effective tone back (own or shared). No texture nulling: every
            // synced pane's `tone_sig` now reflects the new shared tone, so `stage`
            // re-renders and commits each while it keeps showing its last frame —
            // nulling `tex` would flash a heavy LUT_ALPHA/details render to black.
            if synced {
                if self.shared_contrast != contrast
                    || self.shared_tone != tone
                    || self.shared_details != details
                {
                    self.shared_contrast = contrast;
                    self.shared_tone = tone;
                    self.shared_details = details;
                }
            } else {
                let p = &mut self.panes[idx];
                if p.contrast != contrast || p.tone != tone || p.details != details {
                    p.contrast = contrast;
                    p.tone = tone;
                    p.details = details;
                    // No texture invalidation: the new tone changes `tone_sig`, so
                    // `stage` re-renders and commits the fresh frame while the pane
                    // keeps showing its last committed `tex` — nulling it here would
                    // blank a heavy (async) LUT_ALPHA/details render to black instead.
                }
            }
        } // !vis_sync_changed

        // Rotation is applied at draw time (no texture to invalidate); it rides
        // the Geometry sync, so a synced edit turns every synced pane. Skip when
        // the Geometry Sync toggle was just flipped (adopt the shared angle).
        if !geo_sync_changed {
            rotation = wrap180(rotation);
            if self.rotation_of(idx) != rotation {
                self.set_rotation(idx, rotation);
            }
        }
    }
}

/// Render the tone options for `mode` as label/value rows inside a 2-column
/// `Grid` (each row ends with `end_row`). Extend by adding a `match` arm or a
/// row: each mode reads/writes its own sub-struct of `ToneOptions`, so options
/// never collide across modes.
fn draw_tone_options(ui: &mut egui::Ui, _pane_id: u64, mode: ContrastMode, tone: &mut ToneOptions) {
    match mode {
        // Linear and Colormap share the same bounds controls (clip + share);
        // Colormap additionally picks a palette.
        ContrastMode::Linear => draw_clip_and_share(ui, tone),
        ContrastMode::Colormap => {
            ui.label("Palette");
            egui::ComboBox::from_id_salt(("opt_palette", _pane_id))
                .selected_text(tone.palette.label())
                .width(130.0)
                .show_ui(ui, |ui| {
                    for p in crate::palette::Palette::ORDER {
                        ui.selectable_value(&mut tone.palette, p, p.label());
                    }
                });
            ui.end_row();
            draw_clip_and_share(ui, tone);
        }
        // LUT_ALPHA has no options. Add a knob here: one row + a field on
        // `ToneOptions`.
        ContrastMode::LutAlpha => {}
    }
}

/// The clip toggle/percentile and the "Share clip" row, shared by the Linear and
/// Colormap tones (both stretch the native range the same way before display).
fn draw_clip_and_share(ui: &mut egui::Ui, tone: &mut ToneOptions) {
    // Clip toggle + its per-tail percentile (percentile greyed only when the
    // toggle is off). On for >8-bit by default, off for 8-bit (set in add_pane).
    // These are NOT greyed out under "Share clip": since Share clip rides the
    // Transformations sync, the clip toggle/percentile edited on any synced pane
    // is the very statistic the Control media derives its shared [lo, hi] from.
    ui.label("Clip");
    ui.horizontal(|ui| {
        ui.add(egui::Checkbox::without_text(&mut tone.clip.enabled))
            .on_hover_text("Clip a percentile off each tail before the stretch");
        ui.add_enabled(
            tone.clip.enabled,
            egui::DragValue::new(&mut tone.clip.percent)
                .speed(0.01)
                .range(0.0..=49.0)
                .max_decimals(2)
                .suffix(" %"),
        )
        .on_hover_text("Percentile clipped at each tail before the stretch");
        // Reset the percentile back to the default.
        let default_pct = crate::settings::ClipOptions::default().percent;
        if ui
            .add_enabled(
                tone.clip.enabled && tone.clip.percent != default_pct,
                egui::Button::new("Reset"),
            )
            .on_hover_text(format!("Reset to the default ({default_pct} %)"))
            .clicked()
        {
            tone.clip.percent = default_pct;
        }
    });
    ui.end_row();

    // Share clip: apply the Control media's exact [lo, hi] LUT to every pane, so
    // all panes share identical clamp/scaling (even if it over/under-saturates
    // some) rather than each auto-normalising. Rides the Transformations sync.
    ui.label("Share clip");
    ui.add(egui::Checkbox::without_text(&mut tone.share_clip))
        .on_hover_text(
            "Apply the Control media's exact LUT (lo/hi) to every pane — identical clamp/scaling",
        );
    ui.end_row();
}
