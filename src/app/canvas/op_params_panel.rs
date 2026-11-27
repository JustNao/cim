//! The operator-parameters panel: opened by the ⚙ beside an operator tone
//! (LUT_ALPHA, or Boost when its library exports parameters) and docked to the
//! Transformations panel's right edge, like the export preview to the Export
//! panel. A preset dropdown — the library's presets, then the user's own (✏
//! rename, 🗑 delete) — picks a starting point, and every parameter is listed
//! below it to edit. It follows the selected pane and writes its effective
//! Visualization set, so it rides the tone sync like every other tone option.
//!
//! **Edits commit on release.** Every parameter is a construction parameter, so
//! each committed value set builds a fresh heavy C++ instance; a slider drag is
//! held in [`OpPanel::draft`] and only interned and written to the pane when it
//! is released (or a typed value is confirmed).

use crate::app::*;
use crate::imageproc::Op;
use crate::op_params::{self, Origin, Schema, Sel};

/// The panel's width (it is an `Area`, so it is exactly this wide).
const PANEL_W: f32 = 320.0;
/// How tall the parameter list may grow before it scrolls.
const LIST_MAX_H: f32 = 420.0;

/// The panel's state on [`CimApp`].
#[derive(Default)]
pub(crate) struct OpPanel {
    /// Whether the panel is open (the ⚙ toggles it).
    pub show: bool,
    /// Values being dragged / typed, not yet committed: `(pane id, op, values)`.
    draft: Option<(u64, Op, Vec<f64>)>,
    /// The user preset being renamed and its edit buffer (the field under the
    /// preset picker).
    rename: Option<(u64, String)>,
    /// The "Save as" name field, while it is open.
    save_name: Option<String>,
    /// A field just opened (rename or Save as) and should take the focus once.
    focus: bool,
}

/// The operator a tone runs, when it is one with tunable parameters.
pub(crate) fn tunable_op(contrast: ContrastMode) -> Option<Op> {
    match contrast {
        ContrastMode::LutAlpha => Some(Op::LutAlpha),
        ContrastMode::Boost => Some(Op::Boost),
        _ => None,
    }
}

impl CimApp {
    /// The values pane `idx` builds `op` with: its selection's, else the
    /// library default preset's.
    fn op_values(&self, idx: usize, op: Op, schema: &Schema) -> Vec<f64> {
        self.visual(idx)
            .tone
            .op_params
            .get(op)
            .and_then(|s| op_params::get(s.values))
            .filter(|v| v.len() == schema.n)
            .map_or_else(|| schema.default_values().to_vec(), |v| v.to_vec())
    }

    /// The values a preset origin stands for (`None` for a deleted user preset).
    fn origin_values(&self, op: Op, schema: &Schema, origin: Origin) -> Option<Vec<f64>> {
        match origin {
            Origin::Library(i) => schema.presets.get(i).map(|p| p.values.clone()),
            Origin::User(id) => self
                .op_presets
                .find(op.id(), id)
                .map(|p| op_params::from_keys(schema, &p.params)),
        }
    }

    /// Write `values` (from `origin`) as pane `idx`'s selection for `op`. The
    /// library default preset, untouched, is stored as no selection at all, so
    /// the pane builds through plain `create` exactly as before.
    fn set_op_values(
        &mut self,
        idx: usize,
        op: Op,
        schema: &Schema,
        values: &[f64],
        origin: Origin,
    ) {
        let sel = (origin != Origin::Library(schema.default) || values != schema.default_values())
            .then(|| Sel {
                values: op_params::intern(values),
                origin,
            });
        self.visual_mut(idx).tone.op_params.set(op, sel);
    }

    /// Save the user presets, noting a failure in the status line.
    fn save_op_presets(&mut self) {
        if let Err(e) = self.op_presets.save() {
            self.status.set(t!("op_params.save_failed", err = e));
        }
    }

    /// Draw the panel docked beside the Transformations panel (`panel`) for
    /// pane `idx`, when it is open and the pane's tone has parameters.
    pub(crate) fn draw_op_params_window(&mut self, ctx: &egui::Context, panel: Rect, idx: usize) {
        let Some(op) = tunable_op(self.visual(idx).contrast) else {
            return;
        };
        let Some(schema) = crate::imageproc::schema(op) else {
            return;
        };
        egui::Area::new(Id::new("op_params_panel"))
            .fixed_pos(dock_beside(ctx, panel, PANEL_W))
            .show(ctx, |ui| {
                egui::Frame::window(ui.style()).show(ui, |ui| {
                    ui.set_width(PANEL_W);
                    self.draw_op_params(ui, idx, op, &schema);
                });
            });
    }

    fn draw_op_params(&mut self, ui: &mut egui::Ui, idx: usize, op: Op, schema: &Schema) {
        let pane_id = self.panes[idx].id;
        let committed = self.op_values(idx, op, schema);
        let sel = self.visual(idx).tone.op_params.get(op);
        let origin = sel.map_or(Origin::Library(schema.default), |s| s.origin);
        // A draft belongs to one pane and operator; drop one left over from
        // another (the selection moved, or the tone changed).
        if self
            .op_panel
            .draft
            .as_ref()
            .is_some_and(|(p, o, v)| (*p, *o) != (pane_id, op) || v.len() != schema.n)
        {
            self.op_panel.draft = None;
        }
        let mut values = self
            .op_panel
            .draft
            .as_ref()
            .map_or_else(|| committed.clone(), |d| d.2.clone());

        // ---- Header: title + close ----------------------------------------
        ui.horizontal(|ui| {
            ui.strong(t!("op_params.title", op = op.name()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("✕")
                    .on_hover_text(t!("op_params.close"))
                    .clicked()
                {
                    self.op_panel.show = false;
                }
            });
        });
        ui.separator();

        // ---- Preset picker -------------------------------------------------
        let modified = self
            .origin_values(op, schema, origin)
            .is_none_or(|v| v != values);
        let origin_name = match origin {
            Origin::Library(i) => schema.presets.get(i).map(|p| p.name.clone()),
            Origin::User(id) => self.op_presets.find(op.id(), id).map(|p| p.name.clone()),
        }
        .unwrap_or_else(|| t!("op_params.custom").into_owned());
        let shown = if modified {
            t!("op_params.modified", name = origin_name).into_owned()
        } else {
            origin_name
        };
        let mut pick: Option<(Origin, Vec<f64>)> = None;
        let mut delete: Option<u64> = None;
        let mut rename_done: Option<(u64, String)> = None;
        ui.horizontal(|ui| {
            ui.label(t!("op_params.preset"));
            egui::ComboBox::from_id_salt(("op_preset", pane_id, op.id()))
                .selected_text(shown)
                .width(PANEL_W - 140.0)
                .show_ui(ui, |ui| {
                    for (i, p) in schema.presets.iter().enumerate() {
                        let label = if i == schema.default {
                            t!("op_params.default_preset", name = p.name).into_owned()
                        } else {
                            p.name.clone()
                        };
                        if ui
                            .selectable_label(origin == Origin::Library(i) && !modified, label)
                            .clicked()
                        {
                            pick = Some((Origin::Library(i), p.values.clone()));
                        }
                    }
                    let users = self.op_presets.of(op.id()).to_vec();
                    if !users.is_empty() {
                        ui.separator();
                        ui.weak(t!("op_params.user_presets"));
                    }
                    for p in users {
                        ui.horizontal(|ui| {
                            if ui
                                .selectable_label(
                                    origin == Origin::User(p.id) && !modified,
                                    &p.name,
                                )
                                .clicked()
                            {
                                pick = Some((
                                    Origin::User(p.id),
                                    op_params::from_keys(schema, &p.params),
                                ));
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .small_button("🗑")
                                        .on_hover_text(t!("op_params.delete_hover"))
                                        .clicked()
                                    {
                                        delete = Some(p.id);
                                    }
                                    if ui
                                        .small_button("✏")
                                        .on_hover_text(t!("op_params.rename_hover"))
                                        .clicked()
                                    {
                                        self.op_panel.rename = Some((p.id, p.name.clone()));
                                        self.op_panel.save_name = None;
                                        self.op_panel.focus = true;
                                    }
                                },
                            );
                        });
                    }
                });
            if ui
                .button(t!("op_params.reset"))
                .on_hover_text(t!("op_params.reset_hover"))
                .clicked()
            {
                pick = Some((
                    Origin::Library(schema.default),
                    schema.default_values().to_vec(),
                ));
            }
        });
        // Renaming a user preset: its name in a field under the picker (a combo
        // closes on any click inside it, so the field can't live in the list).
        if self.op_panel.rename.is_some() {
            ui.horizontal(|ui| {
                ui.label(t!("op_params.rename"));
                let buf = &mut self.op_panel.rename.as_mut().unwrap().1;
                let r = ui.add(egui::TextEdit::singleline(buf).desired_width(PANEL_W - 110.0));
                if std::mem::take(&mut self.op_panel.focus) {
                    r.request_focus();
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    self.op_panel.rename = None;
                } else if r.lost_focus() {
                    rename_done = self.op_panel.rename.take();
                }
            });
        }
        if let Some((id, name)) = rename_done {
            let name = name.trim().to_string();
            if !name.is_empty() {
                self.op_presets.rename(op.id(), id, name);
                self.save_op_presets();
            }
        }
        if let Some(id) = delete {
            self.op_presets.delete(op.id(), id);
            self.save_op_presets();
        }
        if let Some((o, v)) = pick {
            self.op_panel.draft = None;
            self.set_op_values(idx, op, schema, &v, o);
            values = v;
        }

        // ---- Save as -------------------------------------------------------
        ui.horizontal(|ui| match &mut self.op_panel.save_name {
            None => {
                if ui
                    .button(t!("op_params.save_as"))
                    .on_hover_text(t!("op_params.save_as_hover"))
                    .clicked()
                {
                    self.op_panel.save_name = Some(String::new());
                    self.op_panel.rename = None;
                    self.op_panel.focus = true;
                }
            }
            Some(name) => {
                let r = ui.add(
                    egui::TextEdit::singleline(name)
                        .hint_text(t!("op_params.name_hint"))
                        .desired_width(PANEL_W - 110.0),
                );
                if std::mem::take(&mut self.op_panel.focus) {
                    r.request_focus();
                }
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let ok = ui.button(t!("op_params.save")).clicked();
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    self.op_panel.save_name = None;
                } else if enter || ok {
                    let name = name.trim().to_string();
                    self.op_panel.save_name = None;
                    if !name.is_empty() {
                        let id =
                            self.op_presets
                                .add(op.id(), name, op_params::to_keys(schema, &values));
                        self.save_op_presets();
                        self.op_panel.draft = None;
                        self.set_op_values(idx, op, schema, &values, Origin::User(id));
                    }
                }
            }
        });
        ui.separator();

        // ---- Parameters ----------------------------------------------------
        let mut commit = false;
        let mut edited = false;
        egui::ScrollArea::vertical()
            .max_height(LIST_MAX_H)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Grid::new(("op_params_grid", pane_id, op.id()))
                    .num_columns(2)
                    .spacing([8.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for p in &schema.params {
                            let range = match (p.min.is_finite(), p.max.is_finite()) {
                                (true, true) => {
                                    t!("op_params.range", min = p.min, max = p.max).into_owned()
                                }
                                (true, false) => {
                                    t!("op_params.range_min", min = p.min).into_owned()
                                }
                                (false, true) => {
                                    t!("op_params.range_max", max = p.max).into_owned()
                                }
                                (false, false) => String::new(),
                            };
                            let label = ui.label(&p.key);
                            if !range.is_empty() {
                                label.on_hover_text(&range);
                            }
                            ui.horizontal_wrapped(|ui| {
                                for v in &mut values[p.offset..p.offset + p.len] {
                                    let lo = if p.min.is_finite() {
                                        p.min
                                    } else {
                                        f64::NEG_INFINITY
                                    };
                                    let hi = if p.max.is_finite() {
                                        p.max
                                    } else {
                                        f64::INFINITY
                                    };
                                    let speed = if p.int {
                                        0.25
                                    } else if p.min.is_finite() && p.max.is_finite() {
                                        (p.max - p.min) / 500.0
                                    } else {
                                        (v.abs() / 100.0).max(0.001)
                                    };
                                    let mut drag =
                                        egui::DragValue::new(v).speed(speed).range(lo..=hi);
                                    if p.int {
                                        drag = drag.fixed_decimals(0);
                                    }
                                    let r = ui.add(drag);
                                    if r.changed() {
                                        edited = true;
                                    }
                                    // A drag is released, a typed value confirmed,
                                    // or a one-off change (arrow key) made outside
                                    // a drag or an edit.
                                    if r.drag_stopped()
                                        || r.lost_focus()
                                        || (r.changed() && !r.dragged() && !r.has_focus())
                                    {
                                        commit = true;
                                    }
                                }
                            });
                            ui.end_row();
                        }
                    });
            });
        for p in &schema.params {
            for v in &mut values[p.offset..p.offset + p.len] {
                *v = p.clamp(*v);
            }
        }
        if edited {
            self.op_panel.draft = Some((pane_id, op, values.clone()));
        }
        if commit {
            self.op_panel.draft = None;
            if values != committed {
                self.set_op_values(idx, op, schema, &values, origin);
            }
        }
    }
}
