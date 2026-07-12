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
                self.playback.prefetch = None; // …and any in-flight playback step
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
                self.playback.prefetch = None; // …and any in-flight playback step
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
            Action::ToggleVis => {
                // The Visualise window folded into the per-pane Transformations
                // popup; this now toggles it for the focused pane.
                if let Some(p) = self.panes.get_mut(self.current) {
                    p.show_opts = !p.show_opts;
                }
            }
            Action::ToggleExport => self.toggle_export(),
            Action::PlayPause => self.playback.playing = !self.playback.playing,
            Action::ReloadMedia if n > 0 => self.pending_reload = Some(self.current.min(n - 1)),
            Action::ReloadAll => self.pending_reload_all = true,
            Action::HideMedia if n > 0 => {
                self.panes[self.current.min(n - 1)].visible = false;
                self.reselect_if_hidden();
            }
            Action::SelectMedia(i) if i < n => {
                self.current = i;
                self.mode = Mode::Single;
            }
            _ => {}
        }
        ctx.request_repaint();
    }

    pub(super) fn advance_playback(&mut self, ctx: &egui::Context) {
        if !self.playback.playing {
            self.playback.prefetch = None; // pausing abandons any in-flight step
            return;
        }
        // Playback is render-gated: a step is pre-rendered into `play_prefetch`,
        // and the timeline only advances once every on-screen pane has that frame
        // ready (`refresh_textures` clears it on commit). While one is in flight,
        // wait — so a slow operator paces playback instead of the counter racing
        // ahead of the image. Drop the backlog so we don't burst once it lands.
        if self.playback.prefetch.is_some() {
            self.playback.accum = 0.0;
            return;
        }
        let tl = self.timeline_len();
        let at_end = self.current_at_end();
        // Loop window: a user sub-range, else the whole sequence. When it's the
        // full sequence and the end isn't discovered yet, `hi` is only the
        // frontier — hold there rather than wrapping early.
        let (lo, hi) = self.loop_bounds(tl);
        let full = self.playback.loop_range.is_none();
        if hi <= lo && at_end {
            return;
        }
        let dt = ctx.input(|i| i.stable_dt).min(0.25);
        self.playback.accum += dt;
        let step = 1.0 / self.playback.fps.max(0.1);
        if self.playback.accum < step {
            return;
        }
        // One frame per commit — the render gate, not the accumulator, paces us
        // when an operator is slow, so never carry a backlog into a burst.
        self.playback.accum = 0.0;

        // Fast-forward: advance by `ff` frames per step, skimming the ones in
        // between (they're never staged, and the frontier is discovered by header
        // only — see `ensure_lookahead`). `1` = play every frame.
        let ff = self.playback.fast_forward.max(1);
        let f = self.shared_frame;
        let next = if f < lo {
            Some(lo) // jump into the window
        } else if f < hi {
            Some((f + ff).min(hi)) // stride, but never overshoot the window end
        } else if full && !at_end {
            None // at the frontier of a still-discovering sequence: hold
        } else if self.playback.loop_playback {
            Some(lo) // wrap to the window start
        } else {
            self.playback.playing = false; // stop on the last frame of the window
            None
        };
        if let Some(nf) = next {
            // Pre-render this frame; `refresh_textures` commits it (and applies it
            // to `shared_frame`) once all panes are ready.
            self.playback.prefetch = Some(nf);
            // Advance unsynced panes' own timelines in step, staged the same way.
            for p in &mut self.panes {
                if !p.sync_temporal {
                    let c = p.media.frame_count();
                    if p.frame + 1 < c {
                        p.frame = (p.frame + ff).min(c - 1);
                    } else if p.media.at_end() {
                        p.frame = 0;
                    }
                }
            }
        }
    }

    /// Put the current view command line on the system clipboard (shared by the
    /// Ctrl+Shift+C shortcut and the "View command" window's button). Uses egui's
    /// clipboard channel so it goes through eframe's backend on every platform.
    pub(super) fn copy_view_command(&mut self, ctx: &egui::Context) {
        if self.panes.is_empty() {
            return;
        }
        ctx.copy_text(self.view_command());
        self.status.set("View command copied to clipboard");
    }

    // ---- input -----------------------------------------------------------

    pub(super) fn handle_input(&mut self, ctx: &egui::Context) {
        if let Some(action) = self.rebinding {
            // Capture the pressed key together with the modifiers held at that
            // moment, so a chord like Ctrl+Shift+R can be bound (egui emits no
            // Key event for a bare modifier, so the first Key event is the one).
            let hit = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => Some((*key, *modifiers)),
                    _ => None,
                })
            });
            if let Some((k, m)) = hit {
                if k != Key::Escape {
                    // Live immediately; persisted only on an explicit Save.
                    self.config.keybindings.set(action, Chord::from_modifiers(k, m));
                }
                self.rebinding = None;
            }
            return;
        }

        // Don't fire shortcuts while the user is typing in a text field (e.g. the
        // Compute pane's Save name, or the export name) — the keystrokes belong
        // to that widget, not the view. Buttons never *hold* focus here (see the
        // Tab handling below), so `wants_keyboard_input` is true only for genuine
        // text editors.
        if !ctx.wants_keyboard_input() {
            for action in Action::all() {
                if let Some(chord) = self.config.keybindings.chord_for(action) {
                    // Exact modifier match, so `R` and `Ctrl+R` stay distinct.
                    if ctx.input(|i| chord.pressed(i)) {
                        self.apply_action(action, ctx);
                    }
                }
            }
        }

        // Ctrl+Shift+C copies the reopen command line from anywhere, not just via
        // the "View command" window's button. A dedicated combo (rather than plain
        // Ctrl+C) so it never collides with copying selected text in a field.
        if ctx.input(|i| i.modifiers.command && i.modifiers.shift && i.key_pressed(Key::C)) {
            self.copy_view_command(ctx);
        }

        // Tab cycles the view mode (default `ToggleView`), but egui also treats
        // Tab as "focus the next widget". Left alone it parks on the first
        // toolbar button and stays there, which both traps every shortcut (a
        // focused widget makes `wants_keyboard_input` true) and turns further
        // Tabs into focus-hopping instead of view cycling. On any Tab, drop
        // widget focus and absorb egui's pending focus move onto a throwaway id
        // so nothing lands on (or lingers over) a button. Runs unconditionally
        // so tabbing out of a text field can't re-trap us on a button.
        if ctx.input(|i| i.key_pressed(Key::Tab)) {
            ctx.memory_mut(|m| {
                m.stop_text_input();
                m.interested_in_focus(Id::new("cim_tab_focus_sink"));
            });
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
