# cim — Architecture & Reference

> **cim** ("Compare Images & Media") is a lossless side-by-side viewer for still
> images, multi-page TIFF sequences and videos, built with `egui`/`eframe`. It targets
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
  suite runs anywhere; the ffmpeg-dependent tests (MP4 encode, video decode —
  the latter generate a tiny `testsrc` clip with ffmpeg itself) skip gracefully
  when `ffmpeg` is absent.
- **CI:** `.github/workflows/build.yml` builds Windows + Linux (glibc 2.28 via
  Debian buster) release artifacts on `v*` tags.
- **Deps (`Cargo.toml`):** `eframe` 0.29, `image` 0.25, `tiff` 0.11, `rfd` 0.14,
  `serde`/`serde_json`, `directories` 5, `anyhow`, `libloading` 0.8 (runtime load
  of the optional proprietary C++ operators — **no** C++ compiler needed to build
  cim; see `INTEGRATION_CPP.md`), `rust-i18n` 4 (UI translations, §12). Export
  shells out to the **`ffmpeg` CLI**; video (mp4/avi) loading shells out to
  **`ffprobe`/`ffmpeg`** the same way (§3).
- **Embedded assets** (`assets/`, baked in via `include_bytes!`): `icon.png` (window
  icon) and `cimicons.ttf` (a Braille-block subset of DejaVu Sans, registered in
  `new` as a **fallback** font so glyphs the bundled faces lack — e.g. the `⠿`
  drag-handle grip — render instead of tofu).
- **Translations** (`locales/en.yml`, `locales/fr.yml`) are baked in by the
  `rust_i18n::i18n!` macro in `main.rs` — nothing to ship beside the executable (§12).
- **`help.md` is *not* embedded** — the toolbar's **Help** window reads it from disk
  (beside the executable, else the working dir) so it can be edited or replaced per
  deployment without a rebuild; CI ships it next to the binaries (§12).
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
  cli.rs         CLI: --help, shell completion, sequence-token expansion, and
                 directory expansion (a folder arg → one concatenated sequence of
                 its loadable files, alphabetical; `input_for_path` shares it with
                 drops/dialog).
  media/         Data model, split by concern (re-exported from mod.rs):
    mod.rs       FrameData/Samples core (accessors, crop), save_frame,
                 placeholder_frame.
    source.rs    Media (Still|TiffSeq|FileSeq|ConcatSeq|Video) + SeqCache +
                 DecodeReq: the source kinds behind one interface, length
                 discovery, LRU.
    loader.rs    load*/open*/decode* constructors, SeqReader (persistent TIFF
                 decoder), bilevel-mask bit handling.
    video.rs     Video via the ffmpeg CLI: ffprobe metadata (probe_video) and
                 VideoReader (persistent streaming ffmpeg child; §3).
    fastscan.rs  Fast scan (§4): measure a regular page stride from the first
                 two IFDs, then predict + validate + raw-decode page N in O(1)
                 (never trusted unvalidated; falls back to the chain walk).
                 Also the layout-free offset_jump/PageAnchor: pin one page's
                 byte offset so a reload lands back on it (§9).
    render.rs    Tone rendering: cached-LUT render (ToneLut + render_into*_lut /
                 _scaled / _gray_u16 / _cmap), mask/intensity overlay tints, display-bounds.
    stats.rs     Histograms, region stats/bounds, Compute ops (mean/std reductions,
                 add/sub of two frames).
    percentile.rs  The one per-tail percentile histogram scan (rect + fallback),
                 shared by whole-image auto-contrast and region tone.
  imageproc.rs   Runtime loader (libloading) for the proprietary C++ operators
                 (LUT_ALPHA, DETAILS_ENHANCED); C++ in cpp/ is built separately
                 into two .so, loaded by hard-coded name. PaneOps owns a pane's
                 per-operator instances (create/apply/destroy; 16-bit only) and
                 the shared render tail render_display; ops_active gates them.
  cpu.rs         The instance's CPU thread budget (§5.1): splits config.cpu_budget
                 between the decode pool and the rayon pool, and owns the latter.
  decoder.rs     Background decode thread pool (per-sequence persistent readers).
  offsets.rs     Off-UI-thread fast-offset scanner: measures a fast-scannable
                 sequence's page counts on open/reload (§4) without hitching paint.
  renderer.rs    Off-thread tone-render pool: builds the display RGBA (via
                 PaneOps::render_display) for heavy panes so the UI never blocks.
  gpu/           Optional GPU display path (§7.1), **shelved** behind CIM_GPU=1 (§7.2):
    mod.rs         GpuContext (wraps eframe's wgpu device), GpuTex (a texture egui
                   samples directly), toggle resolution + adapter probe.
    tonemap.rs     Resident VRAM sample buffers keyed by FrameData::uid, the
                   uploaded display table, and the compute dispatch.
    tone.wgsl      The u8 / u16 / f32 tone-map kernels.
  watcher.rs     Off-UI-thread source-file signer for the auto-reload watch
                 (sign_paths / FileWatcher, §9) — file I/O off the paint path.
  debug.rs       Opt-in pipeline profiler (CIM_DEBUG=1): per-stage timing rings.
  tone.rs        The display-bounds maths shared by the live view and the export
                 (§10's parity rule, made structural): pixel_bounds, frame_bounds,
                 clip_pct, uses_colormap, synced_index. Borrows only a frame plus
                 plain parameters, so both sides can call it without either's state.
  view.rs        ViewTransform: zoom/pan/fit math (screen <-> image space).
  palette.rs     Colour palettes (viridis/turbo/diverging) for the Colormap tone.
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
    compute.rs   Compute panes: reduce/add/sub, source graph (chaining +
                 cycle guard), recompute/auto-refresh/save.
    watch.rs     Auto-reload file watching (poll_watches / rebaseline_watch):
                 rate-limited requests + the debounce, signing done off-thread.
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
      options_popup.rs  The global Transformations panel (draw_tone_options — the place
                     to add a tone knob).
      region_stats.rs   Right-drag stats region + panel.
      line_profile.rs   Shift+right-drag profile line overlay.
      compute_ui.rs     In-pane Compute controls.
    panels.rs    Toolbar, media manager (drag the ⠿ handle to reorder rows via
                 `drop_target` + `remap_move`), settings, view-command, frame bar.
    profile.rs   The Line-profile plot window.
    export_ui.rs Export panel UI + building ExportPlan from live app state.
    help.rs      The Help window: loads the **external** `help.md` and renders a
                 small Markdown subset (§12).
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
- **Parallel analytic scans.** The whole-image passes — `value_extent`,
  `histogram_display`, both `percentile_rect_*` (via the shared `scan_rows`), and the
  Compute ops `reduce_frames` / `combine_frames` — split across cores past
  `media::PAR_MIN_SCAN_PX` (256k px; ~3–3.8× on 4 cores). Each is a map-reduce, so unlike
  the render (`render::PAR_MIN_PX`, disjoint output slices, no merge) it pays a per-job
  accumulator merge — hence the lower threshold, and `scan_band` pinning a *binning* scan
  to one job per thread (a 16-bit histogram is 65536 bins to merge, so rayon's default
  splitting would cost more than the scan).
  **Results are identical to the serial ones, not merely close:** histogram counts merge
  by exact integer addition (`merge_hist`), extents by associative min/max (NaN fails both
  comparisons, so it is skipped either way), and `reduce_frames` splits by **sample index**
  rather than by frame — each output sample still sums its stack in the original order, so
  the non-associative `f64` accumulation reproduces bit for bit. The `*_is_independent_of_
  the_split` tests assert equality across 1/2/3/4/8-thread pools.
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

### `Media` = `Still | TiffSeq | FileSeq | ConcatSeq | Video`
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
- `Video` (`VideoSeq`, `media/video.rs`) — one **mp4/avi** file, decoded through
  the **ffmpeg CLI** (like export encodes; no decode crate). `open_video` reads
  length/size/fps with **ffprobe** up front (`probe_video` — `nb_frames`, else a
  `-count_packets` pass, else duration×fps) and eagerly decodes frame 0, so like
  a `FileSeq` it is always `at_end` and only ever decoded, never probed (no
  offset scan, no fastscan). Frames come from a persistent **`VideoReader`**: a
  long-lived `ffmpeg … -f rawvideo pipe:1` child; sequential decodes read the
  next frame off the pipe, a non-sequential index respawns the child with an
  accurate input-side `-ss` (`seek_seconds` — midpoint of the preceding frame
  interval). Always **8-bit** (`rgb24`, or `gray` for grayscale sources — mono
  keeps the Colormap tone usable; `hi_depth` false, never a mask); frame↔time
  assumes **CFR** (avg rate), so a VFR file may land ±1 frame on seeks. Missing
  ffmpeg/ffprobe → a clear open error / per-pane frame error, never a crash.
  `DecodeReq::Video { path, frame }` decodes via the pool's per-pane reader.

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
- **The frontier is probed a run of pages at a time** (`CimApp::probe_ahead`,
  `FRONTIER_PROBES`), not one per update. Discovery must stay serial — `note_len`
  grows only at `idx == len`, so page N+1 isn't confirmed until N is — but it need
  not cost a **UI round trip** per page, which one-probe-per-update did: request →
  worker → `request_repaint` → drain → `note_len` → next update. That loop latency,
  *not* decode speed, is what capped playback and "Load all" of an undiscovered
  sequence (~20 fps against 60+ once the length was known: the decode pool ran dry
  between frames, where a known length lets `drive_eager` queue every missing frame
  at once). While playing, `ensure_lookahead` therefore keeps a **whole prefetch
  window** discovered ahead (`margin = ff × FRONTIER_PROBES + 1`) — `prefetch_playback`
  never queues past the known length, so a frontier one or two pages out left it
  able to see exactly one frame.
  Over-probing is safe **by construction**: a result landing away from the frontier
  is dropped and re-issued later — `note_frontier` ignores `idx != len`, and
  `Media::frontier_ended(idx)` follows the same rule (it takes the index for exactly
  this reason; without it a miss probed past the real end could land while earlier
  pages were still in flight and fix the length short of pages that exist). The
  frontier is extended by **probing, never decoding** — including at `ff == 1` —
  since a decode landing past the frontier is dropped by `insert`, throwing away a
  whole frame read. A `ConcatSeq` can't be probed ahead at all (an undiscovered
  global index has no known `(file, page)` until the ones before it land, so
  `job` returns `None` past the frontier) and quietly does the single probe as before.
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
  true (still-discovering, target ≥ its frontier), so `ensure_lookahead` **probes**
  that pane forward (metadata only, no full decode of the pages in between) and
  `refresh_textures` **skips staging it** — it holds its last committed frame (blank if
  new) instead of flipping through 0…N — until its own length passes the target, then it
  stages just that frame. The `update` clamp pins `shared_frame` to the **loop-driving**
  pane's (`loop_control`) length, so that pane is never "catching up" (that's `pending_seek`'s job); this
  covers the *other*, shorter/newer synced panes. It applies **while playing** too — for
  the pane a `playback_limit` hold (§8) is waiting on, so the pages it must cross are
  crossed by header rather than decoded one per frame. Both this and `pending_seek` **repaint
  immediately** while riding the frontier, so discovery runs as fast as probes land rather
  than one per 30 fps decode-poll tick.
- **Per-frame resolution:** `disp_size(i)` uses the resident frame's own size
  (page-0 fallback) so drawing/readout don't stretch or go out of bounds.
- **`ConcatSeq`** reuses all of this: a frontier miss rolls to the next file's
  page 0, so the run discovers as one seamless length (∑ page counts) with no
  concat-specific code in `drive_seek`/lookahead/playback.
- **Fast scan (`media/fastscan.rs`)** — an O(1) shortcut past all of the above for
  **regularly laid out** TIFFs (uniform, uncompressed pages at a fixed byte
  stride — the `tifffile`/ImageJ capture case), classic **or BigTIFF** (64-bit
  offsets / wider IFDs, branched on `FastScan::big` — the ≥4 GiB case where the
  shortcut matters most). `FastScan::open` measures the
  stride from the first two IFDs (with a long list of rejections: compression,
  tiling, planar, differing pages, irregular placement…); page N is then
  *predicted* at `ifd0 + N×stride` and **validated before trust** (template
  match tag-for-tag, strip data on the same stride, predecessor's next-IFD
  pointer landing on it), so a wrong frame can never be shown — a failed
  validation just falls back to the ordinary chain walk.

  Reads go through `read_at`, a **positional** read (`read_exact_at`/`seek_read`):
  one syscall, no file cursor, and `&File` rather than `&mut File`. A page's pixels
  are fetched in **runs** — `decode_strips` merges strips already contiguous in the
  file into a single read, so a 4096²×u16 page costs ~1 read instead of ~68 (§15).
  `raw` fills in strip order, so a contiguous source run lands in a contiguous
  destination slice: same bytes, no extra copy. **Contiguous runs only** — bridging a
  gap would fetch bytes we discard, which on a shared mount spends someone else's
  bandwidth to save our round trip.

  Used three ways:
  - every `SeqReader` consults a measured layout so far probes/decodes skip the
    chain walk (falling back when a prediction fails);
  - **Load offsets fast** (frame-bar button, right of *Load offsets*, shown only
    when the layout is measurable) finds each file's exact page count by
    binary-searching the largest validating page (~log₂(pages) header reads/file
    — `FastScan::page_count`), then marks the whole timeline known and ended
    (`SeqCache::note_len_to` + `ConcatSeq::set_full_layout`) so any index is
    instantly seekable — `media::fast_load_offsets`, run synchronously; a pane
    it can't handle falls back to the ordinary `Eager::Offsets` discovery;
  - **automatically on open (and reload)**, off the UI thread. `add_pane` /
    `reload` call `request_offset_scan`, which — for a still-discovering TIFF
    sequence (`media::offset_paths` is `Some`) — hands the file paths to a
    dedicated single worker thread (`crate::offsets::OffsetScanner`, mirroring the
    decode/render pools: `ctx.request_repaint` on completion, drained by
    `pump_offset_scans`). The scan is split from the mutation on purpose:
    `media::scan_offset_counts(paths)` (the I/O-bound binary search) runs on the
    worker and returns only a `Vec<usize>` of per-file page counts — never the
    pane's `Media`, which stays UI-thread-owned — and `media::apply_offset_counts`
    applies them on drain. The files themselves are measured **up to `SCAN_FANOUT`
    (8) at a time** (`scan_counts_batch`, scoped threads): unlike the frame path
    this is *header* I/O, latency-bound rather than bandwidth-bound, so overlapping
    it costs a shared server cheap metadata ops rather than a share of the link
    (§15). Each worker writes only its own slice of the output, so **file order is
    structural** rather than something the join restores; results stay **per file**
    (`Vec<Result<usize, _>>`, not one `Result`) because `fast_jump` stops at the
    file holding its target and must not be cancelled by an unscannable file
    further along that it never reaches — it measures in bounded batches, so only
    the tail of the last batch can be surplus. A **dedicated thread, not the decode pool**: `FastScan`
    uses its own file handle (nothing to gain from the pool's persistent
    `SeqReader` cache) and a whole-sequence count vector doesn't fit the pool's
    per-frame `Decoded` result, and this way an open's scan never occupies a
    worker that should be decoding the first visible frame. A **generation** tags
    each scan (`Pane.offset_scan` = the in-flight gen, `CimApp.offset_gen` the
    counter): pane ids are stable across reload, so a scan returning after a reload
    is recognised as stale and dropped. A layout that isn't fast-scannable just
    `Err`s on the worker and is left to lazy discovery — **no** classic
    `Eager::Offsets` probe storm is auto-started;
  - the **frame readout** (typed index) tries `media::fast_jump` first — validate
    + raw-decode that one frame at its predicted position and grow the known
    length through it in one step, no intervening discovery — then falls back to
    riding the frontier (`seek_to`/`pending_seek`) when the prediction can't be
    made. A `ConcatSeq` works throughout: files before the target are page-counted
    by binary search and the global map extended (`ConcatSeq::extend_known` /
    `set_full_layout`), always **verified against whatever prefix ordinary
    discovery already built** (a disagreement refuses the fast path, nothing
    mutated). The *Load offsets* hover carries the rejection reason when the fast
    path isn't available; availability is cached per pane (`Pane.fast_jump`, reset
    on reload).

**Offset-anchored jump (`media::offset_jump` / `PageAnchor`)** — the layout-free
sibling of all of the above, used by **reload** (§9). It pins *one* page's byte offset
(plus its header, its predecessor, and the file's length/first-IFD offset) instead of
deriving every page's position from a stride, so it also works on the files fast scan
rejects. A remembered anchor that still validates makes a reload two header reads; when
it doesn't, the chain is walked once (headers only, `MAX_ANCHOR_WALK`) to rebuild it.
Same invariant as everything else here: nothing is trusted unvalidated, and a failed
check falls back rather than showing a wrong frame.

---

## 5. Background decode pool (`decoder.rs`)

- `BackgroundDecoder::new(threads)` shares one `mpsc` job queue behind a `Mutex`
  (locked only for the hand-off). The thread count is `CimApp::resolve_decode_threads`
  — this pool's share of the **CPU budget** (§5.1). A budget change is **live-applied**
  in `update` by rebuilding the pool (and clearing `inflight`, since jobs queued on the
  old pool won't land on the new one — they re-request; persistent readers reopen on
  demand).
- **Jobs addressed by stable pane `id`**, not Vec index, so results land after
  reorder/close.
- **Persistent readers:** `readers: HashMap<(pane id, file), Arc<Mutex<Reader>>>`,
  where `Reader = Tiff(SeqReader) | Video(VideoReader)` (a key only ever maps to
  one kind — reload/close `forget` first). A `Tiff`/`Video` job locks the map to
  get/open the file's reader, then locks the reader to decode. Different files
  decode in parallel; pages/frames of one file serialise (a video's sequential
  requests then read straight off the streaming pipe). `forget(id)` drops all of
  a pane's readers (killing a video's ffmpeg child via `Drop`). A `File` job has
  no persistent reader.
- `request` enqueues; `drain()` collects finished `Done` non-blocking each update.
  `Done.result: Result<Decoded>` — `Decoded::Frame` a decoded frame, `Decoded::Exists`
  a **metadata-only** frontier probe hit (`DecodeReq::Tiff { probe: true }`, page exists
  but not decoded — §4), `Decoded::End` past-end, `Err` failure.
- App side (`app/decode.rs`): `inflight: HashSet<(id, frame)>` dedupes both `request`
  and `probe`; `pump_decoder` drains (insert + `touch`, or `note_frontier` for a probe
  hit, or `frontier_ended`, or set pane `error`).
- **Playback prefetch (`prefetch_playback`).** While playing, each on-screen pane (plus
  the loop-driving pane) pre-decodes the next `PLAY_PREFETCH` (3) frames along the loop window
  (same walk as `advance_playback`; wraps when looping), so playback overlaps decode with
  display instead of stalling on decode latency at a not-yet-resident frame — the win grows
  with pane count, since the lock-step commit waits for the slowest pane. Requests dedupe
  via `inflight` and never go past the known length (frontier discovery stays with
  `ensure_lookahead`), so re-running it every update is cheap.
  - **Fair dispatch:** each pane's next-frame list is flattened **round-robin by
    distance** (`interleave_prefetch` — every pane's `+1`, then every pane's `+2`, …), so
    one pane's whole burst can't front-load the single decode queue and starve the pane
    that gates the commit.
  - **Adaptive depth:** `prefetch_depth` scales from a fixed floor (`PLAY_PREFETCH`) up to
    a cap using an always-on decode-latency **EMA** (`decode_ema_secs`, maintained in
    `pump_decoder` independent of `CIM_DEBUG`), the displayed-pane count, and the pool
    size — deeper when decode is slow / many panes, shallower when cheap.

### 5.1 CPU thread budget (`cpu.rs`)

`Config.cpu_budget` is the **total** worker threads this instance may run across the
two *shared* pools, split by `cpu::split(n) -> (decode, rayon)`: decode takes a
quarter, floored at 2 and capped at 8 (decoding is as much I/O as CPU, and on a shared
mount extra readers crowd out other users); rayon takes the rest. Default **16**
(→ 4 + 12), slider range `cpu::MIN` 4 … `cpu::MAX` 64. Because it is a *total*, the
number in Settings is the number of threads the instance actually runs — the point of
the setting on a shared host. Live-applied, so a user can raise it for one heavy export
and drop it back without restarting.

**Per-pane render workers sit outside the budget** (one per open pane, §7): each is
idle unless its pane re-renders, and tying them to the budget would make opening a pane
silently slow decoding.

**Why it exists.** Rayon's *global* pool sizes itself to the machine, so before this a
64-core host gave one instance 64 threads no matter how the rest was capped — the
parallel render / composite / scans (§7, §14, `media`) all ran on it. `cpu::install(f)`
runs `f` on the budgeted pool instead; rayon resolves nested parallelism against the
pool the caller is in, so **install at thread boundaries, not at `par_iter` call
sites** — the wrapper then covers parallel calls added later, which is what stops the
cap leaking. Current install points: the render worker's per-job body (`renderer.rs`),
the export composer (`export_ui::run_export`), and the UI thread's four parallel entry
points (`ensure_pane_histogram`, the synchronous `stage` render, `own_tone_bounds`,
`ensure_region_stats`). The UI thread can't be wrapped wholesale — `install` runs its
closure *on a pool thread*, and eframe needs the main thread for GL.

`set_budget` swaps an `Arc<ThreadPool>` behind an `RwLock`; `install` clones the `Arc`
and **releases the lock before running**, so a resize can't deadlock against a long
render, and a replaced pool lives until its last in-flight job finishes. Sizing helpers
that read `rayon::current_num_threads` (`media::scan_band`) see the budgeted count
inside `install`, so they adapt on their own.

Replaces the old `decode_threads` knob, which capped only the decode pool. An old
config's `decode_threads` is an unknown field and simply ignored, so an instance that
had capped it comes back at the default budget.

---

## 6. Cache memory budget / LRU (`app/decode.rs::enforce_cache_budget`)

Frames are held at native bit depth and never freed by decode alone. Guard:

- `CimApp::cache_budget_bytes()` = `config.cache_budget_mb` (**default 1.5 GiB**,
  adjustable via the **Frame cache** slider in Settings, 128 MiB–32 GiB).
- `clock` increments each update; frames are `touch`ed on decode and on display →
  LRU recency. When `resident_bytes()` exceeds budget, evict the oldest frames that
  are **not currently shown** (each pane's `frame_disp(i)` is protected) until under.
  Eviction is **incremental**: each `SeqCache` keeps its resident frames in a
  recency-ordered `BTreeSet` (maintained in O(log n) by `insert`/`touch`/`evict`), and
  the budget merges per-pane `lru_evictable` peeks to pop the globally oldest — so it
  never scans/sorts the thousands of known-but-non-resident slots each over-budget tick
  (the steady-state during multi-sequence playback at the cache ceiling).
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

`render_into` (`media/render.rs`): **U8/U16** build a value-keyed **LUT** (≤ 64 Ki)
then table-look-up per pixel; **F32** maps arithmetically. Mono replicates grey
across R/G/B; alpha = 255. The LUT is a reusable **`ToneLut`** (keyed on
`(lo, hi, mask, entries)` — and a parallel RGB table for Colormap), so a fixed-tone
playback run **reuses one table across frames** instead of rebuilding 64 Ki entries
each frame (the dominant per-frame CPU on a large integer image). Each pane owns its
`ToneLut`: the synchronous stage (`PaneTex.lut`), the render worker (`renderer::Worker`),
and export (`ExportPane`) each reuse one via the `_lut` render variants; a heavily
**decimated** small output skips the table and maps arithmetically. The plain
`render_into`/`_scaled` are convenience wrappers building a throwaway `ToneLut`.

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
- **Colormap** — false-colour a **mono** frame through a palette (`crate::palette`:
  viridis / turbo / diverging), using the **same window/clip bounds as Linear**; a
  display-only tone (no operators), rendered synchronously via `render_into_scaled_cmap`
  (a per-value RGB table in the pane's `ToneLut`). Multi-channel frames fall back to the
  plain render; each texel is still one true source sample (only its colour is the
  palette). The diverging ramp suits a signed **Sub** Compute pane (zero → white).

Plus a per-pane **Share clip** toggle (`ToneOptions.share_clip`) that locks the pane's
display bounds to the **Control** media's own `[lo,hi]` (`control_clip_bounds` — the
Control pane's clip / full-range map on its current frame) instead of computing its own,
for any non-LUT_ALPHA tone, so panes are **locked to identical display bounds** and real
intensity differences show as brightness rather than being hidden by per-pane
auto-normalisation. `tone_bounds` splits into `own_tone_bounds` (a pane's own clip / region
bounds — `tone::clip_pct` + `tone_region` + `tone::frame_bounds`, §2) and the Share-clip path
(which reads the Control's `own_tone_bounds`, so it can't
recurse); the effective bounds move with the Control pane's frame/clip, so `tone_sig` folds
in the Control frame's identity (`control_frame_key` — pane index + frame `Arc` pointer)
plus its clip/region inputs (cheap — never the computed percentile). Edited in the popup's
**Share clip** row (greys out the clip when on), and round-tripped via `--share-clip`. For
export the bounds are **recomputed per exported frame** (not frozen): a share-clip
`ExportPane` carries a `share_clip` flag, and the plan holds one `ExportPlan::control`
(`ControlBounds` — the Control media's source + its clip/region snapshot, both taken through
`tone::clip_pct` / `tone_region`); `compose(t)` decodes the Control's frame for `t`, computes
its bounds with the same `tone::frame_bounds` the live path calls, and pushes that one shared
window onto every share-clip pane — so an animated Control is tracked exactly as the live view
tracks it.

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
**small or decimated plain-LUT renders (Linear, masks, Colormap) stay synchronous**
(cheap `render_into_scaled`), while **LUT_ALPHA / details on a single-channel U16
frame — and any plain-LUT render of a large (`ASYNC_RENDER_PIXELS`, ~1 MP)
full-resolution (`step == 1`) non-Colormap frame** — go off-thread to
`render_display` (the worker's plain-LUT path is pixel-identical by test): a big
synchronous LUT render is itself tens of milliseconds, and on the UI thread it
blocked a whole update — a visible hitch whenever playback stepped while the user
panned. The worker also does the `ColorImage::from_rgba_unmultiplied` conversion (a
full-buffer copy, several ms on a large frame) and hands back a ready `ColorImage`
(`RenderDone.image`), so the UI thread only queues the texture delta (`tex.set`). The off-thread route is
gated to `step == 1` because the worker renders full-resolution only — a `step > 1`
result would never match the commit's step check. The export worker honours the pane's **clip toggle and
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
panes. **An export crop drives the same thing:** while the Export panel is open and a
crop is set, every non-LUT_ALPHA pane's bounds come from `export.region` (taking
precedence over `stats_region`), so the live view previews the region-restricted tone
the export composites; `tone_sig` folds the crop rect in so panes re-render when it
changes/clears.

That precedence — **export crop → pinned stats region → whole frame**, never for
LUT_ALPHA — is `CimApp::tone_region(idx)`, the one place it is stated. It is *policy*;
the maths it feeds is `tone::frame_bounds` (§2). The export snapshots the method's
result into `ExportPane.region`, so a region-pinned pane exports with the bounds it
displays (§10) — it previously had no region on the export side at all.

**Texture filtering:** always **nearest**, at every zoom, both magnification and
minification (`TextureOptions::NEAREST`). The tool is pixel-accurate — an on-screen
pixel must be a true source sample, never a blend — so there is no interpolation option
anywhere (display or export).

### 7.1 GPU display path (`gpu/`, optional — **shelved**, see 7.2)

**What it is.** The path is **opt-in and currently hidden**: `config.hardware_accel` (the
*Hardware acceleration* checkbox in Settings) is off by default, and the checkbox is only
shown — and `wants_gpu` only ever returns true — when the app was launched with
**`CIM_GPU=1`** (`gpu::exposed`). An ordinary run is the CPU
path exactly as it was before this module existed. When it is on *and* the machine has a
hardware adapter (`gpu::wants_gpu`), `main` asks eframe for the **wgpu** renderer
instead of glow, `CimApp` wraps eframe's own device
(`gpu::GpuContext::from_render_state`), and `stage` hands the large full-resolution
renders to `gpu::GpuToneMapper` instead of the render pool. The toned pixels are written
into a `wgpu::Texture` registered with egui (`gpu::GpuTex`, `CachedTex.image ==
TexImage::Native`), so they never enter system memory.

**Why (and why not just "the CPU map, elsewhere").** The CPU tone map is already
row-parallel and near memory bandwidth, so a compute pass that reads its result back
saves nothing — the readback costs what the map saved. The gain is **residency**: a
frame's samples are uploaded once (keyed by the process-unique `FrameData::uid`, since a
pointer would alias after an LRU eviction) and stay in VRAM, so **re-toning** it —
dragging the contrast slider on a 4096² image — is a table upload plus a dispatch instead
of a full-image map plus a full-image texture upload. Playback gains little; that is
expected, and the sample upload is still half the bytes of the RGBA one it replaces.

**Exactness.** The GPU is held to the same bar as the CPU render paths, not a looser one.
Integer sources do **no arithmetic on the GPU**: they index the very table `ToneLut`
built on the CPU (`FrameData::tone_table_rgba`), with the mask rule and the Colormap
palette already folded in, so they are bit-identical by construction. The compute pass
writes a **buffer** which is byte-copied into the texture, rather than writing a storage
texture — that would have inserted an f32→unorm8 quantisation whose rounding the specs
pin only to within 0.6 ULP. Only float sources compute anything, mirroring `map_u8` term
for term; they are tested to ±1 code value (denormal flushing) with NaN/±∞ pinned
exactly. Filtering stays **NEAREST**, as everywhere else.

**Scope.** Only `step == 1` renders of frames ≥ `ASYNC_RENDER_PIXELS` — exactly the
renders the CPU path already judged too big for the UI thread. Panes running the
proprietary C++ operators are excluded outright (closed-source CPU code owned by the
pane's render thread). Decode, the export composite and the Compute reductions are
**deliberately** not on the GPU — see the `gpu` module docs for the reasoning on each,
notably that Mean/Std accumulate in `f64` (which WGSL has no equivalent for) and that
Add/Sub would lose on the readback, their float result being larger than the inputs.

**Backends: `PRIMARY`, never GL** (`gpu::BACKENDS`, handed to *both* the startup probe and
eframe's `WgpuConfiguration`, so the two cannot disagree). A run without acceleration is
already on **glow**, eframe's own OpenGL renderer and the tested path for every VNC /
software-GL machine — wgpu's GLES backend would be a second, worse OpenGL, and it
`unwrap()`s `eglMakeCurrent`, so a display that answers `BadAccess` takes the process down
inside eframe's setup before any fallback here can run. No Vulkan adapter is therefore the
same outcome as no card at all: the CPU path.

**The probe can be wrong, so the start is retried.** The probe has no window, so it can
only answer "this machine has a Vulkan device", not "that device can present to the window
this app is about to open" — a remote / VNC / headless-X session is where those differ. And
eframe does **not** fall back on its own: `run_native` dispatches to `run_wgpu` and returns
its error. So `main::run` is a function taking the renderer, called twice: the wgpu attempt
first and, on `Err`, the whole run again on glow (a line on stderr, and under `CIM_DEBUG` a
log). Hence `cli::Input` / `cli::ViewState` are `Clone` — eframe's app creator is `FnOnce`,
so the second attempt needs its own copy.

**The toned texture is `Rgba8UnormSrgb`** (`GpuTex::FORMAT`), which
`register_native_texture` requires: egui's own managed textures are sRGB and its shader
assumes what it samples is already linear. A plain `Rgba8Unorm` holds the identical bytes
and still draws, but they are then read as linear and gamma-encoded a second time — the
GPU pane looks lighter and flatter than the CPU render of the same pixels, i.e. like a
different tone window, and only above the decimation threshold where the GPU takes over.

**Falling back is the normal case, not the error case.** A machine with no adapter never
builds a context, and a software adapter (llvmpipe) is refused outright — it is slower
than the CPU path it would replace, so taking it would make the toggle a pessimisation.
Any failure — a frame past the device's buffer limits, a lost device —
drops `CimApp.gpu` for the rest of the session, logs under `CIM_DEBUG`, and falls through
to the CPU render *in the same call*. **glow remains the default renderer** and is what
every run with the toggle off — the default — uses, unchanged.

### 7.2 Why the GPU path is shelved (and what was measured)

Kept compiled, tested and documented rather than deleted, behind `CIM_GPU=1`, so the work
can be picked back up. On the deployment it was built for — an **NVIDIA card driven over
VNC** — it measured as a loss on every axis that matters. What was actually found, because
the conclusion is about the *display stack*, not about the tone map:

- **The tone map itself is excellent.** Re-toning a frame already resident in VRAM is
  **under 1 ms**, against ~7 ms for the CPU render. Residency does exactly what §7.1 claims.
- **The frame rate never sees it.** The wgpu renderer costs more per frame than glow does
  there whatever the tone map does, so the win doesn't reach the update.
- **It tears.** A seam between two halves of the window showing different frames — and it
  happens while merely *panning a still frame*, where `stage` early-outs and no render is
  dispatched at all. So it is the Vulkan → X → VNC presentation path, not anything this
  module writes. `PresentMode` is already `AutoVsync` (FIFO) and
  `desired_maximum_frame_latency: Some(1)` changed nothing.
- **It cannot be moved off the UI thread there.** Running the tone map on the pane's render
  worker — the obvious fix for it being synchronous — made a 4.5 ms render take **170 ms**,
  on a *single* pane. The device is monopolised by a presentation slow enough that any
  concurrent use of it queues behind. (Committed and reverted; the reverted commit is worth
  reading before trying it again.)

None of this rules the path out on a **local display**, which is where it should be
evaluated next; that is what the env var is for. Note also that the interaction the path
exists to accelerate — dragging the clip percentile — turned out to be gated by a CPU
percentile scan in front of the render, in *both* modes (§7.3), so the GPU's sub-millisecond
re-tone was never what the user was waiting on.

### 7.3 Percentile histogram memoization (`media/percentile.rs`)

`stage` calls `tone_bounds` → `own_tone_bounds` **on the UI thread** every update, and
`clip_bounds` memoizes only the default 0.01% — "any other percentile is computed fresh".
Fresh meant re-binning the whole image, so dragging the clip slider scanned ~16.7 M samples
per update on a 4096² frame: ~12 ms of `update`, in CPU and GPU modes alike.

The histogram is a function of the **frame alone**; only the walk over it depends on the
percentile. So the binning is split out (`bin_rect_int` / `bin_rect_float`) and memoized
per frame (`FrameData::hist_int` / `hist_float`, `OnceLock` beside `bounds_full`/`bounds_clip`),
turning every percentile after the first into a walk over 64 Ki bins instead of a scan over
however many million samples. **Whole-image rectangles only** (`covers_frame`) — a right-drag
region bins its own pixels, and is small; the float table is additionally only reused when the
call's `extent` is the frame's own, since the bin edges depend on it.

Bounded on purpose: the integer table is kept only when it is ≤ 1/`HIST_CACHE_RATIO` (1/16) of
the frame's sample bytes, so it is noise for the large frames where the scan is expensive and
skipped for the small ones where the scan is already cheap — worst case a 6.25% overshoot of
`cache_budget_mb`. The float table is a flat 16 KiB (`FLOAT_BINS` = 4096) and always kept.
Results are **identical, not close**: both paths call the same binner, and
`the_memoized_histogram_changes_no_bounds` holds a reused frame against a freshly built one
across a spread of percentiles.

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

The **Control** pane (manager's **Control** selector) has two roles: it is the shared
**clip-bounds source** for any *Share clip* pane (§7) — so it may be **any** media — and,
when it's a sequence, it also **drives the loop**. Because a still can now be the Control,
the loop driver is *derived*: `loop_control()` = the Control pane when it's a sequence,
else the first sequence (a still Control supplies the shared bounds but can't drive the
loop). `timeline_len()` / `current_at_end()` and the frontier walks
(`ensure_lookahead`/`prefetch_playback`/`drive_seek`) all read `loop_control()`, while the
scrubber/transport show it too. `ensure_control` now only **clamps** `control` in range (it
no longer repoints onto a sequence). The Control pane is **separate from `current`** (the
focused pane for Single/keyboard/tint), so viewing a still doesn't hijack playback. Picking
a new Control drops the `loop_range` only when `loop_control()` actually changes.

Playback loops over a **window** `loop_bounds(len)` — a user sub-range (`loop_range`,
set by dragging the scrubber brackets; `None` = whole sequence). A full range with an
undiscovered end holds at the frontier rather than wrapping; a sub-range wraps/stops
immediately.

**The frontier playback holds at is the *slowest* one on screen** (`playback_limit`,
used by `advance_playback` **and** `prefetch_playback` so the two agree): the shared
timeline's length/end (`timeline_len` / `current_at_end`), lowered by any *other*
displayed, temporally-synced sequence that is **still discovering**. Otherwise a second
sequence opened next to an already-discovered one was left behind — the timeline ran at
the fast pane's pace while the new pane's frontier only crept forward as it decoded, and
`frame_disp` clamped it to its last discovered frame, so the panes drifted onto
*different* frames while appearing to play together. Only a **still-discovering** pane
holds it back: a genuinely shorter, fully-discovered sequence keeps holding on its last
frame as before, and an errored one (discovery stopped) never stalls playback. The pane
being waited on catches up by **probe**, not decode (`catching_up`, §4), and a
fast-scannable one usually needs no wait at all — the background offset scan (§4)
completes its length outright. **The manual next/previous-frame controls obey the same window**
(`apply_action(NextFrame/PrevFrame)`, shared by the keys, the Ctrl+wheel scrub, and the
frame-bar Prev/Next buttons via `ui.ctx()`): stepping inside `[lo, hi]` moves one frame;
at an edge a sub-range wraps to the other edge, a full range wraps only once the real end
is known (else holds at the frontier). `draw_scrubber` shades resident frames (contiguous runs merged), dims
outside the window, and draws the brackets. `advance_playback` accumulates
**wall-clock time** (`i.time` deltas via `Playback.last_tick` — **never
`stable_dt`**: egui substitutes a fixed `predicted_dt` of 1/60 s for the real
elapsed time on any frame woken by a *delayed* repaint request, i.e. every paced
`request_repaint_after` wake — which silently ran playback at a fraction of the
requested fps unless input events kept the dts real), steps at `fps` carrying the
overshoot into the next interval (capped at one step, so lateness doesn't compound
into a rate error but a stall can't burst), and advances unsynced panes
independently. With a
**fast-forward stride** (`fast_forward` > 1, §6) it steps by `fast_forward` frames
(clamped to the window end), skimming those in between; `prefetch_playback` strides to
match and `ensure_lookahead` probes (headers) rather than decoding the jumped-over
frontier frames, so playback skims a big sequence without reading every frame.

**Render-gated playback (`play_prefetch`).** Playback does **not** bump `shared_frame`
directly. When the accumulator is due, `advance_playback` picks the next frame and parks
it in `play_prefetch` (the candidate next shared frame), then stages the panes toward it;
`refresh_textures` advances `shared_frame` to it only on the commit — i.e. once **every**
on-screen pane has that frame ready. While a prefetch is in flight the accumulator **keeps
counting real time but is capped at one `step`**: the gate's own latency doesn't stretch
the frame interval (the next frame is due `step` after this one *fired*, not after it
committed), yet a slow proprietary operator still **paces** playback — at most one frame
fires the moment a long gate lands, never a burst — instead of the frame counter racing
ahead of the image. `play_prefetch` is cleared (playback step abandoned) by
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

Per pane: the image fills the **whole cell**; the header (top, `HEADER_H`) and footer
(bottom, `footer_area`/`FOOTER_H`) are opaque strips floating **over** the image's
edges, so showing/hiding them never moves the image. All chrome — these pane bars
plus the global toolbar and frame bar — hides together via **`Action::ToggleChrome`**
(default `T`, transient, always back on at startup) for an image-only view; every
shortcut still works while hidden (old configs' `toggle_headers` binding is migrated
on load). While a shown bar is under the cursor (`over_chrome`) new pane
interactions (zoom/rotate/reorder/focus) are suppressed so the bar owns the input.
`draw_header` (buttons,
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
shows the shared position with **both** A and B values, each preceded by its own native
format (`kind_label`, shared with `draw_footer`) — the two media may differ in depth,
which is exactly what A/B is for.

Both footers, and every layout's image backdrop and right-drag threshold, are driven by
shared values rather than per-path literals: `PANE_BG` and `MIN_DRAG_PX` in `app/mod.rs`.
Both had silently drifted (gray 24 vs 18; a per-axis 4 px test for the export crop
against a diagonal one for the stats region, so a long thin crop was discarded while the
same drag made a valid stats region). The **profile line** keeps its own threshold: it
measures in image space, a different unit.

The header is a **single row** (`header_h_for`): the title on the left, then the
**Auto-reload** toggle, **Reload** (re-reads this media from disk → `pending_reload`),
**Hide** (sets `visible = false` — keeps the pane) and **Close** (removes it) buttons on
the right, matching styles (Close tints red on hover to flag that it removes the pane).
**Reload keeps the current frame**, jumping straight to it rather than rediscovering
everything before it: the shown index is captured before the swap, then the fresh media
(which starts length 1) is landed back on it by the first of these that works —
1. `media::fast_jump` — the layout is regular, so the frame's position is *predicted*
   arithmetically, validated and decoded, growing the known length through it in one step;
2. `media::offset_jump` — **the frame's byte offset**, remembered from the last reload
   (`Pane.page_anchor`, a `media::PageAnchor`), re-validated against the fresh file and
   decoded from there. Needs **no** regular layout, so this is what covers the files
   `fast_jump` rejects (compressed or mixed-shape pages). An anchor is reused only when
   the file still matches it in full — same length and first-IFD offset, a byte-identical
   page header still at that offset (same shape *and* strip positions), and the page chain
   still entering it through the same predecessor — which is exactly the in-place
   overwrite auto-reload exists for, at a cost of two header reads. Anything structurally
   different fails a check, so the index can never drift onto a wrong frame; the chain is
   then walked once (headers only, capped at `MAX_ANCHOR_WALK`) to rebuild the anchor,
   which still beats riding the frontier a probe per update — and makes the *next* reload
   O(1). `FastScan::open_header` (the header parse split out of `open`) and
   `decode_strips` (shared with `read_page`) are what let this work without a stride;
3. `seek_to` — riding the frontier back with metadata-only probes, as before.

(An unsynced pane sets its own `frame`; a synced loop driver re-`seek_to`s the shared
timeline; other synced panes follow `shared_frame` via `catching_up`.)
(The Transformations controls are the global toolbar panel, not a per-pane header
button.)

**Auto-reload (file watch).** The **Auto-reload** toggle (fills blue while on, left of
Reload; hidden for a Compute pane, which has its own Auto-refresh) sets `Pane.watch`.
`poll_watches` (run each `update`, before `refresh_textures`) signs the pane's
source file(s) — `watcher::sign_paths` folds each file's **length + mtime + a small
strided byte sample** (`SAMPLE_BYTES` in `SAMPLE_WINDOWS` windows) into a
hash, plus the total length — and reloads the pane once a change has **settled**
(`WATCH_DEBOUNCE`, so a file still being written externally isn't read half-finished;
each further change re-arms the timer). The **byte sample** is the point: the common
case is a tool overwriting a **single multi-page TIFF in place** with identical
dimensions (`tifffile.memmap`), so the length doesn't change and an `mmap` writer
often doesn't bump the mtime until its dirty pages flush — an `(mtime, len)`
signature would miss it entirely, but a `read()` sees the new bytes at once. The
sample is **bounded** (a few KiB per file per poll regardless of file size, never a
bulk read), and applied only for a **small** source (`SAMPLE_MAX_FILES`); a
long numbered run stays on the cheap length+mtime path, since its per-frame files are
written normally and their mtime moves. Only the (heavier) `reload` fires, and only
on quiescence. `Watch.loaded` is the baseline signature (re-based via
`rebaseline_watch` after any reload and when the toggle is switched on, so enabling
never triggers an immediate reload); an unreadable file (mid-rename) simply waits for
the next poll.

**Signing is off the UI thread and rate-limited** — signing is real file I/O (tens of
ms on a network share), and both halves of that used to be wrong: it ran *inline* in
`update`, on *every* repaint (`WATCH_POLL` only paced the idle wake-up, so panning or
zooming signed the source 60–140×/s and hitched the paint). Now `poll_watches`
(a) **requests** a signature at most once per `WATCH_POLL` (`CimApp.watch_polled_at`),
one per pane in flight at a time, and (b) hands the work to
`crate::watcher::FileWatcher` — a single worker thread mirroring
`offsets::OffsetScanner`: it is given only the **paths** (never the pane's `Media` /
`Source`), signs them, posts a `SignDone` back and `request_repaint`s; the UI thread
only compares hashes and runs the debounce. Results are keyed by pane `id` **and a
generation** (`CimApp.watch_gen`, `Watch.inflight`): ids are stable across reload, so a
signature in flight when the watch is re-baselined measured contents that are no longer
the baseline and is dropped. Baselining is likewise asynchronous — `loaded = None`
means "adopt the next signature".

**The signing interval scales with the source's file count** (`CimApp::watch_interval`,
per pane via `Watch.polled_at`; `WATCH_POLL` → `WATCH_POLL_MAX`). A signature costs one
`stat` **per file**, so the cheap-looking metadata path is what scales badly: a 500-file
run at `WATCH_POLL` aimed 2500 filesystem calls a second at the server, which on a
**shared** network mount every other user pays for too. Backing off in proportion keeps
the call *rate* roughly flat instead of scaling with run length, while a lone TIFF — the
case auto-reload exists for — keeps the full 200 ms cadence. Deliberately **not** the
alternative of signing a *subset* of a long run's files: that holds the cadence but
silently stops noticing a change to any file left out. Polling all of them less often
loses neither. While any pane
watches, an otherwise-idle app wakes every `WATCH_POLL` to re-sign — the one
intentional break from "idle requests no repaint", kept moderate to stay VNC-friendly
(a quiet wake changes no pixels, so a delta framebuffer sends ~nothing) (§13/§15).
Hiding the **focused** pane (via the header button or the manager checkbox) moves
focus to the nearest still-shown media (`reselect_if_hidden`), so `current` never
sits on a hidden pane while others are visible. egui window/popup **shadows are
disabled** in `new` so nothing casts under panes or the Compute form.

**Transformations panel** (`draw_transform_panel`). A **single global** floating
window (not a per-pane popup), toggled by the **Transformations** toolbar button
(between Media and Compute) or `Action::ToggleVis` (default `V`), and stored on
`CimApp.show_transform`. Its contents **track the selected pane** (`current`) — the
title shows that pane's name, and selecting another pane updates it live. It is split
into two collapsible groups, each with its own **Sync** toggle:
- **Visualization** (open by default): the tone `ContrastMode` + its mode-specific
  options (`draw_tone_options` — **the single place to add a tone knob**: grow the
  mode's `ToneOptions` sub-struct, add a row, read it in `stage`/`tone_sig`), the
  Details (**RC**) toggle, and the **Overlay** picker. Its Sync toggle drives
  `set_sync_tone`.
- **Geometry** (collapsed by default): the **Rotate** control. Its Sync toggle drives
  `set_sync_geometry`.
Below the groups the pane's **Histogram** (`ensure_pane_histogram` + `draw_histogram`,
cached per pane) is **always** shown. Flipping a group's Sync toggle skips that group's
edit writeback that frame (`vis_sync_changed`/`geo_sync_changed`) so enabling sync makes
the pane *adopt* the shared set rather than push its own values into it.
A tone edit **does not null the texture**: it only changes the pane's `tone_sig`,
so `stage` re-renders and the lock-step commit swaps in the fresh frame while the
pane keeps showing its last committed `tex`. Nulling `tex` would blank a **heavy**
(async, off-thread) LUT_ALPHA/details render to **black** until it lands — a cheap
LUT refills synchronously the same update so its black is never seen, which is why
only the operator tones flashed. (Only overlay edits drop `overlay_tex`; **reload** and a **newly loaded operator
library** still null `tex` since the frame data, not the signature, changed — while a
Compute **recompute** instead bumps `render_gen` so it keeps the last frame, §9.)

**Two sync groups.** The Transformations split into **two independent** sync groups,
each a checkbox column in the manager's Sync row (and a Sync toggle in the matching
panel group):
- **Visualization (`Pane.sync_tone`, default on).** Follows `shared_contrast`/
  `shared_tone`/`shared_details`/`shared_overlay`. `contrast_of`/`tone_of`/`details_of`/
  `overlay_of` return the effective value (shared when synced), read by
  `stage`/`prepare_overlay`/`export_pane`/`view_command`. `set_sync_tone(false)`
  snapshots the shared tone/overlay in so nothing jumps.
- **Geometry (`Pane.sync_geometry`, default on).** Follows `shared_rotation` only.
  `rotation_of` returns the effective angle; `set_rotation` writes shared-or-own by it.
  `set_sync_geometry(false)` snapshots the shared angle in.
Editing a synced pane writes the shared set, and every synced pane re-renders on its
own because its effective `tone_sig` changed (no texture nulling — see above). The
first opened media seeds the shared set (`add_pane`); a replayed `--tone`/`--detail`
is per-pane so it unsyncs Visualization, `--rotate` unsyncs Geometry, then `--tsync`
(Visualization) / `--gsync` (Geometry) re-sync (`apply_view_state`).

**Overlays.** A pane may carry an `OverlaySpec { src_id, color, opacity }` — **any
single-channel media** (a boolean mask **or** a grayscale image/sequence) tinted over
it. The source list (`overlay_source_size`, single-channel resident frame, excluding
the pane itself) is offered in the panel's **Overlay** row. The spec is **config only**
so it rides the Visualization sync; the tinted texture is cached separately per pane in
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

**Pane rotation (Alt + drag / Transformations panel Geometry slider).** Each pane
carries a `rotation` (degrees, -180..180) that **rides the Geometry sync**
(`sync_geometry`, independent of the Visualization/tone sync): `rotation_of(i)` returns
the shared angle (`shared_rotation`) when the pane is geometry-synced, else its own, and
`set_rotation(i, °)` writes whichever applies — so editing one synced pane turns them all. **Alt + primary-drag** on a pane spins it to
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
name + colour underneath. **Hover readout:** while the pointer is over the plot a
full-span **crosshair** in the axis colour follows it, **snapped to a plotted sample** —
the cursor's x picks the sample index, and the series whose value there is nearest the
cursor's y wins (marked with a dot in its own colour; an all-NaN stretch has nothing to
snap to, so the crosshair just follows the cursor there). Each arm is labelled with its
value where it meets its axis (`label_box`, boxed so it stays legible over the tick
labels), and the hovered position is stored in `CimApp.line_hover` (distance along the
line, in line pixels) so **every pane** echoes it as a **green dot** on the line itself
(`draw_line_overlay`, `LINE_HOVER_COL`). Leaving the plot clears it (`draw_profile`
resets `line_hover` each frame and only re-sets it while hovered); since the panes draw
*before* the window, a changed hover asks for one more repaint so the dot doesn't lag.

**Compute panes.** A *generated* pane whose image is derived from other panes. The
**toolbar** "Compute" button sets `pending_compute_create`; the deferred
`add_compute_pane` adds an **unconfigured** Compute pane. `draw_compute_ui` (a
top-left foreground `Area` over the pane) has two states keyed on `Compute.computed`:
while `false` it shows the **config form** (mode + source combos + a **Compute**
button); that button sets `pending_recompute` (run at the top of the next `update`,
before `refresh_textures`, so the result never flashes black — §13) → `recompute_pane`,
which on success sets `computed = true`, so the **result image** then shows with the
**Save** control instead — there is **no Refresh button**, since a computed pane
refreshes itself. `Pane.compute` holds the `kind`, source id(s), `computed` (a result
exists → show it instead of the form) and `armed` (the user pressed **Compute**, or a
view command replayed the pane → it refreshes itself from now on). The two are separate
so a compute that *failed* still retries: a replayed pane whose source frames aren't
resident yet, or one waiting on an upstream Compute pane in a chain, recomputes as soon
as they land. `media::Reduce` modes:
- **Mean | Std** — `recompute_pane` → `compute_reduce` gathers **one** source's
  **resident** frames and calls `media::reduce_frames` (per-pixel/-channel, `f64`
  accumulation → `f32`).
- **Add | Sub** (`Reduce::is_binary`) — `compute_binary` takes **two** sources'
  *current* frames (`frame_disp`, both must be resident) and calls
  `media::combine_frames` (`A + B` / signed `A − B`, float). Sources may be stills;
  reductions need ≥2 frames (`compute_sources`).

**A sequence paired with a still** needs no special case: each source contributes the
frame it is *showing*, and a still always shows its only frame — so a binary op between
a sequence and a single image applies that image to whichever frame the sequence is on,
and the always-on refresh follows it across the whole sequence as it plays.

**Compute results are themselves sources**, so panes chain (mean of a sequence → subtract
that mean from the sequence). `compute_sources(idx, kind)` offers every pane *except* the
pane itself and anything that already reads it — `depends_on` walks the source graph, so a
cycle can't be selected; `compute_source_id` applies the same guard when a view command
replays sources by index.

Results become an `f32` `Media::still` (default tone Linear, clip on). **Refresh is
automatic** and has no toggle: `refresh_auto_compute` compares `compute_sig` (shown frames
for the binary ops, source resident-count for the reductions; a source that is *itself* a
Compute pane contributes its `render_gen`, which is what propagates a recompute along a
chain) against `Compute.last_sig` each update, iterating **to a fixed point** (bounded by
the pane count) so a whole chain settles within one update whatever order the panes sit in.
Only an `armed` pane refreshes, so an unconfigured one keeps its form. `Source::Computed` makes the manager's ⟳ recompute; an inline **Save**
(`media::save_frame`, `.tif` **32-bit float** or `.png`/`.jpg` 8-bit view, relative
to the working dir).

**No black flash on recompute.** `recompute_pane` swaps in the new result media but
does **not** null `tex`; instead it bumps a per-pane data generation
(`Pane.render_gen`, folded into `tone_sig`). The stale front texture keeps showing
while `stage` re-renders the new data into `pending` (off-thread for a large frame),
and the lock-step commit swaps it in when ready — so an auto-refreshing **Sub** pane
holds its last frame instead of blanking to black each time it recomputes (nulling
`tex` would blank a large/off-thread render until it lands).

**View-command round-trip.** A Compute pane re-emits in `view_command` as a
`compute:<kind>:<srcs>` positional token (`compute_token`), its sources given
as **pane indices** (0-based over the whole list) — so it keeps its slot and the
positional per-pane flags (`--tone`/`--tsync`/…) stay aligned. `cli::parse_compute_token`
turns it back into `cli::Input::Compute`; `commit_open` recreates the panes **in order**
(`add_configured_compute_pane`), then a second pass wires each source index → pane id and
recomputes (best-effort — a not-yet-resident source just leaves a status; the auto-refresh
recomputes once frames land), refusing any source index that would close a cycle. A pane
whose source is gone is skipped rather than emitting a dangling index. Older commands still
replay: `Reduce::from_token` accepts `diff` as an alias of `sub`, and a trailing `:auto` is
parsed and ignored. Because the transformations-sync flag is per-pane positional (`--tsync`), it now
round-trips for Compute panes too (they default un-synced). The token is deliberately
**not** prefixed with `@` — a leading `@` is PowerShell's splatting operator and would
silently drop the argument before it reached cim.

**`--tsync` is emitted whenever a per-pane transform flag is** (`--tone`/`--clip`/
`--share-clip`/`--detail`/`--rotate`), not only when a pane is unsynced. Each of those
flags makes `apply_view_state` *unsync* the panes it sets, so an all-synced session (where
`--tsync` is otherwise omitted as the default) would replay unsynced without it.

---

## 10. Export (`export.rs` + `app/export_ui.rs`)

> **The export replicates the view exactly.** This is the rule every change in this
> section answers to: what an exported frame shows must be what the panes showed at
> that timeline position — same pixels, same tone, same geometry, same per-frame
> behaviour. The view command is the other half of it: it captures the viewpoint
> precisely enough to reproduce a session, so *view command → replay → export* has to
> land on the same image as exporting the live session did.
>
> Practically that means: **never re-implement the view's behaviour on the export
> side.** Where the two need the same maths, they call the same function — the tone
> tail is one `imageproc::PaneOps::render_display` (§7), the Compute op is
> `media::combine_frames`, and everything about *tone bounds and frame choice* is
> `crate::tone` (§2): `frame_bounds`, `pixel_bounds`, `clip_pct`, `uses_colormap`
> and `synced_index` each have exactly one definition, called by
> `own_tone_bounds`/`stage`/`frame_disp`/`stage_target` on the view side and by
> `ExportPane`/`ControlBounds` on the export side. The *region precedence* (export
> crop → pinned stats region → whole frame) is likewise one method,
> `CimApp::tone_region`, whose result the export snapshots into `ExportPane.region`
> rather than restating.
>
> These were all hand-written mirrors once, and the mirrors did drift — a
> region-pinned pane had no region on the export side at all, so it exported with
> whole-frame bounds while displaying region ones, and a size-mismatched overlay
> was stretched into the video while the view refused to draw it. Both are now
> pinned by tests (§16). Where the export *cannot* reproduce something (a reduction over
> whatever frames happen to be **resident**), it snapshots the live result instead of
> approximating it. And where the live view is dynamic, the plan must be dynamic too:
> Share-clip bounds and Compute results are recomputed **per exported frame**, never
> frozen on the frame that was on screen when Export was pressed. The parity tests
> (§16) are what hold this — extend them with any new pane behaviour.

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

- **A Compute pane exports as a live computation.** A binary (`Add`/`Sub`) Compute
  pane becomes an `ExportSource::Computed { kind, a, b }` over its two inputs' own
  export sources, and `decode_source` re-runs `media::combine_frames` — the very
  function the live pane calls — for **every exported frame**. So a sequence minus a
  still animates in the MP4 exactly as it does on screen, instead of freezing on the
  result that happened to be showing. Each input is a `SourceInput`: a source, its own
  persistent reader, and the `(count, sync_temporal, own_frame)` triple that `src_index`
  maps `t` through — the export mirror of `frame_disp`, which is what makes a **still
  pair with each frame of a sequence**. Inputs are `ExportSource`s themselves, so a
  Compute pane reading another Compute pane nests. `export_timeline(idx)` gives a
  Compute pane the span of its longest input (and `sync_temporal = true`) so `t` passes
  through to the inputs; without it a one-frame Compute media would map every `t` to
  frame 0 and the video would hold one image.
  The **reductions** (mean/std) deliberately *don't* go this route: they reduce whatever
  frames are **resident**, a property of the live cache that the export can't reproduce,
  so the plan snapshots the on-screen result as a `Still` — which is exactly what the
  view shows (and, being constant across the timeline, loses nothing).
- `ExportPane` holds a snapshot view/clip/source plus its **own reader**
  (`ExportReader = Tiff(SeqReader) | Video(VideoReader)`) and a 1-frame
  decode+render cache. A pane's **mask overlay** is snapshotted too
  (`set_overlay` + `blend_overlay`), so overlays appear in the video. `ExportSource =
  Still | Seq { path } | Files { paths } | Concat { files, map } | Video { path }`
  (a video export walks the timeline forward, so the streaming reader's
  sequential path dominates).
- **Region-limited render pipeline.** `compose` runs in two phases: **(1) `decode`** every
  pane's source frame (so all sizes are known), then **(2) `render`** each pane on **only
  the cropped region**. With several panes both phases run the panes on **scoped threads
  in parallel** (each `ExportPane` is fully independent — own reader, operator instances,
  caches), so a grid export is paced by its slowest pane; a single pane runs inline. `ExportPlan::pane_boxes` computes, per pane, the axis-aligned
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
- **The resample runs across cores** (rayon, past `export::PAR_MIN_PX` = 16k output px;
  ~3.4× on 4 cores at 1080p). By then the panes are rendered, so every output pixel is an
  independent read — the buffer splits by **row** (`par_chunks_mut(w * 4)`). `ExportPane`
  itself is **not `Sync`** (it owns its `imageproc::Instance` operators, `Send`-only by
  design, plus its reader), so the loop borrows a `PaneSampler` per pane instead: just the
  rendered buffer plus the geometry sampling needs. `PaneSampler` owns `unrotate` /
  `sample` / `sample_base` / `blend_overlay`, and `pane_boxes` maps corners through it too,
  so there is a single sampling implementation. `par_composite_matches_serial_composite`
  pins the split against the LUT render at several thread counts.
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
  undiscovered). The warning (width-capped so it never widens the window past the
  preview) offers a single **Load frames** button (`load_offsets` — headers only, which is
  all export needs: the length, not resident frames) and a **Stop** while running. (The
  frame bar keeps the full **Load all** / **Load offsets** pair; the export panel doesn't
  need "Load all" since the encoder reads pixels from disk itself — `export_load_pending`
  and its cache-too-small `warn_popup` remain wired to the frame-bar path.)
- Output filename typed in the panel, written to the **cwd**. For video `start_export`
  spawns `run_export` on a **worker thread**, sharing an `AtomicUsize` progress +
  `AtomicBool` cancel; `export_tick` just polls it each update, relaying cancel and
  joining the thread for the final outcome (`ExportOutcome`). Inside `run_export` the
  compose and encode are **pipelined**: a second thread owns the plan and composes
  frames into a bounded `sync_channel(1)` (double-buffer — at most two frames in
  flight) while `run_export` writes them to ffmpeg, so frame `t+1` composes during the
  encode of `t` and the export runs at the slower stage's pace, not their sum. On any
  early exit the receiver is dropped before the join so a composer blocked in `send`
  can't deadlock. A still skips all that and saves synchronously.
- **Media names burnt into the output ("Add names").** The panel's toggle
  (`Export.labels_on`) draws one text label per media in every layout and both formats.
  The text is per media, keyed by **pane id** (`Export.labels: HashMap<u64, String>`, so it
  survives reorder/close), edited in a list of fields under the toggle and defaulting to the
  media's own name (`label_text`); colour, size, optional background box, 9-way position
  (`LabelAnchor`) and margin are **one global `LabelStyle`** shared by every label. All
  export state is runtime-only (not persisted in the config).
  Labels are **rasterized once at plan time** on the UI thread (`export_ui::rasterize_label`)
  through **egui's own font atlas** — laying the text out adds its glyphs to the atlas, then
  `Fonts::image()` gives the coverage bitmap the glyph `uv_rect`s index into — producing a
  `LabelBitmap` (plain alpha). Asking for a font size of `size_px / pixels_per_point`
  *points* makes the atlas glyphs exactly `size_px` **pixels** tall, so the blit is 1:1.
  Only the finished bitmaps ride into the `ExportPlan` (`labels`, index-aligned with
  `panes`), so no egui font type crosses to the worker thread.
  `ExportPlan::draw_labels` blends them at the **end of `compose`**, in **output-pixel**
  space (after the resample — so text is crisp and its size is independent of zoom, output
  height and composition scale), into the rect `label_rects` gives each pane: its grid slot,
  the single image area, or its half of the A/B wipe. Labelled pixels are forced **opaque**
  so a still's `crop_to_content` never trims a label off. Because this lives in `compose`,
  MP4, still, and all three layouts get it with no per-path code.
  The panel also shows a **preview**: the chosen media's live texture with the label drawn by
  the ordinary egui painter using the same anchor/margin/padding maths scaled by the
  preview's share of the output height — a faithful mock, not a re-run of the compositor.
  Its geometry comes from **`preview_geom(idx)` → `PreviewGeom`**, the live mirror of
  `label_rects`, in composition space: the pane's **label rect** (its packed grid cell /
  the single image area / its side of the A/B wipe), the part of that rect its **image**
  covers, and the **image-space rect** drawn there. The preview box *is* the label rect,
  so the anchor/margin/padding maths below it is the compositor's, unscaled.
  Two things must go through that rect, and both were once wrong by taking the whole
  canvas instead:
  - the **box shape** — a Grid pane measured against `last_area` comes out too wide by the
    column count (the height looks right, since one row spans the canvas), so it drifts
    with the window size and `max_columns`;
  - the **label scale** — `size_px`, `margin` and `bg_pad` are in **output pixels**, and
    the cell is a *fraction* of the output, so the conversion is
    `rect.height() / label.height() × region.height() / out_h` (preview-per-composition ×
    composition-per-output). Dividing by `out_h` alone undersizes the text and its margins
    by the **row count** in a multi-row grid. `grid_labels_are_anchored_inside_their_own_cell`
    (§16) pins the compositor half of that.

  A/B is the one layout where the image doesn't fill the label rect: both sides map through
  the **whole** area (`draw_ab_side` only *clips* to the half), so a side can be part
  background — `PreviewGeom::image` is that sub-rect, and re-centring the image inside the
  half (`pane_content_in(idx, half)`) would be wrong.

---

## 11. CLI (`cli.rs`) & entry (`main.rs`)

`main` → `cli::parse` → `Cli::Run { paths, view }` or `Cli::Exit(code)`.

- `-h/--help`, `-V/--version`.
- **View-state flags** (`ViewState`, 0-based, optional): `--mode`, `--cols`,
  `--zoom`, `--center X,Y`, `--frame`, `--pane`, `--control`, `--ab A,B,SPLIT`,
  `--tone` (per-pane `linear|lutalpha|colormap[:viridis|turbo|diverging]`;
  `linearclip`/`clip` are accepted as deprecated aliases for `linear`), `--clip`
  (per-pane Linear clip: `off` or the per-tail percentile, e.g. `0.01,off,0.5`; omitted
  at each pane's depth default), `--share-clip` (per-pane `1`/`0` — lock the pane's bounds
  to the Control media's), `--detail` (per-pane `1`/`0`),
  `--show` (per-pane visibility), `--tsync` (per-pane Visualization-sync),
  `--gsync` (per-pane Geometry-sync — rotation), `--rotate` (per-pane display rotation
  in degrees, `-180..180`), `--loop LO,HI`. Generated by the in-app "View cmd" window (`view_command`), applied
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
- A positional **`compute:<kind>:<srcs>`** token (kind = `mean|std|add|sub`;
  `srcs` = one pane index for the reductions or `A,B` for the binary ops) →
  `Input::Compute`, recreating a **Compute pane** from a
  view command (`view_command` emits it for a `Source::Computed` pane; §9). Normally
  generated for you, not typed by hand.
- **Videos** (`VIDEO_EXTS = [mp4,avi]`) always open **one pane each** — a video
  already is a timeline, so it is never grouped: a directory arg opens its
  images as one concatenated sequence **plus** one `Single` per video
  (`dir_inputs`, shared with drops/dialog via `inputs_for_path`), and a numbered
  `%0Xu` token naming videos expands to one pane per file rather than a
  `FileSeq`.
- `--complete <word>` lists loadable completions (collapses numbered runs into the
  token — videos are listed literally, never collapsed); `--completions
  <bash|powershell>` prints a completer. `LOADABLE_EXTS =
  [tif,tiff,png,jpg,jpeg,bmp,webp]` + `VIDEO_EXTS` is shared by the dialog and
  the filter.

---

## 12. Settings & persistence (`settings.rs`)

`Config { language, max_columns, ui_scale, cache_budget_mb, cpu_budget, cursor_dot,
cpp_lib_dir, hardware_accel, keybindings }` (`hardware_accel` = build display pixels on
the GPU — §7.1 — **off by default**, and the checkbox is shown only under `CIM_GPU=1`
(§7.2); read **once at startup** since it also picks eframe's renderer, so Settings shows a
"restart to apply" note. An old config's `render_backend`
— the retired `Auto | Cpu | Gpu` dropdown — is an unknown field and ignored, so an
instance that had it on `Auto` comes back on the CPU) (`language` = the UI locale, §12.1; `cpp_lib_dir` = the folder holding the proprietary
operator libraries, loaded at startup and auto-loaded when the folder changes
— §7 — with a Browse/paste field plus found/not-found and loaded indicators in
Settings; empty = the `LIBS` folder next to the cim executable, else by name via `LD_LIBRARY_PATH`. `cpu_budget` = total
worker threads across the decode and rayon pools, 4–64, default 16 — §5.1),
saved as JSON via `ProjectDirs("dev","cim","cim")` — Windows
`%APPDATA%\cim\cim\config\config.json`, Linux `~/.config/cim/cim.json`. Loaded on
start; **written automatically** once an edit settles — there is no Save button.
`config` is edited live by the widgets, so `CimApp::autosave_config` (run each `update`)
notices a change by comparing against `seen_config`, arms `autosave_at` for
`CONFIG_AUTOSAVE_DEBOUNCE` (0.5 s), and writes on expiry only if `config != saved_config`
(the on-disk copy) — the debounce is what keeps a slider drag from rewriting the JSON every
frame, and the comparisons need `PartialEq`. It requests a repaint at the deadline so an
otherwise-idle app still writes. Settings' footer offers only **Reset to defaults**
(`Config::default()`, keybindings included), which is then saved the same way.

### 12.1 Localisation (`locales/*.yml`, `rust-i18n`)

**Every user-visible string is a translation key** — nothing user-facing is a literal in
the source. Two locales ship: **English (the default)** and French. The tables live in
`locales/en.yml` / `locales/fr.yml` in rust-i18n's **version 1** format (one *flat* file
per locale, `_version: 1` then `area.key: text`), baked into the binary by
`rust_i18n::i18n!("locales", fallback = "fr")` in `main.rs`.

- **Reaching it:** `t!("area.key")` (→ `Cow<'static, str>`, which egui's `WidgetText`
  takes directly, so `ui.label(t!(…))` needs no conversion); `t!("k", name = v)` fills a
  `%{name}` placeholder. `app/mod.rs` does `use rust_i18n::t;`, so every `app` submodule
  gets it through its `use super::*`.
- **Dynamic keys** are used where an enum already has a stable id: `Action::label` →
  `action.<id>` and `Reduce::label` → `compute.reduce_<token>`, so adding an action or a
  reduction needs no second table — just a locale entry. The *ids and tokens themselves
  are never translated*: they key the config JSON and round-trip through view commands.
- **`fallback = "fr"`** means a key missing from `en.yml` shows French, not the raw key
  — which is silent, hence the tests (§16) that the two files carry identical key sets
  and that every literal `t!` key in the source exists. `en.yml` is still the *reference*
  table (new keys land there first); the fallback only decides what a gap degrades to.
- **Scope:** the toolbar/panels/manager/export/settings/modals, pane chrome, status
  notes, error and warning messages (including the ffmpeg/TIFF ones that surface as a
  pane error), **and the CLI `--help` page** (`cli.help`, one multi-line entry per
  locale). `main` therefore calls `settings::apply_locale` *before* `cli::parse`, since
  `--help` never reaches the window. **`help.md` is deliberately excluded** — it stays an
  external document the deployment owns (§12, below).
- **Changing language** (Settings → Language, listing each language in its own name)
  calls `apply_locale` immediately, so the UI is translated on the next frame — no
  restart. The choice is `config.language`, persisted by the ordinary autosave.
- **Not translated on purpose:** proper names (LUT_ALPHA, Details, Turbo, Viridis, MP4,
  fps), the A/B slot letters, and anything that is an identifier rather than prose.
- **Text-sized chrome:** the pane header buttons are hand-painted at a measured width, so
  `draw_header` measures each *translated* label (`text_w`) instead of assuming the
  English one — a longer French word would otherwise clip.

**Help window** (`app/help.rs`, toolbar **Help** button, no keybinding). Renders an
**external `help.md`** — looked for beside the executable first (how a release is laid
out, mirroring `LIBS`), then in the working directory; read on the **first open** (so an
unused button costs no I/O) and re-read by the window's **Reload** button, cached in
`CimApp.help_doc` as `Result<String, String>` (the error names every path tried). Only a
deliberate Markdown subset is rendered — `#`/`##`/`###`, `-`/`*` bullets (one nesting
level), fenced code, `---`, and inline `**bold**` / `*italic*` / `` `code` `` — each line's
inline spans laid out as **one `LayoutJob`** so mixed styles wrap as a paragraph; anything
else shows as plain text rather than being dropped. It documents what Settings can't: the
**mouse/modifier** commands (§9), since the keyboard shortcuts are already listed and
rebindable there.
New `bool`/scalar fields take a `#[serde(default = …)]` so an older saved config still
loads.

`Action` = all bindable actions (view toggles, next/prev media & frame, fit/actual/
zoom, load all, open, toggle panels, **open Compute pane** (default `C`), play/pause,
**reload focused / reload all / hide media**, `SelectMedia(0..12)`). Buttons that
replicate an action carry its current shortcut in their hover tooltip
(`CimApp::hover_for` — `"Ctrl+R. <desc>"`, or just the chord when the button had no
description); it reads the live binding, so a rebind updates the tooltip immediately.
`Keybindings` is a `BTreeMap<action_id, chord_string>` with unique bindings, where a
**`Chord`** is a key **plus optional Ctrl/Shift/Alt modifiers** (`ctrl` = egui's
cross-platform `command`). It serialises as a `Ctrl+Shift+Key` string, so an older
config storing a bare key name still parses (a no-modifier chord). Matching is
**exact** (`Chord::pressed` — key **and** modifier set), so `R` (reload focused) and
`Ctrl+R` (reload all) stay distinct; rebinding captures the key press together with
the modifiers held at that moment (`Chord::from_modifiers`, egui emits no Key event
for a bare modifier). Default `Reload focused = R`, `Reload all = Ctrl+R`, `Hide = H`.
The pane header also has a **Reload** button (left of Hide). New default bindings do
**not** retroactively apply to a saved config (shows `—` until rebound).
`handle_input` skips the shortcut scan while `ctx.wants_keyboard_input()` (a text
field has focus), so typing doesn't trigger views. **Tab** (default `ToggleView`)
would otherwise be stolen by egui's built-in focus navigation, which lands on the
first toolbar button and traps every shortcut (a focused widget makes
`wants_keyboard_input` true); `handle_input` absorbs that focus move onto a throwaway
id each frame so Tab cleanly cycles the view and no button ever holds focus.

---

## 13. The update loop (`app/mod.rs::update`)

Each frame: apply `ui_scale`; `clock += 1` (on the **first** frame send
`ViewportCommand::Maximized(true)` — the window is **not** created maximized:
on Windows winit applies `with_maximized` at creation via `ShowWindow(SW_MAXIMIZE)`,
flashing the still-unpainted window **white** before eframe's own
hidden-until-first-frame logic can help, and Linux/Wayland often ignores the builder
flag anyway. Instead `main.rs` asks for an oversized window (eframe clamps it to the
monitor) and this command — processed after the first frame is painted — maximizes
it. On Windows the window is additionally **DWM-cloaked** in `new`
(`set_window_cloak`) because eframe *shows* the window just **before** the first
`swap_buffers` presents it — a race the DWM intermittently loses, compositing the
still-blank window white for a few frames. A cloaked window is fully managed but
never composited, so nothing can flash; `tick` **uncloaks on the third frame**, once
a real maximized frame has been swapped, requesting repaints until then so those
first frames come even while idle); **resize both shared pools if `cpu_budget`
changed** (§5.1); `pump_decoder` → `pump_render` (stage
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
the transient `status` note and `autosave_config` (write a settled Settings edit, §12);
draw the central panel (always the **whole window** —
the toolbar and bottom frame bar are anchored `Area` **overlays** floating over its
top/bottom edges, not layout panels, so hiding them via `Action::ToggleChrome` never
reflows the images; the frame bar shows whenever **any** media is a sequence), the
compute draft, windows (manager/export/settings/view-command/help), error popup, the
**">8 sequences" resource warning** (`pending_open` — Open anyway → `commit_open`,
Quit → close); apply deferred actions; `export_tick`; then a **paced repaint**.

**Transient notifications (`status`).** A single line shown **top-right in the toolbar**
at normal size (e.g. "Settings saved", "View command copied"). `update` shadows the
last value (`status_shadow`) to detect a fresh message, stamps `status_at`, and clears
it after `STATUS_TTL` (10 s) — so every `self.status = …` site, current and future,
auto-expires for free (and a `request_repaint_after` wakes an idle app to clear it).
Per-media errors are **not** this: they stay centred in their pane (`draw_pane_error`),
as does the modal `error_popup`. (There is no per-pane decode spinner — a pane holds its
last committed frame while the next one decodes / renders; see §7.)

**Paced repaint** (not `request_repaint()` at monitor rate — pure waste over VNC):
playback wakes when the **next frame is due** — `request_repaint_after(step −
playback.accum)`, not a fixed `1/fps` interval, so a render-gated commit that already
consumed part of the interval doesn't push the next frame late (while a playback step is
**in flight** awaiting its gate it instead sets a slow `DECODE_POLL` fallback, since the
worker wakes it the instant the frame lands — below); a pending background decode, an
**in-flight tone render** (`render_inflight`), **or a running export** (which encodes on
its own thread — we just poll progress) wakes every `DECODE_POLL` (~30 fps, enough to
pick up landed frames and commit them); a fully idle app requests no repaint at all.

**Worker wake-ups.** Both the decode pool (`decoder.rs`) and the tone-render pool
(`renderer.rs`) hold a cloned `egui::Context` and call `request_repaint()` the instant a
job finishes, so a landed frame / render is drained (and, during **render-gated
playback**, committed) immediately rather than on the next paced repaint. Without this the
gate waited up to a whole frame interval for the paced tick, so playback ran at a fraction
of the requested fps whenever a frame needed a decode or an off-thread render (moving the
mouse — which forces input-driven repaints — masked it by waking the loop constantly).

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
  timeline at build time (press the export panel's **Load frames**, or a frame-bar
  "Load all" / "Load offsets", first for a full export — offsets suffices, since export
  only needs the discovered length).
- **Never time anything with `i.stable_dt`.** egui only reports the real elapsed
  time when the previous frame requested an *immediate* repaint; on a frame woken
  by `request_repaint_after` — i.e. every paced wake in this app — it substitutes
  a fixed `predicted_dt` (1/60 s). Under paced repaints that under-credits time by
  the pacing ratio (playback at 25 fps was credited ~17 ms per ~40 ms wake and ran
  at a fraction of the set rate; any user input masked it by making dts real).
  Use `i.time` deltas (wall clock) instead — see `Playback.last_tick`.

---

## 15. Performance notes (VNC / no GPU)

Everything in this section is about making the tool fast **without** a graphics card, and
none of it is superseded by the optional GPU path (§7.1): that path is off by default on
any machine without a hardware adapter, is never used by a pane running the C++
operators, and hands work back to the CPU on any failure. A GPU is an addition here, not
a replacement — the CPU pipeline stays the one that must be fast.

Done: lazy length, persistent readers, bounded LRU cache, LUT render + memoized bounds
+ reused buffer, **cross-frame LUT reuse** (§7 — `ToneLut` per pane, so fixed-tone
playback doesn't rebuild the 64 Ki table each frame), **incremental LRU eviction** (§6 —
a recency-ordered set per `SeqCache`, no whole-cache scan/sort per over-budget tick),
per-pane histogram cache, **paced repaints** (§13, no busy-spin while
decoding/playing), **display-resolution staging** for minified panes (§7 — nearest-decimate
the synchronous render so a grid of sequences doesn't render/copy/upload full-res textures
the screen can't show; seamless across 1×), **playback decode prefetch** (§5 — overlap
decode with display so first-pass / multi-pane playback doesn't stall on decode latency,
now **fair-dispatched** across panes and **adaptive-depth** on measured decode latency),
**worker wake-ups + wall-clock playback pacing** (§13/§8 — the decode/render pools
`request_repaint` on completion, playback wakes when the next frame is actually due and
times itself on `i.time` deltas rather than the paced-repaint-poisoned `stable_dt`
(§14), so render-gated playback holds the requested fps instead of collapsing to a
fraction of it), **off-thread big plain-LUT renders** (§7 — `ASYNC_RENDER_PIXELS`, so a
playback step's tens-of-ms LUT render doesn't block an update and hitch a concurrent
pan), an **off-thread, rate-limited auto-reload watch** (§9 — the signature I/O used to
run inline on every repaint, so panning/zooming with Auto-reload on stalled the frame
60–140×/s), **parallel per-pixel work** (§7/§14 — the display render, the export
composite and the analytic scans all split across cores), and a **single CPU thread
budget** (§5.1 — one live-applied cap covering the decode *and* rayon pools, so an
instance on a shared host runs the thread count Settings shows rather than one per
core). For shared multi-user servers there's also a **">8 sequences" resource warning**
(§13) before opening a heavy number of sequences at once. Remaining candidates: minor
per-frame allocations (`Action::all()`, `grid_cells`); a per-instance cache-budget cap /
lower default for shared hosts; capping the software-GL (llvmpipe) rasterizer threads
per session (`LP_NUM_THREADS`), which is an env/deploy knob, not code; and passing
`-threads` to the export ffmpeg, whose x264 encoder still sizes itself to the machine
(a child process, so outside the budget above).

### Network mounts (shared NFS/SMB) — the read path

Sources normally live on a **shared** mount, where the picture inverts: a 4096²×u16 page
measured **~150 ms of file I/O against ~0.1 ms of CPU decode**. Everything above tunes the
0.1 ms side. Two constraints shape what's allowed on the other: the mount options are not
ours to change, and the link is shared, so the goal is **to stop wasting round trips, not to
grab bandwidth**.

What the measurements said (keep these — they overturned two plausible theories):

- A `dd` block-size sweep plateaus at **~210–280 MB/s regardless of request size**, and cim
  already achieved 213 MB/s. So the ceiling is the path (server / shared link), *not* cim's
  read shape, and **cold-path headroom is ~11%, not a multiple**. Widening concurrency was
  dropped for this reason: at ~89% of the single-stream ceiling it could only contest the
  remainder, and only by taking someone else's share.
- Kernel readahead was **already working** — cim at 512 KB reads beat cold `dd` at the same
  size (213 vs 140 MB/s) — so `posix_fadvise` was dropped too.
- **Warm page cache runs 1.1–2.6 GB/s, ~10× the cold read.** The real lever is therefore *not
  reading cold at all*, which is why the Settings cache slider now shows **how many frames the
  budget holds** (`cache_budget_frames`): at 32 MB/frame the 1.5 GiB default holds only ~48,
  so scrubbing silently evicts and re-reads.

Done, in that light: **strip-run coalescing** (§4 — `decode_strips` merges file-contiguous
strips into one read, ~68 reads/frame → 1; contiguous runs only, since bridging a gap spends
shared bandwidth on bytes we discard), **positional reads** (`read_at` uses
`read_exact_at`/`seek_read` — one syscall, no cursor, and `&File` rather than `&mut File`),
**parallel per-file offset scanning** (§4 — `SCAN_FANOUT`; safe to widen because these are
*header* reads, latency-bound, so they cost the server cheap metadata ops rather than a share
of the link), and an **auto-reload watch that backs off with file count** (§9 —
`watch_interval`; one `stat` per file meant a 500-file run aimed 2500 filesystem calls/s at a
shared server).

Not done, deliberately: **decimated reads** for minified panes would cut bandwidth hugely, but
`FrameData` is the native cache behind the value readout, histograms, stats and export (§14) —
decimating it breaks pixel accuracy, so it would need a *separate* proxy cache. **Latency-driven
auto-tuning** of thread counts is wrong here: it widens exactly when the mount is busiest.
Worth one email to whoever owns the mount, though: `rsize=65536` caps single-stream throughput
at `rsize/RTT`, and `rsize=1048576` + `nconnect=8` would be worth several times anything above.

**Profiling the pipeline (`debug.rs`).** Launch with **`CIM_DEBUG=1`** to enable a
per-stage timing profiler and a **Debug** toolbar button (both hidden otherwise, so
there's zero cost in a normal run — `debug::enabled()` reads the env var once and every
record site is gated on it). Each stage on the read→display path records into a bounded
ring buffer (last ~120 samples → last/avg/min/max): **Read (file I/O)** and **Decode
(CPU)** — split at a `TimedFile` shim under the persistent TIFF reader that accumulates
time spent inside `read`/`seek` calls (the `tiff` crate interleaves reads with
decompression, so the I/O layer is the only place the two can be told apart; carried back
per job on `Done.elapsed`/`Done.io`; an OS-page-cache hit reads as near-zero I/O, and a
standalone still can't split so it records wholly as Decode) —, **LUT / tone render** and **Operators**
(LUT_ALPHA/details, split and timed on the render worker via `RenderDone.lut_time/ops_time`,
plus the synchronous cheap-pane LUT timed in `stage`), **Texture upload** (`ColorImage` build
+ GPU upload), and **Update** (the whole `update` CPU frame, excluding the GPU paint eframe
does after). The `⏱ Debug` window (`draw_debug`) tabulates them so the bottleneck stands out.

---

## 16. Testing

Inline `#[cfg(test)]`, run against **synthetic fixtures generated at test time**
(`src/testutil.rs` — multi-page u16 TIFFs with varying page sizes, PNG runs, a
hand-written 1-bit bilevel-mask TIFF); the ffmpeg-dependent tests (MP4 encode,
`media::video` probe/decode/seek against a generated `testsrc` clip) skip when
`ffmpeg` is absent, while the ffprobe-output parsing and seek math test without
it. Coverage: `cli` token expansion/grouping (incl. **video dir/token/completion
exclusion**); `media` lazy length / probe
discovery / eviction (incl. **LRU peek order + shown-frame protection**), **LUT render
matches the float reference** bit-for-bit, **`ToneLut` reuse == uncached render** (and
the decimated small-output arithmetic path), **Colormap maps through the palette**,
mask/intensity renders, region stats + save round-trip; **offset-anchored jump** (lands
on a mixed-shape page bit-exact with the chain walk, nothing before it discovered; an
anchor survives an in-place rewrite and is refused by a reshaped file, which then walks);
**percentile equivalence**
(whole-image == full-frame region, integer and float, with golden values);
**parallel-scan equivalence** (percentile, histogram, stack reduction and binary combine
each give bit-identical results across 1/2/3/4/8-thread rayon pools, and a region scan
still ignores pixels outside it); `cpu` **budget split** (the two shared pools never
exceed the total at any budget, out-of-range values clamp, **nested** parallel work stays
within the cap — counting the distinct workers that actually ran, since
`current_num_threads` alone wouldn't catch a job escaping to the global pool — and a
resize mid-job can't deadlock; the resizing tests share a `SERIAL` mutex because the
pool is process-global);
`renderer` **worker output == plain LUT render** when no operator library is loaded;
`app::decode` **prefetch interleave order** + **adaptive depth**; **out-of-order probe
results can't truncate a sequence** (a batch's misses delivered before the hits ahead of
them — the ordering `probe_ahead` can genuinely produce); `app::help` inline-span parsing (every character of a line survives, an
unterminated marker stays literal); **localisation** (§12.1 — `en.yml` and `fr.yml`
carry identical, duplicate-free key sets; every literal `t!` key in the source is
defined; every bindable `Action` has an `action.<id>` entry — the three gaps that
otherwise show up only as English text or a raw key at runtime); `palette` endpoints /
diverging-centre / token round-trip; `cli` **`--share-clip` / `--tone colormap`** parsing;
`export` full compose→ffmpeg encode, **two-pane parallel (scoped-thread) compose**,
**pixel-exact region crop** (incl. rotated), **multi-row grid labels anchored inside
their own cell** (the output-fraction the panel's label preview scales by), **a Computed source recomputed per
exported frame** (the composed pixels equal the same `combine_frames` done by hand on
each page, and the values move frame to frame — the export/view parity rule of §10),
**full-frame export == live LUT render** (and the same at 256×128, past `PAR_MIN_PX`, so
the **row-parallel composite** is checked against it at several thread counts),
content-only export (`content_region`
excludes background) + still background crop, **a size-mismatched overlay skipped rather
than stretched** and **a pinned region driving the exported tone** (the two §10 mirrors
that had actually drifted — the export used to stretch the overlay the view refused to
draw, and carried no region at all). `tone` covers its own maths directly:
**`frame_bounds` == the `FrameData` call it stands for** across clip on/off × region
some/none on an integer and a float frame, a **region outside the frame falling back to
whole-frame bounds**, `clip_pct` ignoring LUT_ALPHA, and `synced_index` holding a short
media / pinning an unsynced one / not dividing by zero. The parity/equivalence tests are
the net that guards the unified `render_display` / `percentile_rect_*` / `tone::*`
paths (§7, §10).

The network-mount read path (§15) is guarded the same way — by equivalence, since every
change there is meant to alter *only* the shape of the I/O: **strip-run coalescing**
assembles the same bytes however a page's strips split into runs (contiguous, disjoint,
and both mixed orders, against a naive strip-by-strip reference), and a multi-strip page
still decodes bit-identically to the `tiff` crate; **parallel offset scanning** keeps
counts in file order across more files than one batch holds, and an unscannable file
does not cancel a `fast_jump` that lands before it (while one that needs it still fails);
`app::watch` covers the signing back-off (flat call rate, capped). All three were
mutation-checked — misaligning the chunk assignment, and failing the whole batch instead
of the prefix, each make the relevant test fail.

---

## 17. Conventions

- **Commits:** small, one concern; imperative summary + a short *why*. Committed
  directly to `main`.
- **Always `cargo fmt` before committing** — the whole tree is rustfmt-clean
  (default settings, no `rustfmt.toml`), so `cargo fmt --check` must pass on every
  commit. Formatting-only churn then never rides along with a real change.
- **`cargo clippy --all-targets` is expected to be silent.** Fix the warning where
  there's a real fix; where the lint is wrong for this code, `#[allow(…)]` it *with
  a comment saying why* (see the `large_enum_variant` allows).
- **Build target:** Windows, debug, during development.
- **Style:** match surrounding code (comment density, naming, `pub(super)` methods,
  free helpers in `app/mod.rs`).
- **No user-facing string literals.** Any new label, tooltip, status note or error the
  user can read goes in `locales/en.yml` **and** `locales/fr.yml` and is reached through
  `t!` (§12.1); the tests fail on a key defined in only one of them. Comments and doc
  comments stay in English.
- **Video limitations (§3):** frames are 8-bit (higher-depth sources tone-mapped
  down by ffmpeg) and frame↔time assumes CFR — a VFR file may land ±1 frame on
  seeks.
