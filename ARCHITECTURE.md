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
- **Tests:** `cargo test` (inline in `media/*.rs`/`export.rs`/`cli.rs`/`renderer.rs`).
  Fixtures are **generated synthetically** at test time by `src/testutil.rs`
  (multi-page u16 TIFFs, PNG runs, a hand-written 1-bit bilevel-mask TIFF), so the
  suite runs anywhere; only the MP4 encode test skips gracefully when `ffmpeg` is absent.
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
- **`.exe` icon (`build.rs`):** on Windows the same `assets/icon.png` is re-encoded
  to a 256×256 `.ico` in `OUT_DIR` and embedded as a Windows resource via
  `winresource` (needs the SDK resource compiler `rc.exe`), so the file/taskbar icon
  matches the runtime window icon. No-op on other targets; Windows-only build-deps
  (`image`, `winresource`).

---

## 2. Source layout

```
src/
  main.rs        Entry point: parse CLI, then launch the eframe window (maximized).
  cli.rs         CLI: --help, shell completion, sequence-token expansion.
  media/         Data model, split by concern (re-exported from mod.rs):
    mod.rs       FrameData/Samples core (accessors, crop), save_frame,
                 placeholder_frame.
    source.rs    Media (Still|TiffSeq|FileSeq|ConcatSeq) + SeqCache + DecodeReq:
                 the source kinds behind one interface, length discovery, LRU.
    loader.rs    load*/open*/decode* constructors, SeqReader (persistent TIFF
                 decoder), bilevel-mask bit handling.
    render.rs    Tone rendering: LUT render (render_into / _scaled / _gray_u16),
                 mask/intensity overlay tints, display-bounds.
    stats.rs     Histograms, region stats/bounds, Compute reductions (mean/std/diff).
    percentile.rs  The one per-tail percentile histogram scan (rect + fallback),
                 shared by whole-image auto-contrast and region tone.
  imageproc.rs   Runtime loader (libloading) for the proprietary C++ operators
                 (LUT_ALPHA, DETAILS_ENHANCED); C++ in cpp/ is built separately
                 into two .so, loaded by hard-coded name. PaneOps owns a pane's
                 per-operator instances (create/apply/destroy; 16-bit only) and
                 the shared render tail render_display; ops_active gates them.
  decoder.rs     Background decode thread pool (per-sequence persistent readers).
  renderer.rs    Off-thread tone-render pool: builds the display RGBA (via
                 PaneOps::render_display) for heavy panes so the UI never blocks.
  debug.rs       Opt-in pipeline profiler (CIM_DEBUG=1): per-stage timing rings.
  view.rs        ViewTransform: zoom/pan/fit math (screen <-> image space).
  settings.rs    Config, keybindings, ContrastMode/ToneOptions; JSON persist.
  export.rs      Export engine: ExportPlan composition + ffmpeg Encoder.
  testutil.rs    #[cfg(test)] synthetic fixture generators (multi-page TIFF, PNG
                 runs, bilevel-mask TIFF) so fixture-driven tests run anywhere.
  app/           The CimApp type (egui App), split by concern:
    mod.rs       State struct + sub-structs (Export/Playback/StatusLine/
                 RegionSel/LineSel/PaneTex/Watch/Deferred), consts, new(),
                 per-pane state resolution, the update loop (tick / draw_modals /
                 apply_deferred).
    lifecycle.rs Open/add/remove/reload media; view-state replay + "View cmd".
    compute.rs   Compute panes: reduce/diff/recompute/auto-refresh/save.
    watch.rs     Auto-reload file watching (source_file_sig / poll_watches).
    decode.rs    Decode plumbing, cache-budget eviction, lock-step texture
                 staging/commit (refresh_textures/stage/pane_texture).
    input.rs     apply_action (keybindings), advance_playback, handle_input.
    util.rs      Small stateless helpers (remap / drop_target / wheel input /
                 ellipsize).
    canvas/      Central image area, split by feature:
      mod.rs         Layout core: draw_central, draw_pane, grid, reorder, export
                     crop overlay.
      chrome.rs      Per-pane header/footer/error text, shared-cursor dot.
      transform.rs   Rotation-aware image<->screen math + region selection +
                     angle/paint helpers.
      ab.rs          A/B wipe view.
      options_popup.rs  The Transformations popup (draw_tone_options — the place
                     to add a tone knob).
      region_stats.rs   Right-drag stats region + panel.
      line_profile.rs   Shift+right-drag profile line overlay.
      compute_ui.rs     In-pane Compute controls.
    panels.rs    Toolbar, media manager (drag the ⠿ handle to reorder rows via
                 `drop_target` + `remap_move`), settings, view-command, frame bar.
    profile.rs   The Line-profile plot window.
    export_ui.rs Export panel UI + building ExportPlan from live app state.
```

`CimApp`'s methods live in sibling `impl` blocks marked `pub(super)`; shared types
(`Mode`, `Pane`, the field sub-structs, consts) and free helpers live in
`app/mod.rs`, reached via `use super::*` (canvas submodules use `crate::app::*`,
being one level deeper). Many CimApp fields are grouped into sub-structs —
`self.export.*`, `self.playback.*`, `self.status`, `self.region_sel` /
`self.line_sel`, and per-pane `pane.tex` (a `PaneTex` owning the commit swap) /
`pane.watch` (a `Watch`).

---

## 3. Core data model (`media/`)

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
decoding past the end returns `Decoded::End` → `frontier_ended()`, and `insert(idx ==
len)` grows length by one.

- `ensure_lookahead` keeps **one page beyond the shown frame** discovered while
  browsing; playback **holds at the frontier** rather than wrapping until `at_end`.
- Headers show `N+` while more frames may exist.
- **Seeking past the frontier** (`--frame N` at launch — so exported view commands
  restore instantly — or **typing an index** in the frame bar's readout — a `TextEdit`
  committing on Enter via `seek_to`): `pending_seek` holds the target; `drive_seek`
  rides the frontier one page/update until the length passes `N` (or the real end),
  then snaps. **The intervening pages are discovered by a metadata-only probe, not
  decoded:** `drive_seek` calls `probe` (not `request`), which issues a `DecodeReq::Tiff
  { probe: true }`; the worker runs `SeqReader::probe` = `seek_to_image` (walk the IFD
  chain, cheap once offsets are cached) + report existence, **without `read_image`** —
  so a far seek walks headers instead of decompressing every frame it passes. A probe
  hit (`Decoded::Exists`) grows the known length by one **empty** (non-resident) slot
  via `note_frontier`/`SeqCache::note_len`; a miss (`Decoded::End`) ends the frontier.
  Only the landed target frame is actually decoded (by `refresh_textures` once the seek
  clears). `ensure_lookahead` is **suppressed while `pending_seek` is set** so it can't
  fire a full decode of the same frontier page and defeat the probe. `refresh_textures`
  also **freezes every pane** (keeps the last committed texture) so the intervening
  frames are never rendered. A within-length target is instant (`seek_to` jumps
  directly). Any manual navigation clears it.
- **A synced pane behind an already-advanced timeline** (loading a second sequence after
  moving ahead in the first — `shared_frame` is past what the new pane has discovered)
  uses the **same probe fast-path** without `pending_seek`, per-pane: `catching_up(i)` is
  true (paused, still-discovering, target ≥ its frontier), so `ensure_lookahead` **probes**
  that pane forward (metadata only, no full decode of the pages in between) and
  `refresh_textures` **skips staging it** — it holds its last committed frame (blank if
  new) instead of flipping through 0…N — until its own length passes the target, then it
  stages just that frame. The `update` clamp pins `shared_frame` to the **control** pane's
  length, so the control pane is never "catching up" (that's `pending_seek`'s job); this
  covers the *other*, shorter/newer synced panes, and only while paused (playback still
  discovers frame-by-frame at the frontier). Both this and `pending_seek` **repaint
  immediately** while riding the frontier, so discovery runs as fast as probes land rather
  than one per 30 fps decode-poll tick.
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
  `Done.result: Result<Decoded>` — `Decoded::Frame` a decoded frame, `Decoded::Exists`
  a **metadata-only** frontier probe hit (`DecodeReq::Tiff { probe: true }`, page exists
  but not decoded — §4), `Decoded::End` past-end, `Err` failure.
- App side (`app/decode.rs`): `inflight: HashSet<(id, frame)>` dedupes both `request`
  and `probe`; `pump_decoder` drains (insert + `touch`, or `note_frontier` for a probe
  hit, or `frontier_ended`, or set pane `error`).
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
- **Bulk loads (`Pane.eager: Eager` = `Off | Full | Offsets`), driven by
  `drive_eager`:**
  - **"Load all"** (`Eager::Full`) decodes every known frame and drives the frontier
    to the end. When it exceeds the budget, `enforce_cache_budget` **downgrades it to
    `Eager::Offsets`** (sets `load_cache_exhausted`) rather than stopping — so length
    discovery **continues with metadata-only probes (headers alone)** while eviction
    keeps memory bounded. Decoding just stops adding frames the cache can't hold.
    - **Fast-forward stride** (`fast_forward`, ≥1, the `FF` field right of the Load all
      button): decode only **1 of every `fast_forward` frames**; the `ff-1` between are
      **skimmed by a header probe** (`probe`, never decoded) — to skim a huge sequence
      fast and low-memory. Applies to **both**: "Load all" (`(0..known).step_by(ff)` +
      a probed frontier) *and* **playback** (§8 — `advance_playback` steps by `ff`;
      `prefetch_playback` strides to match; `ensure_lookahead` probes the frontier when
      `ff > 1` so the jumped-over frames aren't decoded). Viewing/landing on a frame
      still decodes it on demand (`stage`); `1` = every frame (unchanged). For an
      instant skim of an *undiscovered* sequence, run **Load offsets** first so the
      length is known and playback can jump freely.
  - **"Load offsets"** (`Eager::Offsets`) drives the frontier to the true end with
    **probes only** (no pixel decode, no cache pressure) — enough to complete the
    timeline / export range.
  - A **Stop** button (frame bar / export panel, shown while `decoding_all`) cancels
    either via `stop_load`.
- Stills never evict. Export decodes through its own `SeqReader`, so it's unaffected —
  but an **export-initiated "Load all"** that hits the budget raises a modal warning
  on completion (`warn_popup`) that not the whole sequence is resident (`§10`).

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

`render_into` (`media/render.rs`): **U8/U16** build a value-keyed **LUT** (≤ 64 Ki) once
per frame then table-look-up per pixel; **F32** maps arithmetically. Mono replicates
grey across R/G/B; alpha = 255.

**Display bounds:** full range for integers; data extent for floats; with `clip`, a
per-tail percentile stretch (default **0.01%**). Bounds are content-invariant per
frame, memoized in `FrameData`'s `OnceLock` cells.

**Tone modes & C++ post-processing.** Each pane picks a `ContrastMode` plus
`ToneOptions` (edited in the Transformations popup, §9):
- **Linear** — full-range map (native range → [0,255]) with an **optional per-tail
  percentile clip**: `ToneOptions.clip` is a toggle (`enabled`) + the percentile
  (`clip_bounds(percent)`). The clip **defaults on for >8-bit** sources (robust
  auto-contrast) and **off for 8-bit** (which displays 1:1); both the toggle and the
  percentile are seeded in `add_pane` and editable per pane. The default mode.
- **LUT_ALPHA** — full-range map then the proprietary operator at full strength
  (no options; ignores the clip). Knobs slot in via `draw_tone_options`.

(The old separate **Linear + Clip** mode was folded into Linear's clip toggle.)

Plus a per-pane **DETAILS_ENHANCED** toggle. The proprietary operators
(`imageproc.rs`) run on a **single-channel 16-bit** render
(`render_into_gray_u16`, mapping the same `[lo,hi]` bounds to `[0,65535]`, one
sample per pixel) so they see full native precision, then the result is expanded
back to grey RGBA and downscaled to 8-bit for the texture. **They run only for
single-channel 16-bit (`uint16`) frames with the operator library loaded** —
otherwise LUT_ALPHA / Details fall back to the plain 8-bit LUT render
(`render_into`). **One predicate decides when the operators run —
`imageproc::ops_active(frame, lut_alpha, details)`** (folding in `is_op_input` +
`lut_alpha_available`/`details_available`, and excluding masks); the UI-gating
`pane_is_op_input` and the pane-indexed `CimApp::pane_ops_active` sit alongside it.
The heavy render **tail** (gray16 render → operators → expand to RGBA, else plain
LUT) is itself a **single function, `imageproc::PaneOps::render_display`**, so the
paths that use it match pixel-for-pixel by construction rather than by discipline.
It runs in two places: the **export worker** (`export.rs::ExportPane::render` — on
the **cropped region only**, §10) and, for live view, the off-UI-thread
`renderer.rs` `RenderPool` (`renderer::Worker::render`). `stage` splits by weight:
**Linear (clipped or not), masks, and any non-single-channel-U16 or library-absent
case render synchronously** (cheap `render_into_scaled`, no operators), while
**LUT_ALPHA / details on a single-channel U16 frame go off-thread** to
`render_display`. The export worker honours the pane's **clip toggle and
percentile** too (`ExportPane.clip: Option<f32>` → `clip_bounds`/`display_bounds`),
so an exported frame matches the live view's tone exactly.

The operators are **loaded at runtime** (`libloading`, Linux-only) at startup
(`imageproc::init(dir)`) from **two separate libraries**, one per operator, by
their hard-coded file names (`imageproc::LUT_ALPHA_LIB` / `DETAILS_LIB`). The
directory is the **Library folder** Setting (`config.cpp_lib_dir`): when set,
each lib is loaded as `<dir>/<name>`; when empty, it defaults to a **`LIBS`
folder next to the cim executable** (`<cim location>/LIBS`), and only if the
executable path can't be resolved is the bare name left to the loader search
path (`LD_LIBRARY_PATH`) — see `cpp_lib_dir`. Not linked at build time; a missing
library is silently ignored. Changing the folder in Settings **auto-loads**
without a restart: `update` notices `cpp_lib_dir` changed and calls
`CimApp::load_cpp_libs` → `imageproc::load_missing`, which only ever *adds* a
not-yet-loaded library, never unloads one, so it can't dangle the
`apply`/`destroy` pointers copied into live render/export instances (it then
invalidates textures to re-render when something newly loads). Repointing an
*already-loaded* operator at a different folder still needs a restart. The operators are **heavy, size-dependent
C++ objects**, so the C ABI is a **create/apply/destroy lifecycle** per operator
(`cim_<op>_create(w,h)` → opaque handle, `cim_<op>_apply(handle, data, len)` on a
**single-channel 16-bit** buffer `len == width*height`, `cim_<op>_destroy`).
**DETAILS_ENHANCED's `apply` takes a second buffer** — the **after-LUT 8-bit**
companion of the same frame: the **current view LUT output** (the 16-bit buffer
after any LUT_ALPHA, else the linear/clip map, downscaled to 8 bits, built in
`PaneOps::apply`) — so it sees whatever tone the pane is actually showing, not
just the raw 16-bit data.
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
min/max (clip off) or its per-tail-percentile clip (clip on). Pixels outside the
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
`stable_dt`, steps at `fps`, and advances unsynced panes independently. With a
**fast-forward stride** (`fast_forward` > 1, §6) it steps by `fast_forward` frames
(clamped to the window end), skimming those in between; `prefetch_playback` strides to
match and `ensure_lookahead` probes (headers) rather than decoding the jumped-over
frontier frames, so playback skims a big sequence without reading every frame.

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

## 9. Modes & central drawing (`app/canvas/`)

`Mode = Grid | Single | Ab`. `draw_central` dispatches: **Grid** lays out
`grid_cells` and `draw_pane` per cell (ctrl-drag reorders via `drag_src` +
`finish_reorder`); **Single** fills with `current`; **A/B wipe** (`draw_ab`) splits
`slot_a`/`slot_b` at `ab_split` (draggable divider), pan/zoom acting on the side
under the cursor.

**Wheel:** over a pane the wheel **zooms** (about the cursor), but with **Ctrl held it
scrubs the sequence** a frame at a time (up = next, down = previous) — routed through
`apply_action(NextFrame/PrevFrame)`, so it steps the shared timeline exactly like the
next/prev-frame keys (same frontier-hold / wrap-at-end). Reads `raw_scroll_delta` (always
populated even under Ctrl); egui's own Ctrl-scroll UI-zoom is never applied (the app pins
`zoom_factor` to `config.ui_scale` each frame). Works in Grid/Single and both A/B sides.

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
**Transformations** button on the left, the title, then the **◉ auto-reload**
toggle, **⟳ Reload** (re-reads this media from disk → `pending_reload`), **Hide**
(sets `visible = false` — keeps the pane) and **Close** (removes it) buttons on the
right, matching styles (Close tints red on hover to flag that it removes the pane).

**Auto-reload (file watch).** The **◉** toggle (amber while on, left of ⟳ Reload;
hidden for a Compute pane, which has its own Auto-refresh) sets `Pane.watch`.
`poll_watches` (run each `update`, before `refresh_textures`) stats the pane's
source file(s) — `source_file_sig` = latest mtime + total length across
`Source::File` / `Source::Sequence` — and reloads the pane once a change has
**settled** (`WATCH_DEBOUNCE`, so a file still being written externally isn't read
half-finished; each further change re-arms the timer). A `stat` is microseconds, so
watching is negligible; only the (heavier) `reload` fires, and only on quiescence.
`watch_loaded` is the baseline signature (re-based after any reload and when the
toggle is switched on, so enabling never triggers an immediate reload); an
unreadable stat (mid-rename) simply waits for the next poll. While any pane watches,
an otherwise-idle app wakes every `WATCH_POLL` to stat — the one intentional break
from "idle requests no repaint", kept slow to stay VNC-friendly (§13/§15).
Hiding the **focused** pane (via the header button or the manager checkbox) moves
focus to the nearest still-shown media (`reselect_if_hidden`), so `current` never
sits on a hidden pane while others are visible. `image_area` is **flush** to the header/footer bars (no margin), and
egui window/popup **shadows are disabled** in `new` so nothing casts under panes or the
Compute form.

**Transformations popup** (`draw_options_popup`). The header's **Transformations**
button (left, away from ×) toggles `Pane.show_opts`, opening a foreground `Area`
under the header with: the tone `ContrastMode` + its mode-specific options
(`draw_tone_options` — **the single place to add a tone knob**: grow the mode's
`ToneOptions` sub-struct, add a row, read it in `stage`/`tone_sig`), the Details
toggle, the mask **Overlay** picker, and this
pane's **Histogram** (`ensure_pane_histogram` + `draw_histogram`, cached per pane).
A tone edit **does not null the texture**: it only changes the pane's `tone_sig`,
so `stage` re-renders and the lock-step commit swaps in the fresh frame while the
pane keeps showing its last committed `tex`. Nulling `tex` would blank a **heavy**
(async, off-thread) LUT_ALPHA/details render to **black** until it lands — a cheap
LUT refills synchronously the same update so its black is never seen, which is why
only the operator tones flashed. (Only overlay edits drop `overlay_tex`, and
data-changing events — reload, recompute, newly loaded operator library — still
null `tex` since the frame data, not the signature, changed.) `Action::ToggleVis`
(default `V`) toggles the popup for the focused pane.

**Transformations sync (`Pane.sync_tone`, default on).** Like the Pos/Time syncs, a
pane can follow the shared set (`shared_contrast`/`shared_tone`/`shared_details`/
`shared_rotation`/`shared_overlay`), toggled by the **Transf** checkbox in the manager's
Sync column. `contrast_of`/`tone_of`/`details_of`/`rotation_of`/`overlay_of` return the
effective value and are read by `stage`/`prepare_overlay`/`pane_theta`/`export_pane`/
`view_command`; editing a synced pane's popup writes the shared set, and every
synced pane re-renders on its own because its effective `tone_sig` changed (no
texture nulling — see above). `set_sync_tone(false)` snapshots the
shared values in so nothing jumps. The first opened media seeds the shared set
(`add_pane`); a replayed `--tone`/`--detail`/`--rotate` is per-pane, so `apply_view_state`
unsyncs the panes it sets.

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

**Pane rotation (Alt + drag / Transformations slider).** Each pane carries a
`rotation` (degrees, -180..180) that **rides the Transformations sync** like tone /
details / overlay: `rotation_of(i)` returns the shared angle (`shared_rotation`) when the
pane is tone-synced, else its own, and `set_rotation(i, °)` writes whichever applies — so
editing one synced pane turns them all. **Alt + primary-drag** on a pane spins it to
follow the cursor's angle about the image centre (Photoshop-style, `rotate_drag` holding
the grab pivot + start angle), **snapped to the nearest degree**; the Transformations popup
also has a **Rotate** control: a **1°-step** drag bar plus a **typeable angle** field
(`rotation_edit`; a click selects the whole value so it can be typed over, committed on
Enter / focus loss) and a ⟲ reset. Rotation is applied
**at draw time** (the texture stays unrotated, so no re-render): `paint_rotated` draws the
image (and its overlay) as a textured mesh with the image-rect's four corners rotated about
its centre, clipped to the pane. Because the view is a **similarity** (uniform scale +
translate, no rotation), rotating in image space about the image centre equals rotating the
mapped screen point about the image-centre's screen position — so `rot_img_to_screen` /
`rot_screen_to_img` (used by the cursor dot, value readout, and the profile line — things
that track a specific source *pixel*) stay pixel-aligned with the drawn mesh. Export mirrors it:
`ExportPane.rotation` (radians) un-rotates each sampled point (`unrotate`) so a rendered/encoded
pane matches the rotated live view (`--rotate` round-trips it through the view command).

**Region selection is viewer-aligned, not image-aligned.** The **export crop** and the
**stats region** are both stored in the pane's *unrotated* view frame and converted with the
**plain view** (no rotation), via one shared helper `select_region_bounds` (used by
`screen_rect_to_image` and `finalize_region`): the released rectangle stays axis-aligned with
the **viewer** — exactly what the user dragged — rather than snapping to the (possibly rotated)
image axis. Their overlays draw back with the same plain view. The pane's rotation is re-applied
**downstream, once**: the export's `unrotate` maps each output pixel through the rotation (so a
rotated crop shows the rotated content, with the area outside the image left as transparent
**background**). Because the view is a pure similarity, on an **unrotated** pane this is the
plain rectangle exactly as before — the crop is then clamped to the image bounds (dropping the
background); a **rotated** crop is left un-clamped so it can include the background.

**Intensity-profile line (shift + right-drag).** Holding **Shift** while right-dragging
draws an editable **line** (`line_profile`, an image-space `{a, b}` like `stats_region`,
so it **replicates on every pane** and can be edited from any of them). `line_input`
(in `line_overlay_for_pane`, called from `draw_pane` and both A/B sides, right after the
stats overlay) hit-tests the press: near an endpoint → drag it (`LineGrab::Start/End`),
near the body → move the whole line (`Body`), else start a fresh line (`New`);
`region_input` returns early while Shift is held so the stats region doesn't grab the
same button. The line and its endpoint handles paint in **amber** (`LINE_COL`). The
**Line profile** tab (`app/profile.rs::draw_profile`) is a window that shows **only while a
line exists** — drawing one opens it, clearing it (or its **Clear line** button) closes it
(`update` gates the draw on `line_profile.is_some()`); it plots each **shown** media's
pixel **intensity** (only `visible_indices` — a Hidden pane draws no curve and no
legend entry; colour stays keyed on pane index so a media keeps its colour regardless
of which others are hidden) (`FrameData::intensity_at` — mono
value or mean of R/G/B) sampled along the line (`line_samples`, one point per line pixel,
`NaN`/break where a pane's frame doesn't cover it): **position on the x axis, value on the
y axis**, default range the samples' **min/max**. One coloured polyline per media
(`series_color`), value/position **ticks** (`nice_ticks`), and a **legend** of each media
name + colour underneath.

**Compute panes.** A *generated* pane whose image is derived from other panes. The
**toolbar** "Compute" button sets `pending_compute_create`; the deferred
`add_compute_pane` adds an **unconfigured** Compute pane. `draw_compute_ui` (a
top-left foreground `Area` over the pane) has two states keyed on `Compute.computed`:
while `false` it shows the **config form** (mode + source combos + a **Compute**
button); that button sets `pending_recompute` (run at the top of the next `update`,
before `refresh_textures`, so the result never flashes black — §13) → `recompute_pane`,
which on success sets `computed = true`, so the **result image** then shows with the
**Refresh** / **Save** / **Auto refresh** controls instead. `Pane.compute` holds the `kind`, source id(s), `computed`, and the
auto-refresh flag. `media::Reduce` modes:
- **Mean | Std** — `recompute_pane` → `compute_reduce` gathers **one** source's
  **resident** frames and calls `media::reduce_frames` (per-pixel/-channel, `f64`
  accumulation → `f32`).
- **Diff** — `compute_diff` takes **two** sources' *current* frames
  (`frame_disp`, both must be resident) and calls `media::diff_frames` (signed
  `A − B`, float). Sources may be stills; reductions need ≥2 frames
  (`compute_sources`).

Results become an `f32` `Media::still` (default tone Linear, clip on). **Auto refresh**
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
- **Region-limited render pipeline.** `compose` runs in two phases: **(1) `decode`** every
  pane's source frame (so all sizes are known), then **(2) `render`** each pane on **only
  the cropped region**. `ExportPlan::pane_boxes` computes, per pane, the axis-aligned
  bounding box *in the unrotated source image* of the pixels the output actually samples —
  by mapping the four corners of that pane's composition rectangle through
  `view.screen_to_img` + `unrotate` (the map is a pure affine, so the corner bound is exact
  for **any rotation**; a rotated crop yields the tight source rectangle covering its rotated
  region). `render` then `FrameData::crop`s to that box and runs the **whole** tone pipeline
  — LUT bounds (`clip_bounds`/`display_bounds`), the LUT render, **and the LUT_ALPHA /
  details operators** — on just that sub-frame, so a small crop never processes the full
  image (and the operators' auto-contrast is computed on the region). A full-frame box skips
  the copy (no regression for a full-view export); `cur_origin`/`cur_render_size` offset the
  sample lookup into the cropped buffer, while `cur_size` (full frame) still anchors the
  rotation centre and overlay mapping. A pane the output never samples isn't rendered at all.
- `ExportLayout = Grid(Vec<GridCell>) | Single | Ab`. `ExportPlan.compose(t)` maps each
  output pixel back through the pane's view (Grid via `GridCell`'s place→content remap),
  sampling **nearest** — upscaling to a larger output just replicates source pixels, never
  blends them. `start` offsets so output frame `t` = timeline `start+t`.
- **Region crop** is chosen in image space ("Select…" forces Single): a **right-drag**
  draws the crop (secondary-button edge detection in `region_overlay`, like the stats
  region) while **left-drag pans and the wheel zooms** so the user can move around first;
  `screen_rect_to_image` on release maps it to image space, applied to every pane as a
  cell of exactly the crop's pixel size. Closing the panel mid-selection (the toolbar
  toggle **or** the window's ✕) runs `cancel_region_select`, which clears
  `selecting_region` and restores the forced-Single mode — otherwise the flag stays
  stuck true and keeps suppressing pane interaction (rotate / reorder / focus).
- **Frame range:** "all", else inclusive `from/to`; **"Use loop range"** adopts the
  playback window but with the **end exclusive** (the loop's `[lo, hi]` plays through
  `hi`, but exporting it yields `lo..hi` — e.g. loop `[20, 40]` → 20 frames, not 21).
  A warning appears only when the **selected range** isn't fully discovered yet
  (`export_range_incomplete` — an explicit sub-range whose frames every participating
  sequence has already found needs no loading, so no warning even if some tail is still
  undiscovered), with **Load all** / **Load offsets** (the latter — headers only — is
  enough here, since export only needs the length, not resident frames) and a **Stop**
  while running.
  An export **Load all** arms `export_load_pending`: if it can't fully load because
  the frame cache is too small (`load_cache_exhausted`), a modal (`warn_popup`) on
  completion tells the user the whole sequence isn't resident — the length was still
  fully discovered, so the range is right and the encoder reads the rest from disk (§6).
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
  `--tone` (per-pane `linear|lutalpha`; `linearclip`/`clip` are accepted as
  deprecated aliases for `linear`), `--clip` (per-pane Linear clip: `off` or the
  per-tail percentile, e.g. `0.01,off,0.5`; omitted at each pane's depth default),
  `--detail` (per-pane `1`/`0`),
  `--show` (per-pane visibility), `--tsync` (per-pane Transformations-sync),
  `--rotate` (per-pane display rotation in degrees, `-180..180`), `--loop LO,HI`. Generated by the in-app "View cmd" window (`view_command`), applied
  after startup files load (`apply_view_state`). The window's **Copy to clipboard**
  button and a global **Ctrl+Shift+C** shortcut both route through `copy_view_command`
  (egui's `ctx.copy_text`, so it goes via eframe's clipboard backend on every platform).
  Only present flags override defaults,
  and `view_command` **omits any flag left at its default** to keep the line short; a
  restored `--zoom`/`--center` clears `needs_fit`. Only the *shared* view is captured.
- Positional args accept a **compact numbered-sequence token**
  `PREFIX%0Xu SUFFIX,START,END` (e.g. `sequences_%05u.tif,4,15`), expanded at
  launch. A bare path → `Single`; a token
  ≥2 files → `Sequence` opening as **one** pane (`.tif` run → `ConcatSeq`, else
  `FileSeq`). `token` is kept on the pane's `Source` so reload/round-trip work.
  Drag-and-drop / the file dialog only produce `Single`s.
- `--complete <word>` lists loadable completions (collapses numbered runs into the
  token); `--completions <bash|powershell>` prints a completer. `LOADABLE_EXTS =
  [tif,tiff,png,jpg,jpeg,bmp,webp]` is shared by the dialog and the filter.

---

## 12. Settings & persistence (`settings.rs`)

`Config { max_columns, ui_scale, cache_budget_mb, decode_threads, cursor_dot,
cpp_lib_dir, keybindings }` (`cpp_lib_dir` = the folder holding the proprietary
operator libraries, loaded at startup and auto-loaded when the folder changes
— §7 — with a Browse/paste field plus found/not-found and loaded indicators in
Settings; empty = the `LIBS` folder next to the cim executable, else by name via `LD_LIBRARY_PATH`. `decode_threads` = the background decode pool size, `0` =
auto — §5),
saved as JSON via `ProjectDirs("dev","cim","cim")` — Windows
`%APPDATA%\cim\cim\config\config.json`, Linux `~/.config/cim/cim.json`. Loaded on
start; **written only on an explicit "Save settings"** (never on exit). `config` is
edited live while `saved_config` holds the on-disk copy; Settings shows an **"Unsaved
changes"** warning whenever they differ (`config != saved_config`, needing `PartialEq`).
New `bool`/scalar fields take a `#[serde(default = …)]` so an older saved config still
loads.

`Action` = all bindable actions (view toggles, next/prev media & frame, fit/actual/
zoom, load all, open, toggle panels, play/pause, **reload focused / reload all / hide
media**, `SelectMedia(0..12)`).
`Keybindings` is a `BTreeMap<action_id, chord_string>` with unique bindings, where a
**`Chord`** is a key **plus optional Ctrl/Shift/Alt modifiers** (`ctrl` = egui's
cross-platform `command`). It serialises as a `Ctrl+Shift+Key` string, so an older
config storing a bare key name still parses (a no-modifier chord). Matching is
**exact** (`Chord::pressed` — key **and** modifier set), so `R` (reload focused) and
`Ctrl+R` (reload all) stay distinct; rebinding captures the key press together with
the modifiers held at that moment (`Chord::from_modifiers`, egui emits no Key event
for a bare modifier). Default `Reload focused = R`, `Reload all = Ctrl+R`, `Hide = H`.
The pane header also has a **⟳ Reload** button (left of Hide). New default bindings do
**not** retroactively apply to a saved config (shows `—` until rebound).
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
`shared_frame` (and any stale `play_prefetch`); `poll_watches` (reload any watched
pane whose source file changed and settled) then recompute any Compute pane (a deferred
`pending_recompute` button click, then `refresh_auto_compute`) — all **before**
`refresh_textures`, so a reloaded/recomputed texture (nulled by the reload/recompute)
re-renders and commits in the same lock-step group as the other panes, never drawn black
between the two; `refresh_textures`
(stage on-screen panes and, when all ready, flip them + commit a playback step — runs last so
it sees settled frame/tone state, just before drawing reads the textures); expire
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
  timeline at build time (press "Load all" or "Load offsets" first for a full export —
  offsets suffices, since export only needs the discovered length).

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

**Profiling the pipeline (`debug.rs`).** Launch with **`CIM_DEBUG=1`** to enable a
per-stage timing profiler and a **Debug** toolbar button (both hidden otherwise, so
there's zero cost in a normal run — `debug::enabled()` reads the env var once and every
record site is gated on it). Each stage on the read→display path records into a bounded
ring buffer (last ~120 samples → last/avg/min/max): **Decode** (read+decode, timed on the
decode worker and carried back on `Done.elapsed`), **LUT / tone render** and **Operators**
(LUT_ALPHA/details, split and timed on the render worker via `RenderDone.lut_time/ops_time`,
plus the synchronous cheap-pane LUT timed in `stage`), **Texture upload** (`ColorImage` build
+ GPU upload), and **Update** (the whole `update` CPU frame, excluding the GPU paint eframe
does after). The `⏱ Debug` window (`draw_debug`) tabulates them so the bottleneck stands out.

---

## 16. Testing

Inline `#[cfg(test)]`, run against **synthetic fixtures generated at test time**
(`src/testutil.rs` — multi-page u16 TIFFs with varying page sizes, PNG runs, a
hand-written 1-bit bilevel-mask TIFF); only the MP4 encode test skips when `ffmpeg`
is absent. Coverage: `cli` token expansion/grouping; `media` lazy length / probe
discovery / eviction, **LUT render matches the float reference** bit-for-bit,
mask/intensity renders, region stats + save round-trip; **percentile equivalence**
(whole-image == full-frame region, integer and float, with golden values);
`renderer` **worker output == plain LUT render** when no operator library is loaded;
`export` full compose→ffmpeg encode, **pixel-exact region crop** (incl. rotated),
**full-frame export == live LUT render**, content-only export (`content_region`
excludes background) + still background crop. The parity/equivalence tests are the
net that guards the unified `render_display` / `percentile_rect_*` paths (§7).

---

## 17. Conventions

- **Commits:** small, one concern; imperative summary + a short *why*. Committed
  directly to `main`.
- **Build target:** Windows, debug, during development.
- **Style:** match surrounding code (comment density, naming, `pub(super)` methods,
  free helpers in `app/mod.rs`).
- **Future media:** video (mp4/avi) slots in as another `Media` variant behind the
  same interface.
