//! The per-pane "Transformations" options popup: tone mode + its options,
//! the Details toggle, rotation, the overlay picker, and the pane histogram.
//! `draw_tone_options` is THE place to add a tone knob (grow the mode's
//! `ToneOptions` sub-struct, add a row, read it in `stage`/`tone_sig`).

use crate::app::*;

use super::transform::*;

impl CimApp {
    /// The pane options popup (toggled by the header "Transformations" button):
    /// the tone mode, its mode-specific options (`draw_tone_options`), and
    /// Details. Drawn as a foreground `Area` under the header, constrained to the
    /// pane `cell`. When the pane is tone-synced, edits target the *shared*
    /// Transformations (and every synced pane re-renders); otherwise the pane's
    /// own. Nothing is written unless something changed.
    pub(super) fn draw_options_popup(&mut self, ctx: &egui::Context, idx: usize, cell: Rect) {
        let pane_id = self.panes[idx].id;
        let synced = self.panes[idx].sync_tone;
        // Edit the effective values (shared when synced, else the pane's own).
        let mut contrast = self.contrast_of(idx);
        let mut tone = self.tone_of(idx);
        let mut details = self.details_of(idx);
        // Effective rotation (shared when tone-synced, else the pane's own).
        let mut rotation = self.rotation_of(idx);
        let mut close = false;

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

        // Overlay (moved here from the Media manager): the single-channel media
        // available to tint over this pane — a boolean mask or a grayscale image /
        // sequence — plus the current selection/colour/alpha. Excludes the pane
        // itself; not offered on a mask pane target.
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

        // Histogram of this pane's current frame (folded in from Visualise).
        self.ensure_pane_histogram(idx);
        let f = self.frame_disp(idx);
        let have_hist = self.panes[idx].hist.as_ref().map(|h| h.key) == Some((pane_id, f));

        egui::Area::new(Id::new(("pane_opts", pane_id)))
            .order(egui::Order::Foreground)
            .movable(false)
            .constrain_to(cell)
            .fixed_pos(Pos2::new(
                cell.left() + 4.0,
                cell.top() + header_h_for(cell.width()) + 2.0,
            ))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(230.0);
                    ui.horizontal(|ui| {
                        ui.strong("Transformations");
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.small_button("×").clicked() {
                                    close = true;
                                }
                            },
                        );
                    });
                    if synced {
                        ui.label(
                            egui::RichText::new("Transformations synced")
                                .weak()
                                .small(),
                        );
                    }
                    ui.separator();

                    egui::Grid::new(("opt_grid", pane_id))
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
                            ui.add_enabled(details_ok, egui::Checkbox::without_text(&mut details))
                                .on_hover_text("Rehaussement / sharpening")
                                .on_disabled_hover_text(details_hint);
                            ui.end_row();

                            // Display rotation (about the image centre). Also
                            // editable directly on the pane with Alt + drag.
                            ui.label("Rotate");
                            ui.horizontal(|ui| {
                                // Drag bar (its numeric readout is the text box).
                                ui.add(
                                    egui::Slider::new(&mut rotation, -180.0..=180.0)
                                        .step_by(1.0)
                                        .show_value(false),
                                )
                                .on_hover_text("Rotate the image (Alt + drag on the pane)");

                                // Manual angle entry: a click selects the whole
                                // value so it can be typed straight over; committed
                                // on Enter / focus loss. While this pane's field is
                                // focused, keep the buffer as-typed; otherwise mirror
                                // the live angle.
                                if self.rotation_edit_pane != Some(pane_id) {
                                    self.rotation_edit = fmt_angle(rotation);
                                }
                                let mut out = egui::TextEdit::singleline(&mut self.rotation_edit)
                                    .id(Id::new(("rot_edit", pane_id)))
                                    .desired_width(44.0)
                                    .show(ui);
                                if out.response.gained_focus() {
                                    self.rotation_edit_pane = Some(pane_id);
                                    // Select the whole current value on focus.
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
                                // A full interact-height button (not `small_button`,
                                // which zeroes the vertical padding) so the tall ⟲
                                // icon glyph is centred with room above/below rather
                                // than clipped at the top by the taller slider row.
                                if ui
                                    .add(
                                        egui::Button::new("⟲")
                                            .min_size(egui::vec2(0.0, ui.spacing().interact_size.y)),
                                    )
                                    .on_hover_text("Reset to 0°")
                                    .clicked()
                                {
                                    rotation = 0.0;
                                    self.rotation_edit_pane = None;
                                }
                            });
                            ui.end_row();
                        });

                    // Overlay picker + colour/alpha (non-mask panes only).
                    if !self_is_mask && !sources.is_empty() {
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label("Overlay");
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

                    ui.separator();
                    ui.strong("Histogram");
                    if have_hist {
                        self.draw_histogram(ui, idx);
                    } else {
                        ui.label(egui::RichText::new("frame not loaded").weak().small());
                    }
                });
            });

        // Reconcile the overlay. It rides the tone-sync: when synced, edit the
        // shared overlay and rebuild every synced pane's tinted texture; otherwise
        // just this pane's. A newly *selected* source must match this pane's pixel
        // size — reject a mismatch with an error popup (colour/alpha edits on the
        // same source skip the check).
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
        // Rotation is applied at draw time (no texture to invalidate); it rides
        // the Transformations sync, so a synced edit turns every synced pane.
        rotation = wrap180(rotation);
        if self.rotation_of(idx) != rotation {
            self.set_rotation(idx, rotation);
        }
        if close {
            self.panes[idx].show_opts = false;
        }
    }
}

/// Render the tone options for `mode` as label/value rows inside a 2-column
/// `Grid` (each row ends with `end_row`). Extend by adding a `match` arm or a
/// row: each mode reads/writes its own sub-struct of `ToneOptions`, so options
/// never collide across modes.
fn draw_tone_options(
    ui: &mut egui::Ui,
    _pane_id: u64,
    mode: ContrastMode,
    tone: &mut ToneOptions,
) {
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
    // Clip toggle + its per-tail percentile (greyed out when the toggle is off,
    // or when "Share clip" overrides this pane's own bounds). On for >8-bit by
    // default, off for 8-bit (set in add_pane).
    let shared = tone.share_clip;
    ui.label("Clip");
    ui.horizontal(|ui| {
        ui.add_enabled(!shared, egui::Checkbox::without_text(&mut tone.clip.enabled))
            .on_hover_text("Clip a percentile off each tail before the stretch");
        ui.add_enabled(
            !shared && tone.clip.enabled,
            egui::DragValue::new(&mut tone.clip.percent)
                .speed(0.005)
                .range(0.0..=49.0)
                .max_decimals(3)
                .suffix(" %"),
        )
        .on_hover_text("Percentile clipped at each tail before the stretch");
    });
    ui.end_row();

    // Share clip: lock this pane's display bounds to the Control media's
    // [lo, hi], so panes share identical bounds (overrides this pane's own clip).
    ui.label("Share clip");
    ui.add(egui::Checkbox::without_text(&mut tone.share_clip)).on_hover_text(
        "Use the Control media's display bounds (lo/hi) — lock panes to identical bounds",
    );
    ui.end_row();
}
