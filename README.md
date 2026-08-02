# cim — Compare Images & Media

A lossless side-by-side viewer for still images, multi-page TIFF sequences and
videos. It exists for one thing: comparing media **pixel-accurately** — native bit
depth is preserved, the true sample value under the cursor is readable in every
pane at once, and nothing is ever interpolated or re-encoded behind your back.

## Features

Open any number of images, numbered runs, whole folders or videos as panes in a
grid, single view or A/B wipe, with view, timeline and tone controls synced across
them; then read exact values, plot histograms/profiles/region stats, compute
mean/std/add/sub compute panes, and export the comparison as an MP4 or a still.

Also: lazy discovery of huge TIFF sequences (with an O(1) fast scan for regularly
laid out files), a bounded frame cache, auto-reload on file change, and optional
proprietary C++ image operators loaded at runtime (see `INTEGRATION_CPP.md`).

## Usage

```sh
cim [OPTIONS] [FILES|SEQUENCES|DIRS]...
```

Anything you can name on the command line opens as a pane:

```sh
cim a.tif b.tif                 # two panes
cim frame_%05u.tif,0,12         # one pane: a numbered run, frames 0..12
cim my_folder                   # one pane: every loadable file in it, alphabetical
cim clip.mp4                    # one pane per video (never grouped)
```

Supported: `tif`, `tiff`, `png`, `jpg`, `jpeg`, `bmp`, `webp`, plus `mp4`/`avi`
videos (decoded through the `ffmpeg` CLI, which must be on the `PATH` — export
needs it too).

`cim --help` lists everything, including the `--mode` / `--zoom` / `--frame` /
`--tone` / … _view-state_ flags. You normally don't type those by hand: the in-app
**View cmd** panel copies a ready-made command line that reopens exactly the
session you're looking at.

### Shell autocompletion

Completion is built into the binary — there is no separate script to install. Two
flags do the work:

- `cim --completions <bash|powershell>` prints the completion script for that shell.
- `cim --complete <WORD>` is what that script calls; it lists loadable matches for
  `WORD`, one per line.

The point of it is that completion is **sequence-aware**: consecutive numbered
files collapse into the compact `PREFIX%0Xu SUFFIX,START,END` token, so tabbing
through a directory of 10 000 frames offers you _one_ suggestion that opens them
as a single sequence instead of 10 000 file names.

Try it for the current shell:

```sh
# bash
eval "$(cim --completions bash)"

# PowerShell
cim --completions powershell | Out-String | Invoke-Expression
```

To keep it, append the same line to your shell profile:

```sh
echo 'eval "$(cim --completions bash)"' >> ~/.bashrc      # bash
cim --completions powershell >> $PROFILE                  # PowerShell
```

(`cim` must be on the `PATH` for either.)

## Project structure

```
src/
  main.rs       entry point: parse CLI, launch the eframe window
  cli.rs        arg parsing, --help, completion, sequence-token expansion
  media/        the data model: frames, sources (still/TIFF seq/file seq/concat/video),
                loading, tone rendering, stats, fast page scan
  app/          the GUI: state, update loop, panels, input, decode plumbing, export UI
    canvas/     the central image area (grid/single/A-B, overlays, region tools)
  decoder.rs    background decode thread pool
  renderer.rs   off-thread tone-render pool
  export.rs     export engine (compose + ffmpeg encode)
  settings.rs   config, keybindings, persistence
  imageproc.rs  runtime loader for the optional C++ operators
cpp/            those optional operators (built separately)
build_utils/    offline Docker build environment (Linux + Windows)
ci/             CI build helpers
```

## Development

```sh
cargo run -- a.tif b.tif    # run (debug is a console app, so CLI output is visible)
cargo build --release
cargo test                  # all tests
```

Tests need no fixtures on disk: they generate synthetic TIFFs/PNG runs at test
time. The tests that need `ffmpeg` (video decode, MP4 encode) skip gracefully when
it isn't installed.

### Reproducible / offline builds with Docker

`build_utils/` holds one Docker image that builds both release targets — Linux
against **glibc 2.28** (runs on RHEL 8 / Debian 10 / Ubuntu 18.04+) and Windows
cross-compiled with mingw-w64 — with **no network access** at build time.

```sh
build_utils/build.sh image      # build the image (needs internet, run from repo root)
build_utils/build.sh            # both targets
build_utils/build.sh linux      # Linux only
build_utils/build.sh windows    # Windows only
```

Binaries land in `target/docker/linux/release/cim` and
`target/docker/windows/x86_64-pc-windows-gnu/release/cim.exe`, kept apart from your
host `target/`. To use it air-gapped, `docker save` the image on a connected
machine and `docker load` it there. Rebuild the image only when `Cargo.lock` or the
Rust toolchain changes — see `build_utils/README.md` for the full flow and caveats.
