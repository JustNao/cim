//! The windowless export: `cim … --export OUT` (the export panel's **Command**
//! button writes such a line). No window and no GPU — the app is built on a bare
//! `egui::Context` (`CimApp::with_context`) and driven pass by pass through the
//! very same `CimApp::frame` the window runs, so opening, length discovery,
//! seeking, tone, Compute panes and finally `start_export` are all the live
//! code, not a re-implementation of it (§10's parity rule).
//!
//! The view is relative to the image area it was captured in (zoom/centre map
//! through its centre, and an export without a crop takes the image content
//! on screen), so the command carries that area as `--canvas W,H,PPP`; here it
//! is the whole (virtual) screen, with the chrome hidden.

use std::time::{Duration, Instant};

use super::*;

/// Quiet passes in a row before the state counts as settled — one pass can
/// look idle between a decode landing and the next one being queued.
const QUIET_PASSES: u32 = 3;
/// Give up waiting for the state to settle after this long and export anyway.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(120);
/// The image area assumed when a command carries no `--canvas`.
const DEFAULT_CANVAS: (f32, f32, f32) = (1920.0, 1080.0, 1.0);

/// Run the export the command line describes and return the process exit code:
/// 0 when the file was written, 1 when the export failed, 2 for a bad command.
pub fn run(config: Config, inputs: Vec<cli::Input>, view: cli::ViewState) -> i32 {
    let Some(out) = view.export.output.clone() else {
        return 2;
    };
    if inputs.is_empty() {
        eprintln!("cim: {}", t!("headless.no_inputs"));
        return 2;
    }
    let screen = Screen::new(
        view.export.canvas.unwrap_or(DEFAULT_CANVAS),
        config.ui_scale,
    );
    let mut app = CimApp::with_context(&screen.ctx, config, Preload::start(inputs), view);
    app.headless = true;
    // The image area is then the whole screen, as `--canvas` describes it.
    app.show_chrome = false;
    if !screen.prepare(&mut app, true) {
        return 1;
    }

    app.export.name = out;
    screen.pass(&mut app, |app, ctx| app.start_export(ctx));
    if app.export.run.is_none() {
        // A still is written synchronously; a failure also lands here, with
        // its reason in the status.
        return report(&app);
    }
    eprintln!("cim: {}", app.export.status);
    let mut last_report = Instant::now();
    loop {
        // The work is on the export thread; a pass only polls it.
        std::thread::sleep(Duration::from_millis(50));
        screen.pass(&mut app, |_, _| {});
        if app.export.run.is_none() {
            return report(&app);
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            if let Some((done, total)) = app.export_progress() {
                eprintln!("cim: {done}/{total}");
            }
        }
    }
}

/// Print the export's outcome and turn it into the exit code.
fn report(app: &CimApp) -> i32 {
    if !app.export.status.is_empty() {
        eprintln!("cim: {}", app.export.status);
    }
    if app.export.ok {
        0
    } else {
        1
    }
}

/// The virtual screen a windowless app runs on: a bare context and the size /
/// pixel density of the image area the command was captured in.
struct Screen {
    ctx: egui::Context,
    t0: Instant,
    size: Vec2,
    native_ppp: f32,
}

impl Screen {
    /// `canvas` is `--canvas`'s `(W, H, PPP)`. `frame` applies the UI scale as
    /// egui's zoom factor on top of the native pixels per point, so it is
    /// divided back out here to land on the captured `PPP`.
    fn new((w, h, ppp): (f32, f32, f32), ui_scale: f32) -> Self {
        Self {
            ctx: egui::Context::default(),
            t0: Instant::now(),
            size: Vec2::new(w, h),
            native_ppp: ppp / ui_scale.clamp(0.5, 3.0),
        }
    }

    /// Run one whole app update, then `then` inside the same pass (where the
    /// context has its fonts — the export's labels are rasterized from them).
    /// The pass's output (shapes, texture uploads) has nowhere to go: dropped.
    fn pass(&self, app: &mut CimApp, then: impl FnOnce(&mut CimApp, &egui::Context)) {
        let mut raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.size)),
            time: Some(self.t0.elapsed().as_secs_f64()),
            ..Default::default()
        };
        raw.viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point = Some(self.native_ppp);
        // egui may rerun the closure within one pass (a discarded pass);
        // `then` runs once, on the first.
        let mut then = Some(then);
        let _ = self.ctx.run(raw, |ctx| {
            app.frame(ctx);
            if let Some(then) = then.take() {
                then(app, ctx);
            }
        });
    }

    /// Drive `app` until it is ready to export: its inputs open (the command's
    /// view and export settings apply as they land), the export range's frames
    /// discovered, and the live state the export snapshots settled. `false`
    /// when nothing opened. `verbose` reports the steps on stderr.
    fn prepare(&self, app: &mut CimApp, verbose: bool) -> bool {
        let pause = || std::thread::sleep(Duration::from_millis(2));
        // Open.
        loop {
            self.pass(app, |_, _| {});
            if app.opening.is_empty() && app.pending_open.is_none() {
                break;
            }
            pause();
        }
        if let Some(e) = app.error_popup.take() {
            if verbose {
                eprintln!("cim: {e}");
            }
        }
        if app.panes.is_empty() {
            return false;
        }
        // The export reads `export.show`: the crop's Local LUT applies while
        // the panel is open.
        app.export.show = true;

        // Discover the lengths the range needs.
        let mut requested = false;
        loop {
            self.pass(app, |_, _| {});
            if !app.export_range_incomplete() {
                break;
            }
            if !app.decoding_all {
                if requested {
                    // Discovery ran to its end and the range still isn't all
                    // there: export what exists.
                    break;
                }
                if verbose {
                    eprintln!("cim: {}", t!("headless.discovering"));
                }
                app.load_offsets();
                requested = true;
            }
            pause();
        }

        // Settle.
        let from = Instant::now();
        let mut quiet = 0;
        while quiet < QUIET_PASSES {
            self.pass(app, |_, _| {});
            quiet = if app.headless_settled() { quiet + 1 } else { 0 };
            if from.elapsed() > SETTLE_TIMEOUT {
                if verbose {
                    eprintln!("cim: {}", t!("headless.settle_timeout"));
                }
                break;
            }
            pause();
        }
        true
    }
}

impl CimApp {
    /// Whether everything the export snapshots from live state is in place: no
    /// seek still walking discovery, no background work in flight, and every
    /// exported pane's shown frame resident (a Compute reduction, global
    /// statistics and the 16-bit TIFF check all read it).
    fn headless_settled(&self) -> bool {
        self.pending_seek.is_none()
            && !self.work.busy()
            && self.pending_recompute.is_none()
            // The DAI rotation is part of the view being exported.
            && self.panes.iter().all(|p| p.dai_req.is_none())
            && (0..self.panes.len()).all(|i| !self.catching_up(i))
            && self
                .export_participants()
                .into_iter()
                .all(|i| self.panes[i].media.resident(self.frame_disp(i)).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    /// The composited still `app`'s export would write, built inside a pass.
    fn still(screen: &Screen, app: &mut CimApp) -> (usize, usize, Vec<u8>) {
        let mut out = None;
        screen.pass(app, |app, ctx| {
            let mut plan = app.build_export_plan(ctx).expect("an export plan");
            plan.start = app.shared_frame;
            plan.total = 1;
            out = Some((plan.out_w, plan.out_h, plan.compose(0)));
        });
        out.unwrap()
    }

    /// The export panel's **Command** replays to the same pixels: a session
    /// driven like the window (chrome shown, its own config) composes an
    /// export, and its command — run windowless, under a *different* saved
    /// configuration — composes it again, byte for byte. This is the whole
    /// promise of the button (§10's parity rule, across a process boundary).
    #[test]
    fn export_command_replays_to_the_same_pixels() {
        let dir = testutil::fixture_dir("headless_replay");
        let tif = dir.join("stack.tif");
        testutil::write_multipage_tiff_u16(&tif, &[[96, 64], [96, 64], [96, 64]]);
        let pngs = testutil::write_png_run(&dir, 1, 96, 64);
        let inputs = vec![
            cli::Input::Single(tif.clone()),
            cli::Input::Single(pngs[0].clone()),
        ];

        // The live session: a 900×600 area with its chrome, at 1.25 ppp.
        let live = Screen::new((900.0, 600.0, 1.25), 1.0);
        let mut a = CimApp::with_context(
            &live.ctx,
            Config::default(),
            Preload::start(inputs),
            cli::ViewState::default(),
        );
        a.headless = true; // no config autosave from a test
        assert!(live.prepare(&mut a, false));
        // Move off the defaults: zoom/pan, a rotation, a crop, a frame, labels.
        a.shared_view.zoom = 2.37;
        a.shared_view.center = Vec2::new(41.3, 29.7);
        a.shared_view.needs_fit = false;
        a.panes[1].sync_geometry = false;
        a.panes[1].rotation = 17.0;
        a.shared_frame = 2;
        a.pending_seek = Some(2);
        a.export.mode = Mode::Grid;
        a.export.out_height = 300;
        a.config.max_columns = 2;
        a.config.export.labels_on = true;
        a.config.export.label_style.anchor = crate::export::LabelAnchor::TopRight;
        assert!(live.prepare(&mut a, false));
        a.export.name = dir.join("out.png").display().to_string();
        let expected = still(&live, &mut a);

        // Crop it too (the other geometry path), and compare both.
        let cropped = |a: &mut CimApp| {
            a.export.region = Some(Rect::from_min_size(
                Pos2::new(10.0, 7.0),
                Vec2::new(50.0, 31.0),
            ));
            a.export.local = true;
            let _ = live.prepare(a, false);
            still(&live, a)
        };
        let cmd_full = a.export_command().expect("a command");
        let expected_crop = cropped(&mut a);
        let cmd_crop = a.export_command().expect("a command");

        for (cmd, want) in [(cmd_full, expected), (cmd_crop, expected_crop)] {
            let args = cli::split_command(&cmd).into_iter().skip(1).collect();
            let cli::Cli::Run { inputs, view } = cli::parse(args) else {
                panic!("the command parses");
            };
            assert!(view.export.output.is_some(), "{cmd}");
            // A different saved configuration on the replaying machine.
            let mut config = Config {
                max_columns: 5,
                ..Config::default()
            };
            config.export.labels_on = false;
            config.export.label_style.size_px = 40.0;
            let replay = Screen::new(view.export.canvas.unwrap(), config.ui_scale);
            let mut b = CimApp::with_context(&replay.ctx, config, Preload::start(inputs), view);
            b.headless = true;
            b.show_chrome = false;
            assert!(replay.prepare(&mut b, false));
            let got = still(&replay, &mut b);
            assert_eq!((got.0, got.1), (want.0, want.1), "{cmd}");
            assert!(got.2 == want.2, "replayed pixels differ: {cmd}");
        }
    }
}
