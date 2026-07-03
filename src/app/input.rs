//! Keyboard actions, playback advancement, and file drops.

use super::*;

impl CimApp {
    pub(super) fn apply_action(&mut self, action: Action, ctx: &egui::Context) {
        let n = self.panes.len();
        match action {
            Action::ToggleView => {
                self.mode = match self.mode {
                    Mode::Grid => Mode::Single,
                    Mode::Single => Mode::Ab,
                    Mode::Ab => Mode::Grid,
                };
            }
            Action::ViewGrid => self.mode = Mode::Grid,
            Action::ViewSingle => self.mode = Mode::Single,
            Action::ViewAb => self.mode = Mode::Ab,
            Action::NextMedia if n > 0 => self.current = (self.current + 1) % n,
            Action::PrevMedia if n > 0 => self.current = (self.current + n - 1) % n,
            Action::NextFrame => {
                self.pending_seek = None; // manual step cancels an automatic seek
                let tl = self.timeline_len();
                if self.shared_frame + 1 < tl {
                    self.shared_frame += 1;
                } else if self.current_at_end() {
                    self.shared_frame = 0; // wrap only once the real length is known
                }
                // else hold at the frontier; lookahead extends it shortly
            }
            Action::PrevFrame => {
                self.pending_seek = None; // manual step cancels an automatic seek
                let tl = self.timeline_len();
                if self.shared_frame > 0 {
                    self.shared_frame -= 1;
                } else if self.current_at_end() {
                    self.shared_frame = tl - 1;
                }
            }
            Action::ResetView => {
                self.shared_view.needs_fit = true;
                for p in &mut self.panes {
                    p.transform.needs_fit = true;
                }
            }
            Action::ActualSize if n > 0 => {
                let size = self.panes[self.current].media.size();
                self.view_mut(self.current).actual_size(size);
            }
            Action::ZoomIn if n > 0 => {
                let a = self.last_area;
                self.view_mut(self.current).zoom_at(1.25, a.center(), a);
            }
            Action::ZoomOut if n > 0 => {
                let a = self.last_area;
                self.view_mut(self.current).zoom_at(1.0 / 1.25, a.center(), a);
            }
            Action::LoadAll => self.load_all(),
            Action::OpenFiles => self.open_dialog(),
            Action::ToggleSettings => self.show_settings = !self.show_settings,
            Action::ToggleManager => self.show_manager = !self.show_manager,
            Action::ToggleVis => self.show_vis = !self.show_vis,
            Action::ToggleExport => self.toggle_export(),
            Action::PlayPause => self.playing = !self.playing,
            Action::SelectMedia(i) if i < n => {
                self.current = i;
                self.mode = Mode::Single;
            }
            _ => {}
        }
        ctx.request_repaint();
    }

    pub(super) fn advance_playback(&mut self, ctx: &egui::Context) {
        if !self.playing {
            return;
        }
        let tl = self.timeline_len();
        let at_end = self.current_at_end();
        if tl <= 1 && at_end {
            return;
        }
        let dt = ctx.input(|i| i.stable_dt).min(0.25);
        self.play_accum += dt;
        let step = 1.0 / self.fps.max(0.1);
        while self.play_accum >= step {
            self.play_accum -= step;
            if self.shared_frame + 1 < tl {
                self.shared_frame += 1;
            } else if at_end {
                self.shared_frame = 0;
            } else {
                // At the discovered frontier — wait for the next page rather than
                // wrapping early; drop the backlog so we don't burst afterwards.
                self.play_accum = 0.0;
                break;
            }
            for p in &mut self.panes {
                if !p.sync_temporal {
                    let c = p.media.frame_count();
                    if p.frame + 1 < c {
                        p.frame += 1;
                    } else if p.media.at_end() {
                        p.frame = 0;
                    }
                }
            }
        }
    }

    // ---- input -----------------------------------------------------------

    pub(super) fn handle_input(&mut self, ctx: &egui::Context) {
        if let Some(action) = self.rebinding {
            let key = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Key {
                        key, pressed: true, ..
                    } => Some(*key),
                    _ => None,
                })
            });
            if let Some(k) = key {
                if k != Key::Escape {
                    self.config.keybindings.set(action, k);
                    self.config.save();
                }
                self.rebinding = None;
            }
            return;
        }

        for action in Action::all() {
            if let Some(key) = self.config.keybindings.key_for(action) {
                if ctx.input(|i| i.key_pressed(key)) {
                    self.apply_action(action, ctx);
                }
            }
        }

        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            self.open_paths(dropped);
        }
    }
}
