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
- **Deps (`Cargo.toml`):** `eframe` 0.29, `image` 0.25, `tiff` 0.11, `rfd` 0.14,
  `serde`/`serde_json`, `directories` 5, `anyhow`, `libloading` 0.8 (runtime load
  of the optional proprietary C++ operators — **no** C++ compiler needed to build
  cim; see `INTEGRATION_CPP.md`). Export shells out to the **`ffmpeg` CLI**.
- **Embedded assets** (`assets/`, baked in via `include_bytes!`): `icon.png` (window
  icon) and `cimicons.ttf` (a Braille-block subset of DejaVu Sans, registered in
  `new` as a **fallback** font so glyphs the bundled faces lack — e.g. the `⠿`
  drag-handle grip — render instead of tofu).

---

## 2. Source layout

```
src/
  main.rs        Entry point: parse CLI, then launch the eframe window (maximized).
  cli.rs         CLI: --help, shell completion, sequence-token expansion.
  media.rs       Data model: FrameData/Samples, Media (Still|TiffSeq|…),
                 SeqReader (persistent TIFF decoder), rendering, histograms, stats.
  imageproc.rs   Runtime loader (libloading) for the proprietary C++ operators
                 (LUT_ALPHA, DETAILS_ENHANCED); C++ in cpp/ is built separately
                 into two .so, loaded by hard-coded name. PaneOps owns a pane's
                 per-operator instances (create/apply/destroy; 16-bit only).
  decoder.rs     Background decode thread pool (per-sequence persistent readers).
  renderer.rs    Off-thread tone-render pool: builds the display RGBA (LUT render
                 + LUT_ALPHA / details) for heavy panes so the UI never blocks.
  view.rs        ViewTransform: zoom/pan/fit math (screen <-> image space).
  settings.rs    Config, keybindings, ContrastMode/ToneOptions; JSON persist.
  export.rs      Export engine: ExportPlan composition + ffmpeg Encoder.
  app/           The CimApp type (egui App), split by concern:
    mod.rs       State struct, consts, new() (style, embedded fallback font,
                 loading/reload), per-pane state resolution, update loop, helpers.
    decode.rs    Decode plumbing, cache-budget eviction, lock-step texture
                 staging/commit (refresh_textures/stage/pane_texture).
    input.rs     apply_action (keybindings), advance_playback, handle_input.
    canvas.rs    Central image area: grid/single/A-B, pan/zoom, reorder, header/
                 footer, per-pane popups (Transformations, stats, Compute).
    panels.rs    Toolbar, media manager (drag the ⠿ handle to reorder rows via
                 `drop_target` + `remap_move`), settings, view-command, frame bar.
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
  tone), and `render_mask_rgba(rgb, alpha)` builds a tinted overlay buffer; any
  non-mask single-channel frame instead tints by intensity (`render_intensity_rgba`)
  when used as an overlay (§9). Only TIFFs are masks; any single-channel media can
  be an overlay source.
  Mask truth is the **stored sample bit** (what the author set — e.g. `numpy`
  `True`), *not* the pixel's black/white look: `mask_bits` reads
  `PhotometricInterpretation` and un-inverts WhiteIsZero pages (the TIFF default,
  and what `tifffile` writes for a bool array — the `tiff` decoder normalises
  those to intensity, flipping the bit), so a mask isn't shown inverted.

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
- **Seeking past the frontier** (`--frame N` at launch, or **typing an index** in the
  frame bar's readout — a `TextEdit` committing on Enter via `seek_to`): `pending_seek`
  holds the target; `drive_seek` rides the frontier probing one page/update until the
  length passes `N` (or the real end), then snaps. While a `pending_seek` is set,
  `refresh_textures` **freezes every pane** (keeps the last committed texture) so the
  intervening frames the probe rides through are never rendered — the discovery runs as
  fast as it can and only the target frame is drawn. A within-length target is instant
  (`seek_to` jumps directly). Any manual navigation clears it.
- **Per-frame resolution:** `disp_size(i)` uses the resident frame's own size
  (page-0 fallback) so drawing/readout don't stretch or go out of bounds.
- **`ConcatSeq`** reuses all of this: a frontier miss rolls to the next file's
  page 0, so the run discovers as one seamless length (∑ page counts) with no
  concat-specific code in `drive_seek`/lookahead/playback.

---

## 5. Background decode pool (`decoder.rs`)

- `BackgroundDecoder::new(threads)` shares one `mpsc` job queue behind a `Mutex`
  (locked only for the hand-off). The thread count is `CimApp::resolve_decode_threads`:
  `config.decode_threads` clamped to `[1,16]`, or — when it's `0` (**auto**, the
  default) — `available_parallelism().clamp(2,6)`. The **Decode threads** Settings
  slider caps it for shared / VNC hosts where several instances share the CPU; a
  change is **live-applied** in `update` by rebuilding the pool (and clearing
  `inflight`, since jobs queued on the old pool won't land on the new one — they
  re-request; persistent readers reopen on demand).
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
- **Playback prefetch (`prefetch_playback`).** While playing, each on-screen pane (plus
  the control pane) pre-decodes the next `PLAY_PREFETCH` (3) frames along the loop window
  (same walk as `advance_playback`; wraps when looping), so playback overlaps decode with
  display instead of stalling on decode latency at a not-yet-resident frame — the win grows
  with pane count, since the lock-step commit waits for the slowest pane. Requests dedupe
  via `inflight` and never go past the known length (frontier discovery stays with
  `ensure_lookahead`), so re-running it every update is cheap.

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

**Staged, lock-step textures.** `app/decode.rs::refresh_textures(ctx)` (run once per
update, after state settles, just before drawing) brings **every on-screen pane**
(`displayed_indices`) up to date and flips them to their new frame **together**. For
each pane it computes a `stage_target` (the frame to show — the shared frame, or the
in-flight playback prefetch, or the pane's own when unsynced) and calls `stage`, which
renders that frame into the pane's **`pending`** slot *without disturbing the shown
`tex`* — synchronously for a cheap frame (render **only when stale**: `tex`/`pending`
already showing `(target, sig)` is reused), or off-thread for a heavy one (lands in
`pending` via `pump_render`). Only when **all** shown panes report ready does the commit
loop swap each pane whose `pending` holds the target into `tex` (the swap parks the old
texture back in `pending` for handle reuse — no per-frame allocation). `pane_texture(idx)`
(read by drawing) returns the committed `tex`, falling back to `pending` only before the
first commit so a pane isn't blank while its siblings load. **No spinner:** a pane holds
its last committed frame until the group flips. The single-pane render pipeline: bounds →
`render_into_scaled(lo, hi, step, &mut render_scratch)` (a reused buffer) →
`ColorImage::from_rgba_unmultiplied` → texture `set`/`load`.

**Display-resolution staging (minified panes).** The synchronous LUT render is done at a
**nearest-decimation** `step` (`render_into_scaled`) so a minified pane doesn't render, copy
and upload far more pixels than the screen can show — the dominant CPU cost when several
sequences play in a grid over VNC / software GL, where the texture upload is a plain memcpy.
`stage_step` picks `step` from the pane's **physical** scale `zoom × pixels_per_point`
(so OS DPI and the UI-scale zoom count): `1` (full resolution) for any physical scale ≥ 1,
rising to 2, 3, … as the pane shrinks further. Because the whole ≥1× range **and its
neighbourhood** (down to 0.5× at `ppp = 1`) stay at `step 1`, **crossing 1× never changes
what's on screen** — the same full-resolution texture is reused. Decimation only *drops*
whole samples (never blends), so each texel is still a true source value and the
pixel-accuracy invariant holds; the value-under-cursor readout reads native `FrameData`, not
the texture, so it is unaffected. `step` is part of the texture identity
(`CachedTex.step`, alongside `(shown, sig)`) so a zoom change that alters it re-renders and
re-commits. `want_step` forces `step 1` for a **heavy** proprietary-operator pane —
decimating an operator's input would change its output and thrash the size-keyed instances —
so those (and overlays, and the export path) always render full-resolution.

*Commit gotcha:* the commit swaps a pane **only when `pending` actually holds the target**
(not merely `pending.is_some()`) — otherwise an idle repaint (cursor move / pan) would keep
swapping the spent old texture back to the front and flicker between frames.

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
- **LUT_ALPHA** — full-range map then the proprietary operator at full strength
  (no options). Knobs slot in via a `ToneOptions` sub-struct + `draw_tone_options`.
- **Linear** — plain full-range map, no clip.

Plus a per-pane **DETAILS_ENHANCED** toggle. The proprietary operators
(`imageproc.rs`) run on a **single-channel 16-bit** render
(`render_into_gray_u16`, mapping the same `[lo,hi]` bounds to `[0,65535]`, one
sample per pixel) so they see full native precision, then the result is expanded
back to grey RGBA and downscaled to 8-bit for the texture. **They run only for
single-channel 16-bit (`uint16`) frames with the operator library loaded** —
otherwise LUT_ALPHA / Details fall back to the plain 8-bit LUT render
(`render_into`), and the UI disables those controls (`pane_is_op_input` +
`imageproc::lut_alpha_available`/`details_available`). The same pipeline runs in
three places, matching pixel-for-pixel: `export.rs::ensure_frame` (export worker),
and — for live view — split by weight in `stage`: **Linear / Linear+Clip, masks,
and any non-single-channel-U16 or library-absent case render synchronously** (cheap
LUT only), while **LUT_ALPHA / details on a single-channel U16 frame render off the
UI thread** on the `renderer.rs` `RenderPool` (`renderer::Worker::render`). (Export uses the
default clip percentile; `ToneOptions` are live-view only.)

The operators are **loaded at runtime** (`libloading`, Linux-only) at startup
(`imageproc::init`) from **two separate libraries**, one per operator, by their
hard-coded file names (`imageproc::LUT_ALPHA_LIB` / `DETAILS_LIB`) resolved via
the loader search path (set `LD_LIBRARY_PATH`) — not linked at build time; a
missing library is silently ignored. The operators are **heavy, size-dependent
C++ objects**, so the C ABI is a **create/apply/destroy lifecycle** per operator
(`cim_<op>_create(w,h)` → opaque handle, `cim_<op>_apply(handle, data, len)` on a
**single-channel 16-bit** buffer `len == width*height`, `cim_<op>_destroy`).
`imageproc::PaneOps` holds one pane's instances, created lazily and **rebuilt when
the frame dimensions change**, so heavy construction is paid once per size; it is
owned by the pane's render worker thread (and by each export pane), so an instance
is only ever touched by one thread. Each operator is independent: a missing library
disables only its own feature (`lut_alpha_available` / `details_available`). See
`INTEGRATION_CPP.md` for the contract and how to build the `.so`.

**Off-thread live render (`RenderPool`, §5-ish).** For a heavy pane, `stage`
computes a cheap parameter-only `tone_sig` (contrast/clip%/details/region), and
if neither the shown `tex` nor the `pending` slot holds `(target frame, sig)`,
submits a `RenderJob` (frame `Arc`, pre-computed `lo/hi` bounds, `lut_alpha`,
`details`) and returns not-ready — the pane keeps showing its last committed frame.
`render_inflight` (a set of pane ids) caps it to one render per pane, so rapid
tone/frame changes coalesce. `pump_render` (each update) drains finished jobs into
each pane's `pending` slot (not `tex` — the lock-step commit flips them); `CachedTex.sig`
lets a landed texture be recognised as current or re-requested. The pool runs **one worker thread per pane**
(keyed by stable pane `id`, spawned lazily on the pane's first heavy render,
dropped by `renderer::RenderPool::forget` on close/reload): different panes render
**in parallel**, while a single pane's operator calls stay **serialised** on its own
thread. That per-pane thread is the sole owner of the pane's (future) proprietary
operator instances — heavy to construct, dimension-keyed, not assumed reentrant — so
they need no locking. `render_inflight` still caps each pane to one in-flight job.

**Region-driven tone (`Pane.region_tone`).** When pinned (§9), a pane's linear
bounds come from the shared stats region via `region_display_bounds` — the region's
min/max (Linear) or its per-tail-percentile clip (Linear+Clip). Pixels outside the
region that exceed these bounds are clamped (the LUT saturates). LUT_ALPHA still
runs over the whole image. Recomputed on each texture rebuild; replicates to all
panes.

**Texture filtering:** always **nearest**, at every zoom, both magnification and
minification (`TextureOptions::NEAREST`). The tool is pixel-accurate — an on-screen
pixel must be a true source sample, never a blend — so there is no interpolation option
anywhere (display or export).

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

**Render-gated playback (`play_prefetch`).** Playback does **not** bump `shared_frame`
directly. When the accumulator is due, `advance_playback` picks the next frame and parks
it in `play_prefetch` (the candidate next shared frame), then stages the panes toward it;
`refresh_textures` advances `shared_frame` to it only on the commit — i.e. once **every**
on-screen pane has that frame ready. While a prefetch is in flight the accumulator is
zeroed (no burst), so a slow proprietary operator **paces** playback instead of the frame
counter racing ahead of the image. `play_prefetch` is cleared (playback step abandoned) by
pause, any manual next/prev/seek, and length clamping; unsynced panes advance their own
frame in step, staged the same way.

---

## 9. Modes & central drawing (`app/canvas.rs`)

`Mode = Grid | Single | Ab`. `draw_central` dispatches: **Grid** lays out
`grid_cells` and `draw_pane` per cell (ctrl-drag reorders via `drag_src` +
`finish_reorder`); **Single** fills with `current`; **A/B wipe** (`draw_ab`) splits
`slot_a`/`slot_b` at `ab_split` (draggable divider), pan/zoom acting on the side
under the cursor.

Per pane: `image_area(cell)` (between header and `FOOTER_H`), `draw_header` (buttons,
index, name, `frame/known(+)`, `in mem`, sync markers, close ×; the **filename is
dropped** when the header is too narrow to fit the full title — measured against the
Hide/Close span — leaving the index number and frame info so small grid cells stay
readable), `draw_footer`
(`h×w`, native format `uint8`/`uint16`/`float32` via `FrameData::kind_label`, cursor
`x y`, native value). Borders show **only during ctrl-drag**; focus is
the header tint. While `selecting_region` (export crop) the left button still pans and
the wheel zooms; the **right** button draws the crop (so reorder/click-focus/stats-region
are suppressed).

**Shared cursor (`cursor_img`/`cursor_pane`).** `draw_central` records the hovered
pane's cursor in **image space** (only when it's over a real pixel, via
`hover_img_pos`) plus which pane it came from, then every pane replicates it: a red dot
(`draw_cursor_dot`, image→screen per pane's own view) and its own native value at that
pixel in the footer (`value_string`). So the same source pixel is read across all panes
at once. The dot is **not** drawn on `cursor_pane` (its OS cursor already marks the
spot) and the whole dot is gated on `config.cursor_dot` (a Settings toggle); the
per-pane footer values are always shown. In A/B the single footer (`draw_ab_footer`)
shows the shared position with **both** A and B values.

The header is a **single row** (`header_h_for`, feeding `image_area`): the
**Transformations** button on the left, the title, then **Hide** (sets
`visible = false` — keeps the pane) and **Close** (removes it) text buttons on the
right, matching styles (Close tints red on hover to flag that it removes the pane). `image_area` is **flush** to the header/footer bars (no margin), and
egui window/popup **shadows are disabled** in `new` so nothing casts under panes or the
Compute form.

**Transformations popup** (`draw_options_popup`). The header's **Transformations**
button (left, away from ×) toggles `Pane.show_opts`, opening a foreground `Area`
under the header with: the tone `ContrastMode` + its mode-specific options
(`draw_tone_options` — **the single place to add a tone knob**: grow the mode's
`ToneOptions` sub-struct, add a row, read it in `stage`/`tone_sig`), the Details
toggle, the mask **Overlay** picker, and this
pane's **Histogram** (`ensure_pane_histogram` + `draw_histogram`, cached per pane).
Edits invalidate the texture. `Action::ToggleVis` (default `V`) toggles it for the
focused pane.

**Transformations sync (`Pane.sync_tone`, default on).** Like the Pos/Time syncs, a
pane can follow the shared set (`shared_contrast`/`shared_tone`/`shared_details`/
`shared_overlay`), toggled by the **Transf** checkbox in the manager's Sync column.
`contrast_of`/`tone_of`/`details_of`/`overlay_of` return the effective value and are
read by `stage`/`prepare_overlay`/`export_pane`/`view_command`; editing a synced
pane's popup writes the shared set and `invalidate_synced_tone` refreshes every synced
pane. `set_sync_tone(false)` snapshots the shared values in so nothing jumps. The
first opened media seeds the shared set (`add_pane`); a replayed `--tone`/`--detail`
is per-pane, so `apply_view_state` unsyncs the panes it sets.

**Overlays.** A pane may carry an `OverlaySpec { src_id, color, opacity }` — **any
single-channel media** (a boolean mask **or** a grayscale image/sequence) tinted over
it. The source list (`overlay_source_size`, single-channel resident frame, excluding
the pane itself) is offered in the popup's **Overlay** row. The spec is **config only**
so it rides the Transformations sync; the tinted texture is cached separately per pane in
`overlay_tex`. `prepare_overlay` builds it from the source's shown frame (decoded on
demand, so it works even when the source pane isn't drawn) and returns `None` on a mask
pane itself; a **boolean mask** tints where true (`render_mask_rgba`), any **other
single-channel** image tints by normalised intensity (`render_intensity_rgba`, alpha ∝
value through the frame's display range). `draw_pane` **and `draw_ab_side`** paint it at
the base image's rect (1:1), so overlays show in Grid, Single and A/B alike; cleared when
its source closes. **Sizes must match:** a newly selected source whose pixel size differs
from the target is rejected with an `error_popup`, and `prepare_overlay` skips drawing
(never stretches) on any later per-frame size drift.

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

**Compute panes.** A *generated* pane whose image is derived from other panes. The
**toolbar** "Compute" button sets `pending_compute_create`; the deferred
`add_compute_pane` adds an **unconfigured** Compute pane. `draw_compute_ui` (a
top-left foreground `Area` over the pane) has two states keyed on `Compute.computed`:
while `false` it shows the **config form** (mode + source combos + a **Compute**
button); that button runs `recompute_pane`, which on success sets `computed = true`,
so the **result image** then shows with the **Refresh** / **Save** / **Auto refresh**
controls instead. `Pane.compute` holds the `kind`, source id(s), `computed`, and the
auto-refresh flag. `media::Reduce` modes:
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
CPU, and either pipes raw RGBA to the `ffmpeg` CLI (**MP4**, H.264/libx264) or writes a
single frame as a **still** (`export::save_image`). Because the plan is a snapshot, the
video compose+encode loop runs on a **worker thread** — the UI stays responsive and
interaction can't corrupt the export. The output **format is chosen by the file
extension** (`export_format`): a bare name or `.mp4` → video; `.png`/`.jpg`/`.jpeg` → a
still of the frame currently on screen (`export_still_image`, composed inline — one
frame is cheap).

**No background in the output.** With no crop, the export region is the image **content**
on screen, not the full view (`content_region`): panning/zooming so the view shows part
image + part background no longer exports that background. Single/AB take the visible
image rect (`pane_content_in` = `image_rect ∩ area`); **Grid packs each pane's content
flush** (`packed_grid`, per-column widths / per-row heights) so there are no gaps
*between* panes either. Grid decouples each cell's composition slot from its view
reference — `GridCell { place, area, content }`: a point in `place` is remapped into the
`content` sub-rect (same size) before the pane's `view` samples it, so a flush slot still
reads the right pixels. Any still is additionally `crop_to_content`-trimmed, and MP4's
`yuv420p` ignores the alpha-0 background. **The output format is validated** by extension
(`.mp4`/`.png`/`.jpg`/`.jpeg`, or none → MP4); any other extension (e.g. a stray dot in
`clip.v2`) is rejected instead of handed to ffmpeg with an unusable name.

- `ExportPane` holds a snapshot view/clip/source plus its **own `SeqReader`** and a
  1-frame decode+render cache. A pane's **mask overlay** is snapshotted too
  (`set_overlay` + `blend_overlay`), so overlays appear in the video. `ExportSource =
  Still | Seq { path } | Files { paths } | Concat { files, map }`.
- `ExportLayout = Grid(Vec<GridCell>) | Single | Ab`. `ExportPlan.compose(t)` maps each
  output pixel back through the pane's view (Grid via `GridCell`'s place→content remap),
  sampling **nearest** — upscaling to a larger output just replicates source pixels, never
  blends them. `start` offsets so output frame `t` = timeline `start+t`.
- **Region crop** is chosen in image space ("Select…" forces Single): a **right-drag**
  draws the crop (secondary-button edge detection in `region_overlay`, like the stats
  region) while **left-drag pans and the wheel zooms** so the user can move around first;
  `screen_rect_to_image` on release maps it to image space, applied to every pane as a
  cell of exactly the crop's pixel size.
- **Frame range:** "all", else inclusive `from/to`; **"Use loop range"** adopts the
  playback window. A warning + "Load all" appears when a length isn't discovered yet.
- Output filename typed in the panel, written to the **cwd**. For video `start_export`
  spawns `run_export` (compose + `Encoder` write per frame) on a **worker thread**,
  sharing an `AtomicUsize` progress + `AtomicBool` cancel; `export_tick` just polls it
  each update, relaying cancel and joining the thread for the final outcome
  (`ExportOutcome`). A still skips all that and saves synchronously.

---

## 11. CLI (`cli.rs`) & entry (`main.rs`)

`main` → `cli::parse` → `Cli::Run { paths, view }` or `Cli::Exit(code)`.

- `-h/--help`, `-V/--version`.
- **View-state flags** (`ViewState`, 0-based, optional): `--mode`, `--cols`,
  `--zoom`, `--center X,Y`, `--frame`, `--pane`, `--control`, `--ab A,B,SPLIT`,
  `--tone` (per-pane `linear|linearclip|lutalpha`), `--detail` (per-pane `1`/`0`),
  `--show` (per-pane visibility), `--tsync` (per-pane Transformations-sync),
  `--loop LO,HI`. Generated by the in-app "View cmd" window (`view_command`), applied
  after startup files load (`apply_view_state`). The window's **Copy to clipboard**
  button and a global **Ctrl+Shift+C** shortcut both route through `copy_view_command`
  (egui's `ctx.copy_text`, so it goes via eframe's clipboard backend on every platform).
  Only present flags override defaults,
  and `view_command` **omits any flag left at its default** to keep the line short; a
  restored `--zoom`/`--center` clears `needs_fit`. Only the *shared* view is captured.
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

`Config { max_columns, ui_scale, cache_budget_mb, decode_threads, cursor_dot,
keybindings }` (the proprietary operator libraries are loaded at startup by
hard-coded name — §7 — not configured here; `decode_threads` = the background
decode pool size, `0` = auto — §5),
saved as JSON via `ProjectDirs("dev","cim","cim")` — Windows
`%APPDATA%\cim\cim\config\config.json`, Linux `~/.config/cim/cim.json`. Loaded on
start; **written only on an explicit "Save settings"** (never on exit). `config` is
edited live while `saved_config` holds the on-disk copy; Settings shows an **"Unsaved
changes"** warning whenever they differ (`config != saved_config`, needing `PartialEq`).
New `bool`/scalar fields take a `#[serde(default = …)]` so an older saved config still
loads.

`Action` = all bindable actions (view toggles, next/prev media & frame, fit/actual/
zoom, load all, open, toggle panels, play/pause, `SelectMedia(0..12)`).
`Keybindings` is a `BTreeMap<action_id, key_name>` with unique bindings. New default
bindings do **not** retroactively apply to a saved config (shows `—` until rebound).
`handle_input` skips the shortcut scan while `ctx.wants_keyboard_input()` (a text
field has focus), so typing doesn't trigger views. **Tab** (default `ToggleView`)
would otherwise be stolen by egui's built-in focus navigation, which lands on the
first toolbar button and traps every shortcut (a focused widget makes
`wants_keyboard_input` true); `handle_input` absorbs that focus move onto a throwaway
id each frame so Tab cleanly cycles the view and no button ever holds focus.

---

## 13. The update loop (`app/mod.rs::update`)

Each frame: apply `ui_scale`; `clock += 1` (on the **first** frame re-assert
`ViewportCommand::Maximized(true)` — Linux/Wayland often ignores `with_maximized` at
window creation, Windows already honoured it); **rebuild the decode pool if
`resolve_decode_threads` changed** (§5); `pump_decoder` → `pump_render` (stage
finished tone renders into `pending`) → `handle_input` → `advance_playback` → `drive_seek`;
`drive_eager` → `ensure_lookahead` → `prefetch_playback` (pre-decode upcoming frames while
playing, §5) → `poll_decoding_all` → `enforce_cache_budget`; clamp
`shared_frame` (and any stale `play_prefetch`); `refresh_textures` (stage on-screen panes
and, when all ready, flip them + commit a playback step — runs last so it sees settled
frame/tone state, just before drawing reads the textures); `refresh_auto_compute`; expire
the transient `status` note; draw toolbar,
bottom frame bar (shown whenever **any** media is a sequence), central panel, the compute
draft, windows (manager/export/settings/view-command), error popup, the **">8 sequences"
resource warning** (`pending_open` — Open anyway → `commit_open`, Quit → close); apply
deferred actions; `export_tick`; then a **paced repaint**.

**Transient notifications (`status`).** A single line shown **top-right in the toolbar**
at normal size (e.g. "Settings saved", "View command copied"). `update` shadows the
last value (`status_shadow`) to detect a fresh message, stamps `status_at`, and clears
it after `STATUS_TTL` (10 s) — so every `self.status = …` site, current and future,
auto-expires for free (and a `request_repaint_after` wakes an idle app to clear it).
Per-media errors are **not** this: they stay centred in their pane (`draw_pane_error`),
as does the modal `error_popup`. (There is no per-pane decode spinner — a pane holds its
last committed frame while the next one decodes / renders; see §7.)

**Paced repaint** (not `request_repaint()` at monitor rate — pure waste over VNC):
playback requests `request_repaint_after(1/fps)`; a pending background decode, an
**in-flight tone render** (`render_inflight`), **or a running export** (which encodes on
its own thread — we just poll progress) wakes every `DECODE_POLL` (~30 fps, enough to
pick up landed frames and commit them); a fully idle app requests no repaint at all.

Deferred actions (`pending_remove`, `pending_reload(_all)`, `pending_compute_create`,
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
+ reused buffer, per-pane histogram cache, **paced repaints** (§13, no busy-spin while
decoding/playing), **display-resolution staging** for minified panes (§7 — nearest-decimate
the synchronous render so a grid of sequences doesn't render/copy/upload full-res textures
the screen can't show; seamless across 1×), **playback decode prefetch** (§5 — overlap
decode with display so first-pass / multi-pane playback doesn't stall on decode latency),
and a **configurable decode-thread count** (§5 — cap the pool per instance on a shared
host). For shared multi-user servers there's also a **">8 sequences" resource warning**
(§13) before opening a heavy number of sequences at once. Remaining candidates: minor
per-frame allocations (`Action::all()`, `grid_cells`); a per-instance cache-budget cap /
lower default for shared hosts; and capping the software-GL (llvmpipe) rasterizer threads
per session (`LP_NUM_THREADS`), which is an env/deploy knob, not code.

---

## 16. Testing

Inline `#[cfg(test)]` (skip when fixtures/ffmpeg absent): `cli` token
expansion/grouping; `media` lazy length, eviction, **LUT render matches the float
reference** bit-for-bit, region stats + save round-trip; `export` full compose→ffmpeg
encode + **pixel-exact region crop** + content-only export (`content_region` excludes
background) + still background crop (`crop_to_content` trims to the content bounding box).

---

## 17. Conventions

- **Commits:** small, one concern; imperative summary + a short *why*. Committed
  directly to `main`.
- **Build target:** Windows, debug, during development.
- **Style:** match surrounding code (comment density, naming, `pub(super)` methods,
  free helpers in `app/mod.rs`).
- **Future media:** video (mp4/avi) slots in as another `Media` variant behind the
  same interface.
