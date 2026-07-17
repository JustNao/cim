//! Keyboard actions, playback advancement, and file drops.

use super::*;

impl CimApp {
    /// Build a button's hover tooltip prefixed with the current shortcut for the
    /// action the button replicates: `"Ctrl+R. <desc>"`, or just `"Ctrl+R"` when
    /// the button had no description. Reads the live keybinding, so a rebind is
    /// reflected immediately; an unbound action falls back to `desc` unchanged.
    pub(super) fn hover_for(&self, action: Action, desc: &str) -> String {
        match self.config.keybindings.chord_for(action) {
            Some(c) if desc.is_empty() => c.name(),
            Some(c) => format!("{}. {}", c.name(), desc),
            None => desc.to_string(),
        }
    }

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
                // Step within the active loop window (the same one playback
                // obeys), not the whole timeline.
                let tl = self.timeline_len();
                let (lo, hi) = self.loop_bounds(tl);
                let full = self.playback.loop_range.is_none();
                let f = self.shared_frame;
                if lo <= f && f < hi {
                    self.shared_frame += 1;
                } else if !full {
                    self.shared_frame = lo; // sub-range: wrap at its edges
                } else if self.current_at_end() {
                    self.shared_frame = lo; // full range: wrap once length is known
                }
                // else hold at the frontier; lookahead extends it shortly
            }
            Action::PrevFrame => {
                self.pending_seek = None; // manual step cancels an automatic seek
                self.playback.prefetch = None; // …and any in-flight playback step
                let tl = self.timeline_len();
                let (lo, hi) = self.loop_bounds(tl);
                let full = self.playback.loop_range.is_none();
                let f = self.shared_frame;
                if lo < f && f <= hi {
                    self.shared_frame -= 1;
                } else if !full {
                    self.shared_frame = hi; // sub-range: wrap at its edges
                } else if self.current_at_end() {
                    self.shared_frame = hi; // full range: wrap once length is known
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
            Action::OpenCompute => self.deferred.push(Deferred::CreateCompute),
            Action::PlayPause => self.playback.playing = !self.playback.playing,
            Action::ReloadMedia if n > 0 => {
                self.deferred.push(Deferred::Reload(self.current.min(n - 1)))
            }
            Action::ReloadAll => self.deferred.push(Deferred::ReloadAll),
            Action::HideMedia if n > 0 => {
                self.panes[self.current.min(n - 1)].visible = false;
                self.reselect_if_hidden();
            }
            Action::ToggleChrome => self.show_chrome = !self.show_chrome,
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
            self.playback.last_tick = None; // resume starts timing afresh
            return;
        }
        // Wall-clock dt (`i.time` is an accurate absolute clock). NOT
        // `i.stable_dt`: egui substitutes a fixed `predicted_dt` (1/60 s) for the
        // real elapsed time on any frame that wasn't preceded by an *immediate*
        // repaint request — which is every paced `request_repaint_after` wake-up,
        // i.e. all of playback when the user isn't providing input. With that,
        // a 25 fps setting was credited only ~17 ms per ~40 ms wake and played at
        // a fraction of the requested rate (input events masked it by making the
        // dts real again — moving the mouse visibly sped playback up).
        let now = ctx.input(|i| i.time);
        let dt = self
            .playback
            .last_tick
            .map_or(0.0, |t| ((now - t) as f32).clamp(0.0, 0.25));
        self.playback.last_tick = Some(now);
        let step = 1.0 / self.playback.fps.max(0.1);
        // Playback is render-gated: a step is pre-rendered into `play_prefetch`,
        // and the timeline only advances once every on-screen pane has that frame
        // ready (`refresh_textures` clears it on commit). While one is in flight,
        // wait — so a slow operator paces playback instead of the counter racing
        // ahead of the image. Keep accumulating real time so the gate's own
        // latency doesn't stretch the frame interval (the next frame is due `step`
        // after this one *fired*, not after it committed), but cap the backlog at
        // one step so a slow operator can't burst several frames when it lands.
        if self.playback.prefetch.is_some() {
            self.playback.accum = (self.playback.accum + dt).min(step);
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
        self.playback.accum += dt;
        if self.playback.accum < step {
            return;
        }
        // Carry the overshoot into the next interval (else per-frame lateness —
        // e.g. the wake landing a few ms past due — compounds into a rate error),
        // but cap the surplus at one step so a stall can't burst several frames.
        self.playback.accum = (self.playback.accum - step).min(step);

        // Fast-forward: advance by `ff` frames per step, skimming the ones in
        // between (they're never staged, and the frontier is discovered by header
        // only — see `ensure_lookahead`). `1` = play every frame.
        let ff = self.playback.fast_forward.max(1);
        let f = self.shared_frame;
        let next = if f < lo {
            Some(lo) // jump into the window
        } else if f + ff <= hi {
            Some(f + ff) // a full stride fits inside the discovered window
        } else if f < hi && (!full || at_end) {
            // The stride would overshoot, but the window end is a *real* boundary (a
            // sub-range, or the true end of a fully-discovered sequence): take the
            // final short stride onto it.
            Some(hi)
        } else if full && !at_end {
            // The stride would run past the still-undiscovered frontier. Hold rather
            // than clamp onto it — clamping would land on (and decode) every frontier
            // frame one at a time and defeat the stride. `ensure_lookahead` keeps
            // probing the frontier `ff` ahead (headers only), so once `f + ff` is
            // discovered the full-stride branch above fires.
            None
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
