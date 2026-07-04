# cim — Architecture & Reference

> **cim** ("Compare Images & Media") is a lossless side-by-side viewer for still
> images and multi-page TIFF sequences, built with `egui`/`eframe`. It targets
> pixel-accurate comparison: native bit depth is preserved, values are readable
> under the cursor, and the same view/timeline can be synced across panes.
>
> This document is a durable reference for how the project is laid out and how it
> works. Keep it in sync when subsystems change.

---

## 1. Build, run, test

- **Platform:** developed on Windows; the primary loop is `eframe` (OpenGL via
  `glow`). Intended to also run over **VNC with no GPU** — so software GL is the
  worst case, and CPU cost / repaint volume / texture-upload size matter.
- **Build (debug, the normal dev target):** `cargo build`
  - `main.rs` sets `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`,
    so **debug is a console app** (CLI `--help`/completion output is visible);
    release is a windowed app with no console.
  - `[profile.dev]` uses `opt-level = 1`, and `[profile.dev.package."*"]` uses
    `opt-level = 3` (deps optimized) so decode/render is usable in debug.
- **Run:** `cargo run -- [FILES|SEQUENCES]...` or the built exe with paths.
- **Tests:** `cargo test` (unit tests live inline in `media.rs`, `export.rs`,
  `cli.rs`). Tests that need fixtures or `ffmpeg` **skip gracefully** when absent.
- **Fixtures:** `examples/` (`alpes_noisy_a.tif`, `alpes_noisy_b.tif`,
  `alpes_ir16.tif` (16-bit), `alpes.jpg`).
- **CI:** `.github/workflows/build.yml` builds Windows + Linux (glibc 2.28 via
  Debian buster, `ci/build-linux-glibc228.sh`) release artifacts on tags `v*`.
  (Note: the build workflow triggers on `v*` tags only.)

### Dependencies (`Cargo.toml`)
`eframe` 0.29 (egui), `image` 0.25 (stills), `tiff` 0.9 (sequences), `rfd` 0.14
(file dialogs), `serde`/`serde_json` (config), `directories` 5 (config path),
`anyhow` (errors), `cxx` 1 (C++ FFI, with `cxx-build` in `[build-dependencies]`;
needs a host C++ compiler — see `INTEGRATION_CPP.md`). Export shells out to the
**`ffmpeg` CLI** (not a crate).

---

## 2. Source layout

```
src/
  main.rs        Entry point: parse CLI, then launch the eframe window.
  cli.rs         CLI: --help, shell completion, sequence-token expansion.
  media.rs       Data model: FrameData/Samples, Media (Still|TiffSeq),
                 SeqReader (persistent TIFF decoder), rendering, histograms.
  imageproc.rs   cxx bridge to the proprietary C++ operators (LUT_ALPHA,
                 DETAILS_ENHANCED); C++ lives in cpp/, built by build.rs.
  decoder.rs     Background decode thread pool (per-sequence persistent readers).
  view.rs        ViewTransform: zoom/pan/fit math (screen <-> image space).
  settings.rs    Config: rebindable keybindings, columns, UI scale; JSON persist.
  export.rs      Export engine: ExportPlan composition + ffmpeg Encoder.
  app/           The CimApp type (egui App), split by concern:
    mod.rs       State struct, consts, new(), loading/reload, per-pane state
                 resolution, the eframe::App update loop, shared free helpers.
    decode.rs    Decode plumbing: pump_decoder, request, load_all/drive_eager,
                 ensure_lookahead, cache-budget eviction, texture prepare().
    input.rs     apply_action (keybindings), advance_playback, handle_input.
    canvas.rs    Central image area: grid/single/A-B drawing, pan/zoom,
                 ctrl-drag reorder, header/footer, export-region overlay.
    panels.rs    Toolbar + tool windows: media manager, visualise, settings,
                 the full-width bottom frame bar, and the View-command window.
    export_ui.rs Export panel UI + building ExportPlan from live app state.
```

`app.rs` was one ~2300-line file; it is now the `app/` module. All of `CimApp`'s
methods live in sibling `impl CimApp` blocks and are marked `pub(super)` so
cross-module calls resolve. Shared types (`Mode`, `Pane`, consts) and all free
helpers live in `app/mod.rs`; siblings reach them via `use super::*`.

---

## 3. Core data model (`media.rs`)

### `Samples` / `FrameData`
- `Samples` = `U8(Vec<u8>) | U16(Vec<u16>) | F32(Vec<f32>)` — **native** samples,
  interleaved. Frames are kept at native bit depth so the UI can report true
  pixel values and histograms; the 8-bit RGBA for display is derived on demand.
- `FrameData { size:[w,h], channels:1|3|4, samples, bounds_full, bounds_clip }`.
  - Construct via `FrameData::new(size, channels, samples)` — the two
    `bounds_*: OnceLock<(f32,f32)>` cells are private (memoized display bounds).
  - `byte_len()` — resident byte size (for the cache budget).
  - `render_rgba(clip) -> Vec<u8>` and `render_into(lo, hi, &mut Vec<u8>)` —
    build the 8-bit RGBA display buffer (see §7 rendering).
  - `display_bounds(clip)` — memoized `(lo,hi)`; `pixel_string`, `histogram_display`.
  - **Boolean masks:** a frame decoded from a **1-bit bilevel TIFF** is flagged
    `mask` (`new_mask` / `is_mask()`). `render_into` then bypasses the tone
    window and paints false→black / true→white, and `render_mask_rgba(rgb,
    alpha)` builds a tinted overlay buffer (true pixels coloured, false
    transparent). Only TIFFs are masks; everything else is a normal image.
    `Media::is_mask()` (true for a `TiffSeq` whose page 0 is bilevel) lets the
    UI list masks as overlay sources.

### `Media` = `Still | TiffSeq | FileSeq | ConcatSeq`
Unified interface the app/decoder use:
- `name`, `size` (page-0 size), `frame_count`, `hi_depth` (>8-bit → clip default).
- `resident(idx) -> Option<Arc<FrameData>>`, `insert(idx, frame)`.
- `decode_job(idx) -> Option<DecodeReq>` (None for stills). `DecodeReq =
  Tiff { file, page, path } | File(path)` tells the pool **how** to decode the
  frame — seek `page` of `path` in a persistent reader keyed by `(pane id, file)`,
  or decode a standalone still file.
- Lazy length: `at_end()`, `frontier_ended()` — called when a frontier probe
  finds no page. A `TiffSeq` then hits its real end; a `ConcatSeq` rolls to the
  next file; `Still`/`FileSeq` are always `at_end`.
- Cache budget: `byte`/`resident_bytes()`, `touch(idx, clock)`, `evict(idx)`,
  `resident_frames() -> Vec<(idx, last_used, bytes)>`.

`Still { name, frame: Arc<FrameData>, hi_depth }` — single always-resident frame.

The three sequence kinds share a private **`SeqCache`** (`cache: Vec<Option<Arc<FrameData>>>`,
`last_used: Vec<u64>`, `resident_bytes`) that owns residency + LRU/budget
bookkeeping:
- `cache.len()` = **known length** (independent of residency); slots may be
  evicted to `None` without changing the length.
- `insert(idx==len)` grows the length by one (a frontier probe); `touch`/`evict`
  maintain `resident_bytes` incrementally.

`TiffSeq { name, path, size, hi_depth, frames: SeqCache, at_end }` — one multi-page
TIFF; length discovered lazily (§4). `decode_job` → `Tiff { file: 0, page: idx }`.

`FileSeq { name, paths: Vec<PathBuf>, size, hi_depth, frames: SeqCache }` — a
numbered **still** run (one file per frame) opened from a compact CLI token. Its
length is the file count (**known up front** → always `at_end`, no lazy
discovery); `decode_job(idx)` → `File(paths[idx])`. Frames decode on demand via
`media::decode_file` (dispatches TIFF-page-0 vs the `image` crate by extension).

`ConcatSeq { name, files, size, hi_depth, frames: SeqCache, map: Vec<(file,page)>,
disc_file, disc_page, at_end }` — a numbered run of **multi-page TIFFs** played
as **one continuous timeline** (when a file's pages run out, the timeline rolls
into the next file). Since per-file page counts aren't known up front, the global
length grows lazily: `map[global] = (file, page)`, and the frontier probe walks
`(disc_file, disc_page)`. `insert(idx==len)` commits the probed `(disc_file,
disc_page)` into `map` and steps `disc_page`; `frontier_ended` (a probe miss)
rolls to the next file (`disc_file+1, disc_page=0`) or, past the last file, sets
`at_end`. `load_sequence` picks `ConcatSeq` for `.tif`/`.tiff` runs and `FileSeq`
otherwise. `concat_layout()` exposes `(files, map)` for export.

### `SeqReader` — persistent per-sequence decoder
`SeqReader::open(path)` holds one `tiff::Decoder`; `decode(idx) -> Result<Option<FrameData>>`
returns `Ok(None)` when `idx` is past the last page. **Why it exists:** the tiff
crate caches IFD byte offsets only *within a Decoder*. A fresh decoder per call
makes `seek_to_image(k)` re-walk the IFD chain (O(k)), so a sweep is O(N²).
Keeping one reader alive keeps that cache warm. `decode` always seeks (the reader
may sit on any page from a prior call).

`load(path)` dispatches by extension: `tif`/`tiff` → `open_tiff` (reads **only
page 0** for size/depth; length is discovered lazily), else `open_still` (via the
`image` crate, mapping color types to `Samples`).

---

## 4. Lazy sequence-length discovery

Opening a TIFF **never walks all IFDs** (long sequences would stall on open, and
pages may differ in resolution). Instead:

- A fresh `TiffSeq` starts with `cache = [None]` (length 1), `at_end = false`.
- Decoding page `idx` past the end returns `Ok(None)` → the app calls
  `frontier_ended()`. `insert(idx == cache.len())` **grows** the known length by one.
- `app/decode.rs::ensure_lookahead` keeps **one page beyond the shown frame**
  discovered while browsing (probe index `known` when `frame_disp(i)+2 > known`).
- `frame_disp`/playback **hold at the discovered frontier** rather than wrapping,
  until `at_end` is confirmed (`current_at_end()`).
- Headers show `N+` while more frames may exist.
- **Seeking past the frontier** (e.g. a `--frame N` replay at launch) can't jump
  straight to `N`, since pages are only discovered contiguously. `pending_seek`
  holds the target; `app/decode.rs::drive_seek` rides the frontier and probes
  one page per update until the length passes `N` (or the real end is found),
  then snaps the timeline to it. Any manual frame navigation clears it.
- **Per-frame resolution:** pages can differ, so `disp_size(i)` uses the resident
  frame's own size (falling back to page-0). Drawing and the pixel readout use
  `disp_size` to avoid stretching / out-of-bounds indexing.
- **Concatenation (`ConcatSeq`)** extends the same machinery across files: a
  frontier `Ok(None)` means "this file ended", so `frontier_ended` rolls the
  probe to the next file's page 0 instead of ending the timeline; only the last
  file's end is the real end. The whole run therefore discovers as one seamless
  length (∑ page counts), and `drive_seek` / lookahead / playback need no
  concat-specific code.

---

## 5. Background decode pool (`decoder.rs`)

- `BackgroundDecoder::new(threads)` spawns `threads` workers (the app uses
  `available_parallelism().clamp(2,6)`), sharing one `mpsc` job queue behind a
  `Mutex` (lock held only for the hand-off, decode runs unlocked).
- **Jobs are addressed by stable pane `id`**, not Vec index, so results still
  land after reorder/close.
- **Persistent readers:** `readers: HashMap<(u64, usize), Arc<Mutex<SeqReader>>>`
  keyed by `(pane id, file index)`. A `DecodeReq::Tiff { file, page, path }` job
  briefly locks the map to get/open that file's reader, then locks the reader to
  decode `page`. A lone `TiffSeq` uses `file = 0`; a `ConcatSeq` keeps one reader
  per file so each file's IFD cache stays warm. Different files decode in
  parallel; pages of one file serialise on its reader. `forget(id)` drops **all**
  of a pane's readers (`retain` on the key's `id`). A `DecodeReq::File` job
  (numbered still sequence) has no persistent reader — it `media::decode_file`s
  that frame's own file.
- `request(id, frame, path)` enqueues; `drain()` collects finished `Done`
  non-blocking each update; `forget(id)` drops a reader (on reload/remove) so the
  file reopens.
- `Done.result: Result<Option<Arc<FrameData>>>` — `Ok(Some)` frame, `Ok(None)`
  past-end probe, `Err` real decode failure.

App-side plumbing (`app/decode.rs`): `inflight: HashSet<(id, frame)>` dedupes
requests; `pump_decoder` drains results (insert + `touch`, or `set_at_end`, or
set pane `error`).

---

## 6. Cache memory budget / LRU (`app/decode.rs::enforce_cache_budget`)

Frames are held at native bit depth and never freed by decode alone, so a long/
large sequence could OOM. Guard:

- `CACHE_BUDGET_BYTES` (in `app/mod.rs`) = **1.5 GiB** soft ceiling across *all*
  sequences. Tunable constant (could be surfaced in settings later).
- `CimApp.clock` increments each update; frames are `touch`ed on decode (in
  `pump_decoder`) and on display (in `prepare`) → LRU recency in `last_used`.
- When total `resident_bytes()` exceeds budget: gather resident frames that are
  **not currently shown** (each pane's `frame_disp(i)` is protected so the view
  never blanks), sort by `last_used`, evict oldest until under budget.
- If an eager **"Load all"** can't fit the budget, it is **stopped** (rather than
  thrashing against eviction) with a status note. Stills are never evicted.
- Export is unaffected — it decodes through its own `SeqReader`, not this cache.

---

## 7. Rendering pipeline (native samples → texture)

`app/decode.rs::prepare(ctx, idx)` ensures a pane shows the best texture for its
current frame and returns `(Option<TextureId>, loading)`:
- If the target frame is resident, render + upload **only when stale**
  (`tex.shown != f`), else reuse; if not resident, queue a decode and keep
  showing the last texture with `loading = true` (spinner drawn).
- Rendering: `display_bounds(clip)` (memoized) → `render_into(lo, hi, &mut render_scratch)`
  → `ColorImage::from_rgba_unmultiplied(size, &scratch)` → texture `set`/`load`.

`render_into` (`media.rs`):
- **U8/U16:** build a value-keyed **LUT** (`≤ 64 Ki` entries) once per frame, then
  the per-pixel loop is a table lookup (`fill_rgba`), avoiding float
  multiply-and-clamp per pixel. **F32:** arithmetic path (no bounded domain).
- Mono (1 colour channel) replicates grey across R/G/B; alpha = 255.
- `render_scratch: Vec<u8>` on `CimApp` is reused → no per-frame allocation.

Display bounds: full range for integers; data extent for floats; with `clip`, a
fixed **0.01% percentile stretch** (`percentile_bounds` builds a histogram).
Bounds are **content-invariant per frame**, memoized in `FrameData`'s `OnceLock`
cells so the clip histogram scan runs once per frame, not once per redraw.

**Tone modes & proprietary post-processing (C++).** Each pane picks a *tone*
mode (`ContrastMode`, media-tab dropdown, in this order):
- **Linear + Clip** — full-range map with the 0.01% percentile clip
  (`display_bounds(true)`); robust auto-contrast, the default for **>8-bit**
  media (8-bit displays 1:1 so it defaults to plain Linear).
- **LUT_ALPHA** — full-range map, then the proprietary auto-contrast operator.
- **Linear** — plain full-range map (native range → [0, 255]), no clip.

Plus a per-pane **DETAILS_ENHANCED** on/off toggle (proprietary detail enhance).
`render_into` first produces the 8-bit RGBA using the mode's built-in bounds
(`ContrastMode::clips()`), then the proprietary operators from `imageproc.rs`
(`lut_alpha` when the mode is LUT_ALPHA, `details_enhanced` when the toggle is
on) transform the RGBA in place before upload. Both take/return interleaved
RGBA8 and run in `app/decode.rs::prepare` (live) and `export.rs::ensure_frame`
(export), so exports match the screen. The C++ bridge, data contract, and
drop-in steps are documented in `INTEGRATION_CPP.md`.

**Region-driven tone (`Pane.region_tone`).** When pinned (the stats panel's
"Tone ⟵ region" button, §9), a pane's linear bounds come from the shared stats
region via `FrameData::region_display_bounds` — the region's **min/max** (Linear)
or its **0.01% percentile** (Linear+Clip) — instead of `display_bounds`. Pixels
outside the region that exceed these bounds are **clamped** by the render (the
LUT covers the full sample domain and saturates), so extremes elsewhere go
black/white while the region drives the contrast. LUT_ALPHA is unaffected: it
still runs over the **whole** rendered image after the (region-derived) linear
map. `region_tone` is recomputed on each texture rebuild (so it tracks the frame
in a sequence) and replicates to every pane.

Texture filtering (`tex_options`): magnification follows `config.vis.interp`
(Nearest/Bilinear); minification Linear.

---

## 8. View / sync model

`ViewTransform` (`view.rs`): `{ zoom, center (image-space), needs_fit }` with
`fit`, `actual_size`, `img_to_screen`/`screen_to_img`, `image_rect`, `zoom_at`
(anchored), `pan`. Zoom clamps to `[1e-4, 512]`.

Each `Pane` has its own `transform` and `frame`, plus `sync_spatial` /
`sync_temporal` flags. `CimApp` holds a `shared_view` and `shared_frame`:
- `view_ref/view_mut(i)` → shared view if `sync_spatial`, else the pane's own.
- `frame_disp(i)` → `shared_frame` clamped to the pane's length if
  `sync_temporal` (shorter sequences **hold on their last frame**), else the
  pane's own `frame % len`.
- Toggling sync **off** snapshots the shared state into the pane so it doesn't
  jump. The media manager offers per-column and aggregate ("all") toggles.

`timeline_len()` = the **control** pane's known length; it drives the loop. The
control pane (the sequence driving the shared timeline / transport) is **separate
from `current`** (the focused pane for Single view / keyboard / header tint), so
selecting a still to view doesn't hijack or hide playback. `ensure_control` keeps
`control` on a sequence (repointing if it isn't one), and the media manager's
**Control** selector (Sync column, sequences only) chooses which one.

Playback loops over a **window** `loop_bounds(len)` — a user sub-range
(`loop_range: Option<(lo,hi)>`, set by dragging the scrubber's brackets; `None`
= whole sequence, reset via the transport's loop-range button). When the window
is the full sequence and the end isn't discovered yet, `hi` is only the frontier
and playback holds there rather than wrapping early; a sub-range (both bounds
known) wraps/stops at `hi` immediately. The scrubber (`draw_scrubber`) shades
memory-resident frames in the accent colour (contiguous runs merged) over a
grey base, dims outside the loop window, and draws the two draggable brackets.
Playback (`advance_playback`) accumulates `stable_dt`, steps `shared_frame`
at `fps`, holds at the frontier when `!current_at_end()`, and advances unsynced
panes independently.

---

## 9. Modes & central drawing (`app/canvas.rs`)

`Mode = Grid | Single | Ab`. `draw_central` dispatches:
- **Grid:** `grid_cells(visible, area)` lays out `config.max_columns`;
  `draw_pane` per cell. Ctrl-drag reorders (`drag_src` + `finish_reorder`,
  `panes.swap` + `remap` of `current/slot_a/slot_b`).
- **Single:** the `current` pane fills the area.
- **A/B wipe:** `draw_ab` shows `slot_a`/`slot_b` split at `ab_split` (draggable
  divider, `HANDLE_HIT` grab zone); pan/zoom acts on the side under the cursor.

**Mask overlays.** A pane may carry a `MaskOverlay { src_id, color, opacity,
tex }` — a boolean-mask media (referenced by stable pane id) tinted and drawn on
top. `app/decode.rs::prepare_overlay` builds/caches the tinted texture from the
mask's currently shown frame (via `render_mask_rgba`) and `draw_pane` paints it
over the base image at the *same* image-space rect (1:1). The mask frame is
**decoded on demand** there, so the overlay works even when the mask pane isn't
drawn (hidden, or just reloaded — `reload` invalidates dependent overlay
textures). Configured per pane in the media manager's **Overlay** column (mask
picker + colour + α, with an aggregate row); cleared when its source mask is closed. Aligns pixel-for-pixel, so a mask is expected to match
its target's dimensions.

Per pane: `image_area(cell)` (between `HEADER_H` header and `FOOTER_H` footer),
`draw_header` (index, name, `frame/known(+)`, `in mem`, sync markers, close ×),
`draw_footer` (`h×w`, cursor `x y`, native pixel value via `pixel_string`).
Borders appear **only during ctrl-drag** (blue = moved pane, green = swap
target); there is no persistent focus border (it doubled at `GAP = 0`). Focus is
shown by the header tint.

Interaction guards: while `selecting_region` (export crop), pane pan/zoom is
disabled so the drag isn't stolen.

**Statistics region (right-drag).** A **right-button drag** on any pane selects
a rectangle; it is stored in **image space** (`stats_region: Option<Rect>`, like
the export crop) so the same region — and each pane's own statistics for it —
**replicate across every pane**. `region_overlay_for_pane` (called from
`draw_pane` and both A/B sides) runs the selection edge-detection
(`region_input`, tracking the secondary button + a per-pane `stats_sel_*`
anchor), draws the live rubber band, and otherwise draws the committed outline
plus a **stats panel** under it: a mini histogram (`draw_region_hist`, Visualise
style, with min/max labelled at its ends) and a verbose one-per-row list of
mean / std / pixel count, computed by `FrameData::region_stats` and cached per
pane (`RegionStatsCache`, keyed on `(frame, stats_gen)`). A near-zero drag (or a
plain right-click) **clears** the region. The panel's **"compute LUT from
region"** toggle pins every pane's tone to the region (`apply_region_tone`,
see §7). The toolbar **"Stats"** button (`show_stats`) hides/shows the panels
(the outline stays); pan/reorder are switched to **primary-button-only** so the
right-drag is never stolen.

---

## 10. Export (`export.rs` + `app/export_ui.rs`)

The app builds a self-contained **`ExportPlan`** (snapshot of layout, views, clip
flags, sources, frame range) decoupled from live state, then composites each
output frame on the CPU and pipes raw RGBA to the `ffmpeg` CLI (H.264, libx264).

- `ExportPane` holds a snapshot view/clip/source plus its **own `SeqReader`**
  (lazily opened) and a 1-frame decode+render cache. A pane's **mask overlay** is
  snapshotted too (`set_overlay`: the mask's `ExportSource` + tint + its own
  decode cache); `blend_overlay` tints it over the base at sample time, so mask
  overlays appear in the exported video just as on screen. `ExportSource =
  Still(Arc<FrameData>) | Seq { path } | Files { paths } | Concat { files, map }`
  — a numbered still run exports each file standalone; a concatenation follows
  its `(file, page)` map, reopening the reader when the timeline crosses files.
  (A `ConcatSeq`'s export length is the **discovered** timeline at build time —
  press "Load all" first to export it in full.)
- `ExportLayout = Grid(Vec<(pane,rect)>) | Single(pane,rect) | Ab{a,b,img,split_x}`.
- `ExportPlan.compose(t)` maps each output pixel back through the pane's view and
  samples (nearest **or** bilinear — export forces **nearest** to preserve
  detail). `start` offsets the timeline so output frame `t` = timeline `start+t`.
- **Region crop** is chosen in **image space**: "Select…" forces Single view; the
  screen drag is converted to image-space on release (`screen_rect_to_image`) and
  then applies to *every* pane (each becomes a cell of exactly the crop's pixel
  size via `region_view`, 1:1). `dim_outside` shows it on all panes.
- **Frame range:** "all" by default, else inclusive `from/to` (0-based, clamped
  to the known timeline); a **"Use loop range"** button adopts the current
  playback loop window. A warning + "Load all" button appears when any media's
  true length isn't discovered yet.
- Output: filename typed in the panel, written to the **current working dir** (no
  save dialog). `Encoder` streams frames one-per-update on the UI thread;
  `export_tick` drives it, `kill`/`finish` manage ffmpeg.

---

## 11. CLI (`cli.rs`) & entry (`main.rs`)

`main` collects argv → `cli::parse` → either `Cli::Run { paths, view }` (launch
GUI) or `Cli::Exit(code)`.
- `-h/--help`, `-V/--version`.
- **View-state flags** (`ViewState`, all 0-based, all optional): `--mode
  <grid|single|ab>`, `--cols`, `--zoom`, `--center X,Y`, `--frame`, `--pane`,
  `--ab A,B,SPLIT`, `--tone T,T,…` (per-pane `linear|linearclip|lutalpha`),
  `--detail B,B,…` (per-pane `1`/`0`), `--loop LO,HI` (playback loop range).
  `--tone`/`--detail` are positional over the panes. These reproduce a saved
  viewpoint and are normally *generated*
  by the in-app "⧉ View cmd" window (`CimApp::view_command`), then applied once
  after the startup files load (`CimApp::apply_view_state`). Only present flags
  override defaults; a restored `--zoom`/`--center` clears `needs_fit` so the
  auto-fit doesn't stomp it. Only the *shared* view is captured (unsynced panes
  fall back to it), and sequences are listed as individual files.
- Positional args accept a **compact numbered-sequence token**
  `PREFIX%0Nd,START,END.EXT` (e.g. `frame_%05d,0,12.tif` → `frame_00000.tif` …
  `frame_00012.tif`), expanded at launch (`expand_sequence_token`).
- Each positional becomes a `cli::Input`: a bare path → `Single`, a token naming
  ≥2 files → `Sequence { token, files }`. A `Sequence` opens as **one** pane (not
  a pane per file): a `.tif`/`.tiff` run becomes a `ConcatSeq` (its files played
  as one continuous timeline), any other extension a `FileSeq` (one still per
  frame). The app keeps `token` on the pane's `Source` so reload re-opens the
  whole run and the View-command panel re-emits the token (round-tripping the
  sequence). Drag-and-drop / the file dialog only ever produce `Single`s.
- `--complete <word>` lists loadable completions for shell integration: it hides
  non-loadable extensions, offers directories, and **collapses contiguous
  numbered runs** into the compact token (`group_files`/`split_index`).
- `--completions <bash|powershell>` prints a ready-to-source completer.
- `LOADABLE_EXTS = [tif,tiff,png,jpg,jpeg,bmp,webp]` — shared by the file dialog
  and the completion filter so they can't drift.

---

## 12. Settings & persistence (`settings.rs`)

`Config { max_columns, vis: { interp }, ui_scale, keybindings }`, saved as JSON
via `directories::ProjectDirs("dev","cim","cim")` → `config.json`. Loaded on
start, saved on exit / explicit save.

`Action` enum = all bindable actions (view toggle + direct Grid/Single/A-B,
next/prev media & frame, fit/actual/zoom, load all, open, toggle panels,
play/pause, `SelectMedia(0..12)`). `Keybindings` is a `BTreeMap<action_id,
key_name>`; bindings are unique (setting a key clears it elsewhere). Defaults
include `Tab` cycle, `G/U/B` direct views, arrows, `F`, digits for media, etc.
**Note:** new default bindings do **not** retroactively apply to a user's saved
config — the action just shows `—` until rebound (a known limitation).

---

## 13. The update loop (`app/mod.rs::update`)

Order each frame:
1. Apply `ui_scale` (egui `zoom_factor`).
2. `clock += 1`.
3. `pump_decoder` → `handle_input` → `advance_playback`.
4. `drive_eager` (Load-all) → `ensure_lookahead` (browsing) → `poll_decoding_all`
   (clear the status when a batch lands) → `enforce_cache_budget`.
5. Clamp `shared_frame` to the timeline.
6. Draw: top toolbar panel, the full-width bottom frame bar (`draw_frame_bar`,
   shown whenever **any** loaded media is a sequence (`any_sequence`) so it
   doesn't drop/shift when a still is focused — a click/drag-seekable scrubber
   plus transport controls that follow the *control* sequence), central panel,
   then windows (manager/vis/export/settings/**view-command**), the error popup,
   and apply deferred `pending_remove/reload(_all)`.
7. `export_tick` if a run is active.
8. `request_repaint()` while playing, decoding (`!inflight.is_empty()`), or
   exporting.

Deferred actions (`pending_remove`, `pending_reload`, `pending_reload_all`,
`error_popup`) avoid mutating panes mid-draw-closure.

---

## 14. Invariants & gotchas

- **Pane `id` is stable** across reorder/close and keys decode results + the
  decoder's persistent readers. Vec index is *not* stable — never key by it.
- `cache.len()` is the **known length**, not residency; eviction keeps length.
- `insert` only grows length when `idx == cache.len()` (contiguous discovery);
  out-of-range inserts are ignored.
- **Protected frames:** each pane's `frame_disp(i)` is never evicted.
- `frame_disp` clamps to the pane's own length; after reload the frame index is
  kept and re-discovered lazily.
- `disp_size(i)` (not `media.size()`) must be used for drawing/readout because
  pages can vary in resolution — using page-0 size risks stretch/OOB panic.
- Files are opened **read-only with shared access**, so persistent `SeqReader`s
  don't block external writes; `forget(id)` on reload picks up new contents.
- Export decodes independently of the display cache; capping the cache never
  truncates an export (but export length = the **known** timeline at build time —
  press "Load all" first to export a not-yet-discovered sequence fully).

---

## 15. Performance notes (VNC / no GPU)

Done: lazy length (fast open), persistent readers (O(1)-ish seeks vs O(N²)),
bounded LRU cache (no OOM), LUT render + memoized bounds + reused buffer (cheap
redraws). Remaining candidate ameliorations (not yet done), roughly in value
order:
- **Repaint throttling** while merely waiting on decodes (currently repaints at
  full rate whenever `!inflight.is_empty()`); use `request_repaint_after`.
- **Threaded export** (compose+encode off the UI thread; currently one frame per
  UI repaint, so throttled by paint rate).
- Minor: `Action::all()` + per-action `ctx.input()` each frame; per-frame
  `visible_indices()`/`grid_cells()` allocations; display-downscale when a large
  image is shown in a tiny grid cell (uploads full-res regardless); the final
  `ColorImage` copy inside egui (would require coupling `media.rs` to `Color32`).

---

## 16. Testing

Inline `#[cfg(test)]` modules (skip when fixtures/ffmpeg absent):
- `cli`: token expansion, non-token pass-through, run grouping, digit splitting.
- `media`: lazy length discovery; eviction frees bytes & keeps length; **LUT
  render matches the float reference** bit-for-bit (u8/u16, mono/RGB).
- `export`: full compose→ffmpeg encode of a few frames; **region crop is
  pixel-exact**.

---

## 17. Conventions

- **Commits:** small, one concern each; imperative summary + a short body
  explaining the *why*. Committed directly to `main` in this project.
- **Build target:** Windows, debug, during development.
- **Style:** match surrounding code (comment density, naming, `pub(super)` for
  cross-module `CimApp` methods, free helpers in `app/mod.rs`).
- **Future media:** video (mp4/avi) is intended to slot in as another `Media`
  variant behind the same interface.
