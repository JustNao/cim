//! The in-pane Compute controls: the config form (mode + source pickers +
//! Compute) while unconfigured, then the Refresh / Save / Auto-refresh row
//! over the result. The compute engine itself is in app (recompute_pane).

use crate::app::*;

impl CimApp {
    /// Overlay a Compute pane with a top-left foreground `Area`. Two states:
    /// **unconfigured** shows the config form (mode + source combos + a
    /// **Compute** button that runs it); once computed, the result image shows
    /// with the **Refresh** / **Save** / **Auto refresh** controls instead.
    /// Edits are written back and a recompute / save is dispatched after.
    pub(super) fn draw_compute_ui(
        &mut self,
        ctx: &egui::Context,
        idx: usize,
        img_area: Rect,
        header_height: f32,
    ) {
        let pane_id = self.panes[idx].id;
        let (
            mut kind,
            mut source_id,
            mut source_b,
            computed,
            mut auto,
            mut saving,
            mut save_name,
            status,
        ) = {
            let c = self.panes[idx].compute.as_ref().unwrap();
            (
                c.kind,
                c.source_id,
                c.source_b,
                c.computed,
                c.auto,
                c.saving,
                c.save_name.clone(),
                c.status.clone(),
            )
        };
        let sources = self.compute_sources(kind);
        let mut recompute = false;
        let mut do_save = false;

        // Bottom-left of the cell: anchor within a clip rect that stops just
        // above the footer strip, with a small inset offset so the frame's
        // border doesn't land on (and get clipped by) the clip edge.
        egui::Area::new(Id::new(("compute_ctrl", pane_id)))
            .order(egui::Order::Foreground)
            .movable(false)
            .constrain_to(img_area)
            .anchor(egui::Align2::LEFT_TOP, Vec2::new(6.0, header_height + 6.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(240.0);
                    if !computed {
                        // Config form: pick the mode + source(s), then Compute.
                        ui.label(egui::RichText::new("New compute").strong());
                        compute_config_rows(
                            ui,
                            pane_id,
                            &sources,
                            &mut kind,
                            &mut source_id,
                            &mut source_b,
                        );
                        let ready = source_id.is_some()
                            && (!matches!(kind, media::Reduce::Diff) || source_b.is_some());
                        if ui
                            .add_enabled(ready, egui::Button::new("Compute"))
                            .clicked()
                        {
                            recompute = true;
                        }
                    } else {
                        // Result controls (the form is replaced by the output).
                        ui.horizontal(|ui| {
                            if ui.button("Refresh").clicked() {
                                recompute = true;
                            }
                            if !saving && ui.button("Save").clicked() {
                                saving = true;
                            }
                            ui.checkbox(&mut auto, "Auto refresh");
                        });
                        // Inline save: a name field (relative to the working dir).
                        if saving {
                            ui.add(
                                egui::TextEdit::singleline(&mut save_name)
                                    .desired_width(220.0)
                                    .hint_text("name.tif"),
                            );
                            ui.horizontal(|ui| {
                                if ui.button("Save").clicked() {
                                    do_save = true;
                                }
                                if ui.button("Cancel").clicked() {
                                    saving = false;
                                }
                            });
                        }
                    }
                    if !status.is_empty() {
                        ui.label(egui::RichText::new(&status).weak().small());
                    }
                });
            });

        // Write edits back, then dispatch heavier work outside the closures.
        {
            let c = self.panes[idx].compute.as_mut().unwrap();
            c.kind = kind;
            c.source_id = source_id;
            c.source_b = source_b;
            c.auto = auto;
            c.saving = saving;
            c.save_name = save_name.clone();
        }
        if recompute {
            // Defer to the top of the next update (before `refresh_textures`) so
            // the fresh result re-renders in the same lock-step commit as the
            // others (`recompute_pane` bumps `render_gen` and keeps the last
            // texture, so the pane never draws black).
            self.pending_recompute = Some(idx);
        }
        if do_save {
            self.save_computed(idx, &save_name);
        }
    }
}

/// The mode + source combo rows shared by the floating compute draft and a
/// realized Compute pane. `salt` disambiguates widget ids between the two.
/// `sources` is the caller's kind-filtered source list. Returns true if any
/// selection changed (so the caller can recompute). Diff shows an A and a B
/// picker; the reductions show a single Source.
fn compute_config_rows(
    ui: &mut egui::Ui,
    salt: u64,
    sources: &[(u64, String)],
    kind: &mut Reduce,
    source_id: &mut Option<u64>,
    source_b: &mut Option<u64>,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label("Mode");
        egui::ComboBox::from_id_salt(("ckind", salt))
            .selected_text(kind.label())
            .show_ui(ui, |ui| {
                for k in [Reduce::Mean, Reduce::Std, Reduce::Diff] {
                    if ui.selectable_value(kind, k, k.label()).clicked() {
                        changed = true;
                    }
                }
            });
    });
    let diff = matches!(*kind, Reduce::Diff);
    let mut pick = |ui: &mut egui::Ui, label: &str, id: &str, sel: &mut Option<u64>| {
        ui.horizontal(|ui| {
            ui.label(label);
            let cur = sel
                .and_then(|s| sources.iter().find(|(m, _)| *m == s))
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| "—".into());
            egui::ComboBox::from_id_salt((id, salt))
                .selected_text(cur)
                .show_ui(ui, |ui| {
                    for (mid, mname) in sources {
                        if ui
                            .selectable_value(sel, Some(*mid), format!("{} {}", mid, mname))
                            .clicked()
                        {
                            changed = true;
                        }
                    }
                });
        });
    };
    pick(ui, if diff { "A " } else { "Source " }, "csrc", source_id);
    if diff {
        pick(ui, "B ", "csrcb", source_b);
    }
    changed
}
