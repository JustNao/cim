# cim — Architecture & Reference

> **cim** ("Compare Images & Media") is a lossless side-by-side viewer for still
> images and multi-page TIFF sequences, built with `egui`/`eframe`. It targets
> pixel-accurate comparison: native bit depth is preserved, values are readable
> under the cursor, and the same view/timeline can sync across panes. Keep this
> doc in sync when subsystems change.

---

## 1. Build, run, test

- **Platform:** Windows dev; `eframe` (OpenGL via `glow`). Must also run over
  **VNC with no GPU**, so CPU cost / repaint volume / texture-upload size matter.
- **Build:** `cargo build`. `main.rs` sets `windows_subsystem = "windows"` only in
  release, so **debug is a console app** (CLI output visible). `[profile.dev]`
  uses `opt-level = 1` with deps at `opt-level = 3` so decode/render is usable.
- **Run:** `cargo run -- [FILES|SEQUENCES]...`.
- **Tests:** `cargo test` (inline in `media.rs`/`export.rs`/`cli.rs`; skip
  gracefully when fixtures or `ffmpeg` are absent). Fixtures in `examples/`.
- **CI:** `.github/workflows/build.yml` builds Windows + Linux (glibc 2.28 via
  Debian buster) release artifacts on `v*` tags.
- **Deps (`Cargo.toml`):** `eframe` 0.29, `image` 0.25, `tiff` 0.9, `rfd` 0.14,
  `serde`/`serde_json`, `directories` 5, `anyhow`, `cxx` 1 (C++ FFI — needs a host
  C++ compiler, see `INTEGRATION_CPP.md`). Export shells out to the **`ffmpeg` CLI**.

---

## 2. Source layout

```
src/
  main.rs        Entry point: parse CLI, then launch the eframe window.
  cli.rs         CLI: --help, shell completion, sequence-token expansion.
  media.rs       Data model: FrameData/Samples, Media (Still|TiffSeq|…),
                 SeqReader (persistent TIFF decoder), rendering, histograms, stats.
  imageproc.rs   cxx bridge to the proprietary C++ operators (LUT_ALPHA,
                 DETAILS_ENHANCED); C++ lives in cpp/, built by build.rs.
  decoder.rs     Background decode thread pool (per-sequence persistent readers).
  view.rs        ViewTransform: zoom/pan/fit math (screen <-> image space).
  settings.rs    Config, keybindings, ContrastMode/ToneOptions; JSON persist.
  export.rs      Export engine: ExportPlan composition + ffmpeg Encoder.
  app/           The CimApp type (egui App), split by concern:
    mod.rs       State struct, consts, new(), loading/reload, per-pane state
                 resolution, the update loop, shared free helpers.
    decode.rs    Decode plumbing, cache-budget eviction, texture prepare().
    input.rs     apply_action (keybindings), advance_playback, handle_input.
    canvas.rs    Central image area: grid/single/A-B, pan/zoom, reorder, header/
                 footer, per-pane popups (Transformations, stats, Compute).
    panels.rs    Toolbar, media manager, settings, view-command, bottom frame bar.
    export_ui.rs Export panel UI + building ExportPlan from live app state.
```

`CimApp`'s methods live in sibling `impl` blocks marked `pub(super)`; shared types
(`Mode`, `Pane`, consts) and free helpers live in `app/mod.rs`, reached via
`use super::*`.

---

## 3. Core data model (`media.rs`)

### `Samples` / `FrameData`
- `Samples` = `U8 | U16 | F32` — **native** interleaved samples, kept at native bit
  depth so the UI reports true values/histograms; 8-bit RGBA is derived on demand.
- `FrameData { size, channels:1|3|4, samples, bounds_full, bounds_clip }`.
  `new()`; `byte_len()` (cache budget); `render_rgba`/`render_into` (§7);
  `display_bounds(clip)` memoized in the two `OnceLock` cells; `pixel_string`,
  `histogram_display`, `region_stats`.
- **Boolean masks:** a frame from a **1-bit bilevel TIFF** is flagged `mask`
  (`new_mask`/`is_mask`). `render_into` paints false→black/true→white (bypassing
  tone), and `render_mask_rgba(rgb, alpha)` builds a tinted overlay buffer. Only
  TIFFs are masks; `Media::is_mask()` lets the UI list them as overlay sources.

### `Media` = `Still | TiffSeq | FileSeq | ConcatSeq`
Unified interface: `name`, `size`, `frame_count`, `hi_depth`; `resident(idx)` /
`insert(idx, frame)`; `decode_job(idx) -> Option<DecodeReq>` (how the pool decodes:
`Tiff { file, page, path }` seeks in a persistent reader keyed by `(pane id,
file)`, `File(path)` decodes a standalone still); lazy length `at_end()` /
`frontier_ended()`; cache budget `resident_bytes()` / `touch` / `evict` /
`resident_frames()`. `Media::still(name, frame)` wraps an in-memory frame (Compute).

- `Still` — one always-resident frame.
- The three sequence kinds share a private **`SeqCache`** (`cache:
  Vec<Option<Arc<FrameData>>>`, `last_used`, `resident_bytes`): `cache.len()` = the
  **known length** (independent of residency; eviction sets slots to `None` without
  changing it); `insert(idx==len)` grows length by one (a frontier probe).
- `TiffSeq` — one multi-page TIFF; length discovered lazily (§4).
- `FileSeq` — a numbered **still** run (one file per frame) from a compact CLI
  token; length known up front → always `at_end`. Frames decode via
  `media::decode_file`.
- `ConcatSeq` — a numbered run of **multi-page TIFFs** as **one timeline** (rolls
  into the next file when a file's pages run out). `map[global] = (file, page)`;
  the frontier probe walks `(disc_file, disc_page)`; `frontier_ended` rolls to the
  next file or, past the last, sets `at_end`. `concat_layout()` exposes it to export.

### `SeqReader` — persistent per-sequence decoder
`open(path)` holds one `tiff::Decoder`; `decode(idx)` returns `Ok(None)` past the
last page. The tiff crate caches IFD offsets only *within a Decoder*, so a fresh
decoder per call makes `seek_to_image(k)` O(k) and a sweep O(N²); keeping one
reader warm avoids that. `load(path)` dispatches by extension (TIFF page-0 vs the
`image` crate).

---

## 4. Lazy sequence-length discovery

Opening a TIFF **never walks all IFDs** (long sequences would stall; pages may
differ in resolution). A fresh `TiffSeq` starts at length 1, `at_end = false`;
decoding past the end returns `Ok(None)` → `frontier_ended()`, and `insert(idx ==
len)` grows length by one.

- `ensure_lookahead` keeps **one page beyond the shown frame** discovered while
  browsing; playback **holds at the frontier** rather than wrapping until `at_end`.
- Headers show `N+` while more frames may exist.
- **Seeking past the frontier** (`--frame N` at launch): `pending_seek` holds the
  target; `drive_seek` rides the frontier probing one page/update until the length
  passes `N` (or the real end), then snaps. Any manual navigation clears it.
- **Per-frame resolution:** `disp_size(i)` uses the resident frame's own size
  (page-0 fallback) so drawing/readout don't stretch or go out of bounds.
- **`ConcatSeq`** reuses all of this: a frontier miss rolls to the next file's
  page 0, so the run discovers as one seamless length (∑ page counts) with no
  concat-specific code in `drive_seek`/lookahead/playback.

---

## 5. Background decode pool (`decoder.rs`)

- `BackgroundDecoder::new(threads)` (`available_parallelism().clamp(2,6)`) shares
  one `mpsc` job queue behind a `Mutex` (locked only for the hand-off).
- **Jobs addressed by stable pane `id`**, not Vec index, so results land after
  reorder/close.
- **Persistent readers:** `readers: HashMap<(pane id, file), Arc<Mutex<SeqReader>>>`.
  A `Tiff` job locks the map to get/open the file's reader, then locks the reader
  to decode. Different files decode in parallel; pages of one file serialise.
  `forget(id)` drops all of a pane's readers. A `File` job has no persistent reader.
- `request` enqueues; `drain()` collects finished `Done` non-blocking each update.
  `Done.result: Result<Option<Arc<FrameData>>>` — `Ok(Some)` frame, `Ok(None)`
  past-end probe, `Err` failure.
- App side (`app/decode.rs`): `inflight: HashSet<(id, frame)>` dedupes; `pump_decoder`
  drains (insert + `touch`, or `frontier_ended`, or set pane `error`).

---

## 6. Cache memory budget / LRU (`app/decode.rs::enforce_cache_budget`)

Frames are held at native bit depth and never freed by decode alone. Guard:

- `CimApp::cache_budget_bytes()` = `config.cache_budget_mb` (**default 1.5 GiB**,
  adjustable via the **Frame cache** slider in Settings, 128 MiB–32 GiB).
- `clock` increments each update; frames are `touch`ed on decode and on display →
  LRU recency. When `resident_bytes()` exceeds budget, evict the oldest frames that
  are **not currently shown** (each pane's `frame_disp(i)` is protected) until under.
- An over-budget **"Load all"** is **stopped** with a status note. Stills never
  evict. Export decodes through its own `SeqReader`, so it's unaffected.

---

## 7. Rendering pipeline (native samples → texture)

`app/decode.rs::prepare(ctx, idx)` returns `(Option<TextureId>, loading)`: render +
upload **only when stale** (`tex.shown != f`), else reuse; if not resident, queue a
decode and keep showing the last texture with a spinner. Pipeline: bounds →
`render_into(lo, hi, &mut render_scratch)` (a reused buffer) →
`ColorImage::from_rgba_unmultiplied` → texture `set`/`load`.

`render_into` (`media.rs`): **U8/U16** build a value-keyed **LUT** (≤ 64 Ki) once
per frame then table-look-up per pixel; **F32** maps arithmetically. Mono replicates
grey across R/G/B; alpha = 255.

**Display bounds:** full range for integers; data extent for floats; with `clip`, a
per-tail percentile stretch (default **0.01%**). Bounds are content-invariant per
frame, memoized in `FrameData`'s `OnceLock` cells.

**Tone modes & C++ post-processing.** Each pane picks a `ContrastMode` plus per-mode
`ToneOptions` (edited in the Transformations popup, §9):
- **Linear + Clip** — full-range map with a per-tail percentile clip
  (`clip_bounds(percent)`, editable `ToneOptions.clip`; the default for **>8-bit**).
- **LUT_ALPHA** — full-range map then the proprietary operator, with a Rust-side
  **Blend** back toward the linear image (`ToneOptions.lut_alpha.blend`,
  `blend_rgba`). More knobs slot into `LutAlphaOptions` + `draw_tone_options`.
- **Linear** — plain full-range map, no clip.

Plus a per-pane **DETAILS_ENHANCED** toggle. `render_into` produces the 8-bit RGBA,
then the `imageproc.rs` operators (`lut_alpha`, `details_enhanced`) transform it in
place. Both run in `prepare` (live) and `export.rs::ensure_frame` (export) so
exports match the screen. (Export uses the default clip percentile; `ToneOptions`
are live-view only.) See `INTEGRATION_CPP.md` for the C++ contract.

**Region-driven tone (`Pane.region_tone`).** When pinned (§9), a pane's linear
bounds come from the shared stats region via `region_display_bounds` — the region's
min/max (Linear) or its per-tail-percentile clip (Linear+Clip). Pixels outside the
region that exceed these bounds are clamped (the LUT saturates). LUT_ALPHA still
runs over the whole image. Recomputed on each texture rebuild; replicates to all
panes.

**Magnification filter** (`tex_options(idx)`): follows the pane's `ToneOptions.interp`
(Nearest/Bilinear, so it rides the tone-sync); minification Linear.

---

## 8. View / sync model

`ViewTransform` (`view.rs`): `{ zoom, center (image-space), needs_fit }` with `fit`,
`actual_size`, `img_to_screen`/`screen_to_img`, `image_rect`, `zoom_at`, `pan`; zoom
clamps to `[1e-4, 512]`.

Each `Pane` has its own `transform`/`frame` plus `sync_spatial`/`sync_temporal`
flags; `CimApp` holds `shared_view`/`shared_frame`. `view_ref/view_mut(i)` and
`frame_disp(i)` return the shared state when synced (a shorter sequence **holds on
its last frame**), else the pane's own. Toggling sync **off** snapshots the shared
state into the pane so it doesn't jump. (Transformations sync is §9.)

`timeline_len()` = the **control** pane's known length (drives the loop). The control
pane is **separate from `current`** (the focused pane for Single/keyboard/tint), so
viewing a still doesn't hijack playback; `ensure_control` keeps it on a sequence, and
the manager's **Control** selector chooses which.

Playback loops over a **window** `loop_bounds(len)` — a user sub-range (`loop_range`,
set by dragging the scrubber brackets; `None` = whole sequence). A full range with an
undiscovered end holds at the frontier rather than wrapping; a sub-range wraps/stops
immediately. `draw_scrubber` shades resident frames (contiguous runs merged), dims
outside the window, and draws the brackets. `advance_playback` accumulates
`stable_dt`, steps at `fps`, and advances unsynced panes independently.

---

## 9. Modes & central drawing (`app/canvas.rs`)

`Mode = Grid | Single | Ab`. `draw_central` dispatches: **Grid** lays out
`grid_cells` and `draw_pane` per cell (ctrl-drag reorders via `drag_src` +
`finish_reorder`); **Single** fills with `current`; **A/B wipe** (`draw_ab`) splits
`slot_a`/`slot_b` at `ab_split` (draggable divider), pan/zoom acting on the side
under the cursor.

Per pane: `image_area(cell)` (between header and `FOOTER_H`), `draw_header` (buttons,
index, name, `frame/known(+)`, `in mem`, sync markers, close ×), `draw_footer`
(`h×w`, cursor `x y`, native value). Borders show **only during ctrl-drag**; focus is
the header tint. While `selecting_region` (export crop), pane pan/zoom is disabled.

The header is **one or two rows** (`header_rows`/`header_h_for`, feeding
`image_area`): when a cell is too narrow to fit the two buttons + a little title, the
**Compute** button drops onto a second row under **Transformations** and the image
area shrinks to match.

**Transformations popup** (`draw_options_popup`). The header's **Transformations**
button (left, away from ×) toggles `Pane.show_opts`, opening a foreground `Area`
under the header with: the tone `ContrastMode` + its mode-specific options
(`draw_tone_options` — **the single place to add a tone knob**: grow the mode's
`ToneOptions` sub-struct, add a row, read it in `prepare`/`tex_options`), the Details
toggle, the per-pane magnification **Interp**, the mask **Overlay** picker, and this
pane's **Histogram** (`ensure_pane_histogram` + `draw_histogram`, cached per pane).
Edits invalidate the texture. `Action::ToggleVis` (default `V`) toggles it for the
focused pane.

**Transformations sync (`Pane.sync_tone`, default on).** Like the Pos/Time syncs, a
pane can follow the shared set (`shared_contrast`/`shared_tone`/`shared_details`/
`shared_overlay`), toggled by the **Transf** checkbox in the manager's Sync column.
`contrast_of`/`tone_of`/`details_of`/`overlay_of` return the effective value and are
read by `prepare`/`prepare_overlay`/`export_pane`/`view_command`; editing a synced
pane's popup writes the shared set and `invalidate_synced_tone` refreshes every synced
pane. `set_sync_tone(false)` snapshots the shared values in so nothing jumps. The
first opened media seeds the shared set (`add_pane`); a replayed `--tone`/`--detail`
is per-pane, so `apply_view_state` unsyncs the panes it sets.

**Mask overlays.** A pane may carry an `OverlaySpec { src_id, color, opacity }` — a
boolean-mask media tinted over it. The spec is **config only** so it rides the
Transformations sync; the tinted texture is cached separately per pane in
`overlay_tex`. `prepare_overlay` builds it from the mask's shown frame (decoded on
demand, so it works even when the mask pane isn't drawn) and returns `None` on a mask
pane itself; `draw_pane` paints it at the base image's rect (1:1). Configured in the
popup's **Overlay** row; cleared when its source mask closes. Expected to match the
target's dimensions.

**Statistics region (right-drag).** A **right-button drag** selects a rectangle,
stored in **image space** (`stats_region`) so the region and each pane's own stats
**replicate across panes**. `region_overlay_for_pane` (from `draw_pane` and both A/B
sides) runs the selection (`region_input`, secondary-button edge detection), draws the
rubber band, else the outline plus a **stats panel**: a mini histogram
(`draw_region_hist`, min/max at its ends) and mean/std/count (`region_stats`, cached
per pane keyed on `(frame, stats_gen)`). A near-zero drag / plain right-click clears
it. **"compute LUT from region"** pins every pane's tone to the region (§7); a **–**
corner button collapses the panel to a small **"σ stats"** re-open button. Pan/reorder
are **primary-button-only** so the right-drag isn't stolen.

**Compute panes.** A *generated* pane whose image is derived from other panes.
Two-phase flow: the header's **Compute** button opens a floating **`ComputeDraft`**
panel (`draw_compute_draft`) *where it was clicked* — mode + source pickers with no
pane yet; its **Compute** button sets `pending_compute_create`, and the deferred
`open_compute(draft)` realizes it into a pane, after which the controls live on that
pane (`draw_compute_ui`, `Refresh` + **Auto refresh**) and the draft is dropped.
`Pane.compute` holds the `kind`, source id(s), and the auto-refresh flag.
`media::Reduce` modes:
- **Mean | Std** — `recompute_pane` → `compute_reduce` gathers **one** source's
  **resident** frames and calls `media::reduce_frames` (per-pixel/-channel, `f64`
  accumulation → `f32`).
- **Diff** — `compute_diff` takes **two** sources' *current* frames
  (`frame_disp`, both must be resident) and calls `media::diff_frames` (signed
  `A − B`, float). Sources may be stills; reductions need ≥2 frames
  (`compute_sources`).

Results become an `f32` `Media::still` (default tone Linear+Clip). **Auto refresh**
recomputes when inputs change: `refresh_auto_compute` compares `compute_sig` (shown
frames for Diff, source resident-count for reductions) against `Compute.last_sig`
each update. `Source::Computed` makes the manager's ⟳ recompute; an inline **Save**
(`media::save_frame`, `.tif` **32-bit float** or `.png`/`.jpg` 8-bit view, relative
to the working dir). Skipped by `view_command`.

---

## 10. Export (`export.rs` + `app/export_ui.rs`)

The app builds a self-contained **`ExportPlan`** (snapshot of layout, views, clip,
sources, frame range) decoupled from live state, composites each output frame on the
CPU, and pipes raw RGBA to the `ffmpeg` CLI (H.264, libx264).

- `ExportPane` holds a snapshot view/clip/source plus its **own `SeqReader`** and a
  1-frame decode+render cache. A pane's **mask overlay** is snapshotted too
  (`set_overlay` + `blend_overlay`), so overlays appear in the video. `ExportSource =
  Still | Seq { path } | Files { paths } | Concat { files, map }`.
- `ExportLayout = Grid | Single | Ab`. `ExportPlan.compose(t)` maps each output pixel
  back through the pane's view (export forces **nearest**). `start` offsets so output
  frame `t` = timeline `start+t`.
- **Region crop** is chosen in image space ("Select…" forces Single;
  `screen_rect_to_image` on release), applied to every pane as a cell of exactly the
  crop's pixel size.
- **Frame range:** "all", else inclusive `from/to`; **"Use loop range"** adopts the
  playback window. A warning + "Load all" appears when a length isn't discovered yet.
- Output filename typed in the panel, written to the **cwd**. `Encoder` streams one
  frame per update; `export_tick` drives it.

---

## 11. CLI (`cli.rs`) & entry (`main.rs`)

`main` → `cli::parse` → `Cli::Run { paths, view }` or `Cli::Exit(code)`.

- `-h/--help`, `-V/--version`.
- **View-state flags** (`ViewState`, 0-based, optional): `--mode`, `--cols`,
  `--zoom`, `--center X,Y`, `--frame`, `--pane`, `--ab A,B,SPLIT`, `--tone` (per-pane
  `linear|linearclip|lutalpha`), `--detail` (per-pane `1`/`0`), `--loop LO,HI`.
  Generated by the in-app "View cmd" window (`view_command`), applied after startup
  files load (`apply_view_state`). Only present flags override defaults; a restored
  `--zoom`/`--center` clears `needs_fit`. Only the *shared* view is captured.
- Positional args accept a **compact numbered-sequence token**
  `PREFIX%0Nd,START,END.EXT`, expanded at launch. A bare path → `Single`; a token
  ≥2 files → `Sequence` opening as **one** pane (`.tif` run → `ConcatSeq`, else
  `FileSeq`). `token` is kept on the pane's `Source` so reload/round-trip work.
  Drag-and-drop / the file dialog only produce `Single`s.
- `--complete <word>` lists loadable completions (collapses numbered runs into the
  token); `--completions <bash|powershell>` prints a completer. `LOADABLE_EXTS =
  [tif,tiff,png,jpg,jpeg,bmp,webp]` is shared by the dialog and the filter.

---

## 12. Settings & persistence (`settings.rs`)

`Config { max_columns, vis: { interp }, ui_scale, cache_budget_mb, keybindings }`,
saved as JSON via `ProjectDirs("dev","cim","cim")`. Loaded on start, saved on
exit / explicit save.

`Action` = all bindable actions (view toggles, next/prev media & frame, fit/actual/
zoom, load all, open, toggle panels, play/pause, `SelectMedia(0..12)`).
`Keybindings` is a `BTreeMap<action_id, key_name>` with unique bindings. New default
bindings do **not** retroactively apply to a saved config (shows `—` until rebound).
`handle_input` skips the shortcut scan while `ctx.wants_keyboard_input()` (a text
field has focus), so typing doesn't trigger views.

---

## 13. The update loop (`app/mod.rs::update`)

Each frame: apply `ui_scale`; `clock += 1`; `pump_decoder` → `handle_input` →
`advance_playback` → `drive_seek`; `drive_eager` → `ensure_lookahead` →
`poll_decoding_all` → `enforce_cache_budget`; clamp `shared_frame`; draw toolbar,
bottom frame bar (shown whenever **any** media is a sequence), central panel, windows
(manager/export/settings/view-command), error popup; apply deferred actions;
`export_tick`; `request_repaint()` while playing/decoding/exporting.

Deferred actions (`pending_remove`, `pending_reload(_all)`, `pending_compute`,
`error_popup`) avoid mutating panes mid-draw.

---

## 14. Invariants & gotchas

- **Pane `id` is stable** across reorder/close and keys decode results + persistent
  readers. Vec index is *not* — never key by it.
- `cache.len()` is the **known length**, not residency; eviction keeps length.
  `insert` only grows it at `idx == len` (contiguous discovery).
- **Protected frames:** each pane's `frame_disp(i)` is never evicted.
- `disp_size(i)` (not `media.size()`) must be used for drawing/readout, since pages
  can vary in resolution.
- Files are opened **read-only with shared access**; `forget(id)` on reload picks up
  new contents.
- Export decodes independently of the display cache; export length = the **known**
  timeline at build time (press "Load all" first for a full export).

---

## 15. Performance notes (VNC / no GPU)

Done: lazy length, persistent readers, bounded LRU cache, LUT render + memoized bounds
+ reused buffer, per-pane histogram cache. Remaining candidates: **repaint
throttling** while waiting on decodes (`request_repaint_after`); **threaded export**
(compose+encode off the UI thread); minor per-frame allocations (`Action::all()`,
`grid_cells`) and display-downscale for large images in tiny cells.

---

## 16. Testing

Inline `#[cfg(test)]` (skip when fixtures/ffmpeg absent): `cli` token
expansion/grouping; `media` lazy length, eviction, **LUT render matches the float
reference** bit-for-bit, region stats + save round-trip; `export` full compose→ffmpeg
encode + **pixel-exact region crop**.

---

## 17. Conventions

- **Commits:** small, one concern; imperative summary + a short *why*. Committed
  directly to `main`.
- **Build target:** Windows, debug, during development.
- **Style:** match surrounding code (comment density, naming, `pub(super)` methods,
  free helpers in `app/mod.rs`).
- **Future media:** video (mp4/avi) slots in as another `Media` variant behind the
  same interface.
