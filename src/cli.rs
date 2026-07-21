//! Command-line handling: `--help`, shell completion, and expansion of the
//! compact numbered-sequence token used both on the command line and by the
//! completion suggestions.
//!
//! A numbered run is written `PREFIX%0Xu SUFFIX,START,END` — for example
//! `frame_%05u.tif,0,12` stands for `frame_00000.tif` … `frame_00012.tif`.
//! The GUI never has to enumerate a directory: the token carries the whole
//! range, so the shell offers it on Tab and the app expands it on launch.
//!
//! A bare **directory** argument (`cim folder` / `cim folder/`) is a second way
//! to name a sequence: every loadable file directly inside it, sorted
//! **alphabetically**, is concatenated into one pane. Unlike the numbered token
//! this needs no `%0Xu` range and so works for any file naming; the directory
//! path itself is the token, round-tripped by the view command.

use std::path::{Path, PathBuf};

/// File extensions the app can open (stills + multi-page TIFF). Shared by the
/// file dialog and the completion filter so they never drift apart.
pub const LOADABLE_EXTS: &[&str] = &["tif", "tiff", "png", "jpg", "jpeg", "bmp", "webp"];

/// Video containers, each opened as **one pane of its own** — never grouped
/// into a numbered-run sequence or a directory concatenation (a video already
/// is a timeline).
pub const VIDEO_EXTS: &[&str] = &["mp4", "avi"];

/// Outcome of parsing argv.
pub enum Cli {
    /// Launch the GUI, opening these inputs at the initial view described by
    /// `view` (empty unless `--mode`/`--zoom`/… were given).
    Run { inputs: Vec<Input>, view: ViewState },
    /// A CLI-only request (help / completion) was handled; exit with this code.
    Exit(i32),
}

/// One thing to open. A bare path becomes a single media; a compact
/// `PREFIX%0Xu SUFFIX,START,END` token that names two or more files becomes **one**
/// image sequence (a single pane), not a pane per file.
pub enum Input {
    Single(PathBuf),
    /// A numbered still sequence: `token` is the original compact argument (so
    /// the view-command panel can round-trip it) and `files` are its frames.
    Sequence {
        token: String,
        files: Vec<PathBuf>,
    },
    /// A Compute pane recreated from a view command (`compute:<kind>:<srcs>`).
    /// Its sources are given as **pane indices** (0-based over the whole pane
    /// list) — resolved to the newly created panes once they all exist. `Diff`
    /// carries two indices, the reductions one. `auto` restores auto-refresh.
    Compute {
        kind: crate::media::Reduce,
        a: usize,
        b: Option<usize>,
        auto: bool,
    },
}

/// Which layout a `--mode` flag selects. Mirrors the app's `Mode` but lives here
/// so the CLI stays decoupled from the GUI module.
#[derive(Clone, Copy, PartialEq)]
pub enum ViewMode {
    Grid,
    Single,
    Ab,
}

/// Per-pane tone mode carried by `--tone`. Mirrors the app's `ContrastMode` but
/// lives here so the CLI stays decoupled from the GUI module.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tone {
    Linear,
    LutAlpha,
    /// Colormap with the given palette (`colormap:<name>`; bare `colormap` =
    /// the default palette).
    Colormap(crate::palette::Palette),
}

/// Per-pane Linear-clip state carried by `--clip`: the toggle plus, when on, the
/// per-tail percentile. Mirrors the app's `ClipOptions`.
#[derive(Clone, Copy, PartialEq)]
pub enum ClipSpec {
    /// Clip disabled — the plain full-range map.
    Off,
    /// Clip enabled at the given per-tail percentile (percent).
    On(f32),
}

/// A viewpoint captured from a running session and replayed on the next launch.
/// Every field is optional: only the flags actually present on the command line
/// override the app's defaults. Produced by the "View command" panel and parsed
/// back here so a shared command reopens the same files at the same view.
#[derive(Default)]
pub struct ViewState {
    pub mode: Option<ViewMode>,
    pub cols: Option<usize>,
    pub zoom: Option<f32>,
    pub center: Option<(f32, f32)>,
    pub frame: Option<usize>,
    pub pane: Option<usize>,
    pub ab: Option<(usize, usize, f32)>,
    /// Per-pane tone modes (`--tone`), in pane order.
    pub tones: Option<Vec<Tone>>,
    /// Per-pane Linear clip state (`--clip`), in pane order.
    pub clips: Option<Vec<ClipSpec>>,
    /// Per-pane "Share clip" toggles (`--share-clip`), in pane order.
    pub share_clip: Option<Vec<bool>>,
    /// Per-pane DETAILS_ENHANCED toggles (`--detail`), in pane order.
    pub details: Option<Vec<bool>>,
    /// Per-pane visibility / show-hide (`--show`), in pane order.
    pub visible: Option<Vec<bool>>,
    /// Per-pane Visualization-sync flags (`--tsync`), in pane order.
    pub tsync: Option<Vec<bool>>,
    /// Per-pane Geometry-sync flags (`--gsync`, rotation), in pane order.
    pub gsync: Option<Vec<bool>>,
    /// Per-pane display rotation in degrees (`--rotate`), in pane order.
    pub rotations: Option<Vec<f32>>,
    /// The Control media: the shared clip-bounds source, and — when it's a
    /// sequence — the timeline / playback driver (`--control`).
    pub control: Option<usize>,
    /// Inclusive playback loop range `LO,HI` (`--loop`), 0-based.
    pub loop_range: Option<(usize, usize)>,
}

/// Parse the arguments after argv[0].
pub fn parse(args: Vec<String>) -> Cli {
    let mut inputs = Vec::new();
    let mut view = ViewState::default();
    let mut i = 0;
    while i < args.len() {
        // Flags that take a value read the following argument and skip it.
        let next = |i: usize| args.get(i + 1).map(String::as_str);
        match args[i].as_str() {
            "-h" | "--help" => {
                print!("{}", help_text());
                return Cli::Exit(0);
            }
            "-V" | "--version" => {
                println!("cim {}", env!("CARGO_PKG_VERSION"));
                return Cli::Exit(0);
            }
            "--complete" => {
                let word = args.get(i + 1).map(String::as_str).unwrap_or("");
                for c in complete(word) {
                    println!("{c}");
                }
                return Cli::Exit(0);
            }
            "--completions" => {
                let shell = args.get(i + 1).map(String::as_str).unwrap_or("");
                match completion_script(shell) {
                    Some(s) => {
                        print!("{s}");
                        return Cli::Exit(0);
                    }
                    None => {
                        eprintln!("cim: unknown shell '{shell}' (try: bash, powershell)");
                        return Cli::Exit(2);
                    }
                }
            }
            "--mode" => {
                view.mode = next(i).and_then(parse_mode);
                i += 1;
            }
            "--cols" => {
                view.cols = next(i).and_then(|s| s.parse().ok());
                i += 1;
            }
            "--zoom" => {
                view.zoom = next(i).and_then(|s| s.parse().ok());
                i += 1;
            }
            "--center" => {
                view.center = next(i).and_then(parse_pair);
                i += 1;
            }
            "--frame" => {
                view.frame = next(i).and_then(|s| s.parse().ok());
                i += 1;
            }
            "--pane" => {
                view.pane = next(i).and_then(|s| s.parse().ok());
                i += 1;
            }
            "--ab" => {
                view.ab = next(i).and_then(parse_ab);
                i += 1;
            }
            "--tone" => {
                view.tones = next(i).and_then(parse_tones);
                i += 1;
            }
            "--clip" => {
                view.clips = next(i).and_then(parse_clips);
                i += 1;
            }
            "--share-clip" => {
                view.share_clip = next(i).and_then(parse_details);
                i += 1;
            }
            "--detail" => {
                view.details = next(i).and_then(parse_details);
                i += 1;
            }
            "--show" => {
                view.visible = next(i).and_then(parse_details);
                i += 1;
            }
            "--tsync" => {
                view.tsync = next(i).and_then(parse_details);
                i += 1;
            }
            "--gsync" => {
                view.gsync = next(i).and_then(parse_details);
                i += 1;
            }
            "--rotate" => {
                view.rotations = next(i).and_then(parse_floats);
                i += 1;
            }
            "--control" => {
                view.control = next(i).and_then(|s| s.parse().ok());
                i += 1;
            }
            "--loop" => {
                view.loop_range = next(i).and_then(parse_uint_pair);
                i += 1;
            }
            other => expand_arg(other, &mut inputs),
        }
        i += 1;
    }
    Cli::Run { inputs, view }
}

/// Parse a `--mode` value (case-insensitive).
fn parse_mode(s: &str) -> Option<ViewMode> {
    match s.to_ascii_lowercase().as_str() {
        "grid" => Some(ViewMode::Grid),
        "single" => Some(ViewMode::Single),
        "ab" | "a/b" => Some(ViewMode::Ab),
        _ => None,
    }
}

/// Parse `x,y` into a float pair (used by `--center`).
fn parse_pair(s: &str) -> Option<(f32, f32)> {
    let (a, b) = s.split_once(',')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// Parse `a,b,split` (0-based pane indices + 0..1 divider) for `--ab`.
fn parse_ab(s: &str) -> Option<(usize, usize, f32)> {
    let mut it = s.split(',');
    let a = it.next()?.trim().parse().ok()?;
    let b = it.next()?.trim().parse().ok()?;
    let split = it.next()?.trim().parse().ok()?;
    Some((a, b, split))
}

/// Parse `LO,HI` into a `usize` pair (used by `--loop`).
fn parse_uint_pair(s: &str) -> Option<(usize, usize)> {
    let (a, b) = s.split_once(',')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// Parse a comma-separated per-pane tone list for `--tone` (case-insensitive).
/// Any unrecognised token makes the whole flag ignored.
fn parse_tones(s: &str) -> Option<Vec<Tone>> {
    s.split(',')
        .map(|t| match t.trim().to_ascii_lowercase().as_str() {
            // `linearclip`/`clip` are deprecated aliases (the clip is now the
            // separate `--clip` flag); accept them as plain Linear.
            "linear" | "linearclip" | "clip" => Some(Tone::Linear),
            "lutalpha" | "lut_alpha" => Some(Tone::LutAlpha),
            "colormap" => Some(Tone::Colormap(crate::palette::Palette::default())),
            other if other.starts_with("colormap:") => {
                crate::palette::Palette::from_token(&other["colormap:".len()..]).map(Tone::Colormap)
            }
            _ => None,
        })
        .collect()
}

/// Parse a comma-separated per-pane Linear-clip list for `--clip`. Each token is
/// `off` (clip disabled) or a percentile number (clip enabled at that percent,
/// per tail). Any unrecognised token makes the whole flag ignored.
fn parse_clips(s: &str) -> Option<Vec<ClipSpec>> {
    s.split(',')
        .map(|t| match t.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Some(ClipSpec::Off),
            num => num.parse::<f32>().ok().map(ClipSpec::On),
        })
        .collect()
}

/// Parse a comma-separated per-pane float list for `--rotate` (degrees). Any
/// unparseable token makes the whole flag ignored.
fn parse_floats(s: &str) -> Option<Vec<f32>> {
    s.split(',').map(|t| t.trim().parse().ok()).collect()
}

/// Parse a comma-separated per-pane on/off list (`1`/`0`), shared by `--detail`,
/// `--share-clip`, `--show` and `--tsync`.
fn parse_details(s: &str) -> Option<Vec<bool>> {
    Some(
        s.split(',')
            .map(|t| matches!(t.trim(), "1" | "true" | "on"))
            .collect(),
    )
}

fn help_text() -> String {
    format!(
        "\
cim {ver} — Compare Images & Sequences

Lossless side-by-side viewer for images and sequences.

USAGE:
    cim [OPTIONS] [FILES|SEQUENCES]...

ARGS:
    <FILES|SEQUENCES|DIRS>...
        Any number of images, sequences, videos or directories to open
        ({exts}).
        A numbered run may be given compactly as PREFIX%0Xu SUFFIX,START,END,
        e.g. frame_%05u.tif,0,12 expands to frame_00000.tif .. frame_00012.tif.
        A directory (e.g. `cim folder`) opens every loadable image inside it,
        sorted alphabetically, concatenated into one pane. Videos ({vexts};
        decoded via the ffmpeg CLI, which must be on the PATH) always open as
        one pane each — including those found in a directory.
        A compute:<kind>:<srcs>[:auto] token (kind = mean|std|diff; srcs = one
        pane index, or A,B for diff) recreates a Compute pane; it is normally
        generated for you by the \"View cmd\" panel, not typed by hand.

OPTIONS:
    -h, --help                 Print this help and exit
    -V, --version              Print version and exit
        --complete <WORD>      List loadable completions for WORD, one per line
                               (used by the shell completers below; consecutive
                               numbered files collapse into the compact
                               PREFIX%0Xu SUFFIX,START,END form)
        --completions <SHELL>  Print a completion script for SHELL to stdout
                               (bash | powershell)

VIEW STATE:
    These reproduce a saved viewpoint and are normally generated for you by the
    in-app \"View cmd\" panel (⧉ Copy). All indices are 0-based.

        --mode <grid|single|ab>  Initial layout
        --cols <N>               Grid columns
        --zoom <F>               Shared zoom (screen px per image px)
        --center <X,Y>           Shared view centre, in image pixels
        --frame <N>              Timeline frame to show
        --pane <N>               Focused pane
        --ab <A,B,SPLIT>         A/B operands and 0..1 divider position
        --tone <T,T,...>         Per-pane tone: linear | lutalpha |
                               colormap[:viridis|turbo|diverging]
        --clip <C,C,...>         Per-pane Linear clip: off | PERCENT (each tail)
        --share-clip <B,B,...>   Per-pane share Control media's bounds (1/0)
        --detail <B,B,...>       Per-pane DETAILS_ENHANCED toggles (1/0)
        --show <B,B,...>         Per-pane visibility / show-hide (1/0)
        --tsync <B,B,...>        Per-pane Visualization-sync toggles (1/0)
        --gsync <B,B,...>        Per-pane Geometry-sync toggles (rotation) (1/0)
        --rotate <D,D,...>       Per-pane display rotation in degrees (-180..180)
        --control <N>            Control media: shared clip source (+ timeline if a sequence)
        --loop <LO,HI>           Inclusive playback loop range (0-based)

SHELL COMPLETION:
    bash         eval \"$(cim --completions bash)\"
    PowerShell   cim --completions powershell | Out-String | Invoke-Expression
",
        ver = env!("CARGO_PKG_VERSION"),
        exts = LOADABLE_EXTS.join(", "),
        vexts = VIDEO_EXTS.join(", "),
    )
}

// ---- sequence-token expansion -------------------------------------------

/// Turn one positional argument into an `Input`. A **directory** becomes one
/// concatenated sequence of the loadable files it contains (alphabetical order);
/// a sequence token naming two or more files becomes a single `Sequence`; a token
/// that resolves to one file, or any plain path, becomes a `Single`.
fn expand_arg(arg: &str, out: &mut Vec<Input>) {
    // A `compute:…` token recreates a Compute pane (emitted by the view command
    // for a computed pane). Recognised before any filesystem probing so it's
    // never mistaken for a path. (Deliberately not prefixed with `@`: a leading
    // `@` is PowerShell's splatting operator and would mangle the argument before
    // it reaches us.)
    if let Some(input) = parse_compute_token(arg) {
        out.push(input);
        return;
    }
    // A directory opens all the loadable files directly inside it, sorted
    // alphabetically, as one concatenated pane — the same downstream path as a
    // numbered token, but the frames come from a listing so any naming works.
    // The arg (not a normalised path) is kept as the token so the view command
    // round-trips `cim folder` verbatim.
    if Path::new(arg).is_dir() {
        out.extend(dir_inputs(Path::new(arg), arg));
        return;
    }
    match expand_sequence_token(arg) {
        // A numbered run of videos never becomes a sequence — each video is
        // already a timeline of its own, so open one pane per file.
        Some(files) if files.len() >= 2 && !is_video(&files[0].to_string_lossy()) => {
            out.push(Input::Sequence {
                token: arg.to_string(),
                files,
            })
        }
        Some(files) if files.len() >= 2 => out.extend(files.into_iter().map(Input::Single)),
        Some(mut files) => out.push(Input::Single(files.pop().unwrap_or_default())),
        None => out.push(Input::Single(PathBuf::from(arg))),
    }
}

/// Inputs for a directory argument: the **image** files inside it as one
/// concatenated sequence (`token` names it, so the view command round-trips),
/// plus one `Single` per **video** file (alphabetical, after the sequence).
fn dir_inputs(dir: &Path, token: &str) -> Vec<Input> {
    let (videos, mut images): (Vec<PathBuf>, Vec<PathBuf>) = list_dir_files(dir)
        .into_iter()
        .partition(|p| is_video(&p.to_string_lossy()));
    let mut out = Vec::new();
    match images.len() {
        // Nothing loadable at all: fall through as a plain path so opening
        // surfaces a clear "failed to open" error rather than silently doing
        // nothing. (Any videos present still open below.)
        0 if videos.is_empty() => out.push(Input::Single(dir.to_path_buf())),
        0 => {}
        1 => out.push(Input::Single(images.pop().unwrap())),
        _ => out.push(Input::Sequence {
            token: token.to_string(),
            files: images,
        }),
    }
    out.extend(videos.into_iter().map(Input::Single));
    out
}

/// Build the `Input`s for one filesystem path (from a drag-and-drop or the file
/// dialog): a **directory** expands to a concatenated sequence of its image
/// files (alphabetical) plus one pane per video, everything else is a plain
/// `Single`. Shares the folder expansion with the CLI's `expand_arg`.
pub fn inputs_for_path(path: PathBuf) -> Vec<Input> {
    if path.is_dir() {
        let token = path.to_string_lossy().into_owned();
        dir_inputs(&path, &token)
    } else {
        vec![Input::Single(path)]
    }
}

/// List the loadable files directly inside `dir`, sorted **alphabetically** by
/// path (which, all being in the same directory, orders them by file name — the
/// order that defines the concatenated frame sequence). Sub-directories and files
/// whose extension isn't loadable (e.g. a stray `.txt`/`.json`) are skipped.
fn list_dir_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| is_loadable(&n.to_string_lossy()))
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    files
}

/// Parse a `compute:<kind>:<srcs>[:auto]` token into an `Input::Compute`, or
/// `None` when `arg` isn't such a token. `<kind>` is `mean`/`std`/`diff`;
/// `<srcs>` is one pane index for the reductions or `A,B` for `diff`; a trailing
/// `:auto` restores the auto-refresh toggle. Indices are 0-based over the pane
/// list and resolved to panes once they all exist.
fn parse_compute_token(arg: &str) -> Option<Input> {
    let rest = arg.strip_prefix("compute:")?;
    let mut segs = rest.split(':');
    let kind = crate::media::Reduce::from_token(segs.next()?)?;
    let srcs = segs.next()?;
    let auto = matches!(segs.next(), Some("auto"));
    let (a, b) = match kind {
        crate::media::Reduce::Diff => {
            let (a, b) = srcs.split_once(',')?;
            (a.trim().parse().ok()?, Some(b.trim().parse().ok()?))
        }
        _ => (srcs.trim().parse().ok()?, None),
    };
    Some(Input::Compute { kind, a, b, auto })
}

/// Expand a `PREFIX%0Xu SUFFIX,START,END` token into the files it stands for, or
/// return `None` when `arg` isn't a well-formed sequence token. The printf-style
/// file name (including any suffix and extension) comes first, then the inclusive
/// range — e.g. `sequences_%05u.tif,4,15` → `sequences_00004.tif` …
/// `sequences_00015.tif`.
pub fn expand_sequence_token(arg: &str) -> Option<Vec<PathBuf>> {
    let (prefix, width, rest) = split_at_specifier(arg)?;
    // `rest` is `SUFFIX,START,END`: the last two comma fields are the range, and
    // everything before them is the file-name suffix (which may be empty).
    let (head, end) = rest.rsplit_once(',')?;
    let (suffix, start) = head.rsplit_once(',')?;
    let start: usize = start.parse().ok()?;
    let end: usize = end.parse().ok()?;
    if end < start {
        return None;
    }
    let files = (start..=end)
        .map(|n| PathBuf::from(format!("{prefix}{n:0width$}{suffix}")))
        .collect();
    Some(files)
}

/// Split at a `%0Xu` (or `%Xu` / `%u`) conversion, returning
/// `(prefix, pad_width, rest_after_the_u)`.
fn split_at_specifier(s: &str) -> Option<(&str, usize, &str)> {
    let pct = s.find('%')?;
    let prefix = &s[..pct];
    let after = &s[pct + 1..];
    let bytes = after.as_bytes();
    let mut j = 0;
    if bytes.first() == Some(&b'0') {
        j += 1; // zero-pad flag
    }
    let wstart = j;
    while bytes.get(j).is_some_and(u8::is_ascii_digit) {
        j += 1;
    }
    let width = if j > wstart {
        after[wstart..j].parse().ok()?
    } else {
        0
    };
    if bytes.get(j) != Some(&b'u') {
        return None;
    }
    Some((prefix, width, &after[j + 1..]))
}

// ---- completion ----------------------------------------------------------

/// Completion candidates for the partial word `word`: loadable files (with
/// consecutive numbered runs collapsed into a compact token), plus directories
/// so the user can descend into them.
pub fn complete(word: &str) -> Vec<String> {
    // Split the typed word into the directory part (kept verbatim so the shell
    // replaces the whole word) and the partial base name we match against.
    let sep = word.rfind(['/', '\\']);
    let dir_disp = match sep {
        Some(i) => &word[..=i],
        None => "",
    };
    let partial = match sep {
        Some(i) => &word[i + 1..],
        None => word,
    };
    let read_root = if dir_disp.is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(dir_disp)
    };

    let Ok(entries) = std::fs::read_dir(&read_root) else {
        return Vec::new();
    };

    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut videos = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !starts_with_ci(&name, partial) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            dirs.push(format!("{dir_disp}{name}{}", std::path::MAIN_SEPARATOR));
        } else if is_video(&name) {
            // Videos are never grouped into a `%0Xu` token (each is its own
            // timeline), so list them literally.
            videos.push(format!("{dir_disp}{name}"));
        } else if is_loadable(&name) {
            files.push(name);
        }
    }

    let mut out = group_files(&files, dir_disp);
    out.append(&mut videos);
    out.append(&mut dirs);
    out.sort();
    out
}

/// Collapse consecutive numbered files into compact tokens; anything else is
/// listed literally (with the typed directory prefix re-attached).
fn group_files(files: &[String], dir_disp: &str) -> Vec<String> {
    use std::collections::BTreeMap;

    // Bucket by (prefix, pad width, suffix); anything without a trailing number
    // is emitted as-is.
    let mut buckets: BTreeMap<(String, usize, String), Vec<usize>> = BTreeMap::new();
    let mut out = Vec::new();
    for name in files {
        match split_index(name) {
            Some((prefix, idx, width, suffix)) => {
                buckets
                    .entry((prefix, width, suffix))
                    .or_default()
                    .push(idx);
            }
            None => out.push(format!("{dir_disp}{name}")),
        }
    }

    for ((prefix, width, suffix), mut idxs) in buckets {
        idxs.sort_unstable();
        idxs.dedup();
        // Emit each maximal contiguous run: length >= 2 -> compact token,
        // singletons -> the plain file name.
        let mut i = 0;
        while i < idxs.len() {
            let start = idxs[i];
            let mut j = i;
            while j + 1 < idxs.len() && idxs[j + 1] == idxs[j] + 1 {
                j += 1;
            }
            let end = idxs[j];
            if end > start {
                out.push(format!(
                    "{dir_disp}{prefix}%0{width}u{suffix},{start},{end}"
                ));
            } else {
                out.push(format!("{dir_disp}{prefix}{start:0width$}{suffix}"));
            }
            i = j + 1;
        }
    }
    out
}

/// Split a file name at its last run of digits: `(prefix, index, width, suffix)`.
/// `suffix` keeps the extension, so `frame_00012.tif` → (`frame_`, 12, 5, `.tif`).
fn split_index(name: &str) -> Option<(String, usize, usize, String)> {
    let bytes = name.as_bytes();
    let mut end = bytes.len();
    while end > 0 && !bytes[end - 1].is_ascii_digit() {
        end -= 1;
    }
    if end == 0 {
        return None; // no digits at all
    }
    let mut start = end;
    while start > 0 && bytes[start - 1].is_ascii_digit() {
        start -= 1;
    }
    let digits = &name[start..end];
    let idx = digits.parse().ok()?;
    Some((
        name[..start].to_string(),
        idx,
        digits.len(),
        name[end..].to_string(),
    ))
}

fn is_loadable(name: &str) -> bool {
    Path::new(name)
        .extension()
        .map(|e| {
            let e = e.to_string_lossy().to_lowercase();
            LOADABLE_EXTS.contains(&e.as_str()) || VIDEO_EXTS.contains(&e.as_str())
        })
        .unwrap_or(false)
}

fn is_video(name: &str) -> bool {
    Path::new(name)
        .extension()
        .map(|e| VIDEO_EXTS.contains(&e.to_string_lossy().to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Case-insensitive prefix test (Windows file names are case-insensitive).
fn starts_with_ci(name: &str, prefix: &str) -> bool {
    name.len() >= prefix.len()
        && name
            .chars()
            .zip(prefix.chars())
            .all(|(a, b)| a.eq_ignore_ascii_case(&b))
}

// ---- completion scripts --------------------------------------------------

fn completion_script(shell: &str) -> Option<&'static str> {
    match shell {
        "bash" => Some(BASH_COMPLETION),
        "powershell" | "pwsh" => Some(POWERSHELL_COMPLETION),
        _ => None,
    }
}

const BASH_COMPLETION: &str = r#"_cim() {
    local cur="${COMP_WORDS[COMP_CWORD]}"
    local IFS=$'\n'
    COMPREPLY=( $(cim --complete "$cur") )
    compopt -o filenames
    if [[ ${#COMPREPLY[@]} -eq 1 && "${COMPREPLY[0]}" == *\\ ]]; then
        compopt -o nospace
    fi
}
complete -F _cim cim
"#;

const POWERSHELL_COMPLETION: &str = r#"Register-ArgumentCompleter -Native -CommandName cim -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)
    cim --complete "$wordToComplete" | ForEach-Object {
        $full = $_
        $leaf = $full.TrimEnd('\', '/') -replace '.*[\\/]', ''
        if ($full -match '[\\/]$') {
            $leaf = "$leaf/"
            $type = 'ProviderContainer'
        } else {
            $type = 'ParameterValue'
        }
        [System.Management.Automation.CompletionResult]::new($full, $leaf, $type, $full)
    }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_padded_range() {
        let files = expand_sequence_token("frame_%05u.tif,0,12").unwrap();
        assert_eq!(files.len(), 13);
        assert_eq!(files[0], PathBuf::from("frame_00000.tif"));
        assert_eq!(files[12], PathBuf::from("frame_00012.tif"));
    }

    #[test]
    fn non_tokens_pass_through() {
        assert!(expand_sequence_token("plain.png").is_none());
        assert!(expand_sequence_token("C:\\imgs\\a.tif").is_none());
    }

    #[test]
    fn groups_contiguous_run() {
        let files = vec![
            "f_000.tif".to_string(),
            "f_001.tif".to_string(),
            "f_002.tif".to_string(),
            "solo.png".to_string(),
        ];
        let mut out = group_files(&files, "");
        out.sort();
        assert_eq!(
            out,
            vec!["f_%03u.tif,0,2".to_string(), "solo.png".to_string()]
        );
    }

    #[test]
    fn parses_view_flags() {
        let args = "a.tif b.tif --mode ab --cols 3 --zoom 2.5 --center 10,20 \
                    --frame 4 --pane 1 --ab 0,1,0.25"
            .split(' ')
            .map(String::from)
            .collect();
        let Cli::Run { inputs, view } = parse(args) else {
            panic!("expected Run");
        };
        assert_eq!(inputs.len(), 2);
        assert!(matches!(view.mode, Some(ViewMode::Ab)));
        assert_eq!(view.cols, Some(3));
        assert_eq!(view.zoom, Some(2.5));
        assert_eq!(view.center, Some((10.0, 20.0)));
        assert_eq!(view.frame, Some(4));
        assert_eq!(view.pane, Some(1));
        assert_eq!(view.ab, Some((0, 1, 0.25)));
    }

    #[test]
    fn parses_tone_detail_loop() {
        let args = "a.tif b.tif --tone linear,lutalpha --clip 0.02,off --detail 0,1 --loop 3,9"
            .split(' ')
            .map(String::from)
            .collect();
        let Cli::Run { view, .. } = parse(args) else {
            panic!("expected Run");
        };
        assert!(matches!(
            view.tones.as_deref(),
            Some([Tone::Linear, Tone::LutAlpha])
        ));
        assert!(matches!(
            view.clips.as_deref(),
            Some([ClipSpec::On(p), ClipSpec::Off]) if (p - 0.02).abs() < 1e-6
        ));
        assert_eq!(view.details, Some(vec![false, true]));
        assert_eq!(view.loop_range, Some((3, 9)));
    }

    #[test]
    fn parses_share_clip() {
        let args = "a.tif b.tif --share-clip 1,0"
            .split(' ')
            .map(String::from)
            .collect();
        let Cli::Run { view, .. } = parse(args) else {
            panic!("expected Run");
        };
        assert_eq!(view.share_clip, Some(vec![true, false]));
    }

    #[test]
    fn parses_colormap_tone() {
        use crate::palette::Palette;
        let args = "a.tif b.tif c.tif --tone colormap,colormap:viridis,lutalpha"
            .split(' ')
            .map(String::from)
            .collect();
        let Cli::Run { view, .. } = parse(args) else {
            panic!("expected Run");
        };
        assert_eq!(
            view.tones.as_deref(),
            Some(
                [
                    Tone::Colormap(Palette::Turbo), // bare = default palette
                    Tone::Colormap(Palette::Viridis),
                    Tone::LutAlpha,
                ]
                .as_slice()
            )
        );
        // An unknown palette drops the whole flag.
        let bad = "a.tif --tone colormap:bogus"
            .split(' ')
            .map(String::from)
            .collect();
        let Cli::Run { view, .. } = parse(bad) else {
            panic!("expected Run");
        };
        assert!(view.tones.is_none());
    }

    #[test]
    fn deprecated_linearclip_maps_to_linear() {
        let args = "a.tif --tone linearclip"
            .split(' ')
            .map(String::from)
            .collect();
        let Cli::Run { view, .. } = parse(args) else {
            panic!("expected Run");
        };
        assert!(matches!(view.tones.as_deref(), Some([Tone::Linear])));
    }

    #[test]
    fn parses_show_tsync_control() {
        let args = "a.tif b.tif --show 1,0 --tsync 0,1 --gsync 1,0 --control 1"
            .split(' ')
            .map(String::from)
            .collect();
        let Cli::Run { view, .. } = parse(args) else {
            panic!("expected Run");
        };
        assert_eq!(view.visible, Some(vec![true, false]));
        assert_eq!(view.tsync, Some(vec![false, true]));
        assert_eq!(view.gsync, Some(vec![true, false]));
        assert_eq!(view.control, Some(1));
    }

    #[test]
    fn parses_compute_tokens() {
        use crate::media::Reduce;
        let Cli::Run { inputs, .. } = parse(vec![
            "a.tif".into(),
            "b.tif".into(),
            "compute:diff:0,1:auto".into(),
            "compute:mean:0".into(),
        ]) else {
            panic!("expected Run");
        };
        assert_eq!(inputs.len(), 4);
        assert!(matches!(
            inputs[2],
            Input::Compute {
                kind: Reduce::Diff,
                a: 0,
                b: Some(1),
                auto: true
            }
        ));
        assert!(matches!(
            inputs[3],
            Input::Compute {
                kind: Reduce::Mean,
                a: 0,
                b: None,
                auto: false
            }
        ));
        // A malformed token isn't silently turned into a phantom pane — it just
        // falls through to a (non-existent) path Single, like any other arg.
        let Cli::Run { inputs, .. } = parse(vec!["compute:bogus:0".into()]) else {
            panic!("expected Run");
        };
        assert!(matches!(inputs[0], Input::Single(_)));
    }

    #[test]
    fn parses_rotate() {
        let args = "a.tif b.tif --rotate -90,45.5"
            .split(' ')
            .map(String::from)
            .collect();
        let Cli::Run { view, .. } = parse(args) else {
            panic!("expected Run");
        };
        assert_eq!(view.rotations, Some(vec![-90.0, 45.5]));
    }

    #[test]
    fn view_flags_absent_stay_none() {
        let Cli::Run { inputs, view } = parse(vec!["a.tif".into()]) else {
            panic!("expected Run");
        };
        assert_eq!(inputs.len(), 1);
        assert!(view.mode.is_none() && view.zoom.is_none() && view.ab.is_none());
    }

    #[test]
    fn token_becomes_one_sequence_input() {
        let Cli::Run { inputs, .. } = parse(vec!["frame_%03u.png,0,11".into(), "solo.png".into()])
        else {
            panic!("expected Run");
        };
        assert_eq!(inputs.len(), 2);
        match &inputs[0] {
            Input::Sequence { token, files } => {
                assert_eq!(token, "frame_%03u.png,0,11");
                assert_eq!(files.len(), 12);
            }
            _ => panic!("first input should be a sequence"),
        }
        assert!(matches!(inputs[1], Input::Single(_)));
    }

    #[test]
    fn multiple_tokens_each_become_a_sequence() {
        // A big video split into parts: two independent numbered runs, each its
        // own token, must open as two separate sequences (two panes).
        let Cli::Run { inputs, .. } = parse(vec![
            "partA_%03u.png,0,49".into(),
            "partB_%03u.png,0,99".into(),
        ]) else {
            panic!("expected Run");
        };
        assert_eq!(inputs.len(), 2);
        match (&inputs[0], &inputs[1]) {
            (
                Input::Sequence {
                    token: t0,
                    files: f0,
                },
                Input::Sequence {
                    token: t1,
                    files: f1,
                },
            ) => {
                assert_eq!(t0, "partA_%03u.png,0,49");
                assert_eq!(f0.len(), 50);
                assert_eq!(t1, "partB_%03u.png,0,99");
                assert_eq!(f1.len(), 100);
            }
            _ => panic!("both inputs should be sequences"),
        }
    }

    #[test]
    fn directory_becomes_one_concat_sequence() {
        use std::fs;
        // A unique temp dir with a mix of loadable files and a non-image file.
        let dir = std::env::temp_dir().join(format!("cim_dir_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("b_002.tif"), b"").unwrap();
        fs::write(dir.join("a_001.tif"), b"").unwrap();
        fs::write(dir.join("c.png"), b"").unwrap();
        fs::write(dir.join("notes.txt"), b"").unwrap(); // must be ignored

        let mut out = Vec::new();
        expand_arg(dir.to_str().unwrap(), &mut out);
        assert_eq!(out.len(), 1);
        match &out[0] {
            Input::Sequence { token, files } => {
                assert_eq!(token, dir.to_str().unwrap());
                // .txt excluded; the rest sorted alphabetically by name.
                assert_eq!(files.len(), 3);
                assert!(files[0].ends_with("a_001.tif"));
                assert!(files[1].ends_with("b_002.tif"));
                assert!(files[2].ends_with("c.png"));
            }
            _ => panic!("a directory should open as one sequence"),
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn single_file_directory_becomes_a_single() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("cim_dir_solo_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("only.tif"), b"").unwrap();
        fs::write(dir.join("readme.md"), b"").unwrap(); // ignored

        let mut out = Vec::new();
        expand_arg(dir.to_str().unwrap(), &mut out);
        assert_eq!(out.len(), 1);
        match &out[0] {
            Input::Single(p) => assert!(p.ends_with("only.tif")),
            _ => panic!("a one-file directory should open as a single image"),
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn directory_videos_open_one_pane_each() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("cim_dir_video_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a_001.tif"), b"").unwrap();
        fs::write(dir.join("b_002.tif"), b"").unwrap();
        fs::write(dir.join("clip.mp4"), b"").unwrap();
        fs::write(dir.join("other.avi"), b"").unwrap();

        let mut out = Vec::new();
        expand_arg(dir.to_str().unwrap(), &mut out);
        // The images concatenate into one sequence; each video is its own pane.
        assert_eq!(out.len(), 3);
        match &out[0] {
            Input::Sequence { files, .. } => assert_eq!(files.len(), 2),
            _ => panic!("images should open as one sequence"),
        }
        match (&out[1], &out[2]) {
            (Input::Single(a), Input::Single(b)) => {
                assert!(a.ends_with("clip.mp4"));
                assert!(b.ends_with("other.avi"));
            }
            _ => panic!("videos should each open as a single"),
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn video_run_token_opens_one_pane_per_file() {
        let mut out = Vec::new();
        expand_arg("clip_%02u.mp4,0,2", &mut out);
        assert_eq!(out.len(), 3);
        for (i, input) in out.iter().enumerate() {
            match input {
                Input::Single(p) => assert!(p.ends_with(format!("clip_{i:02}.mp4"))),
                _ => panic!("a video run must never become a sequence"),
            }
        }
    }

    #[test]
    fn videos_complete_literally_never_as_tokens() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("cim_complete_video_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("clip_001.mp4"), b"").unwrap();
        fs::write(dir.join("clip_002.mp4"), b"").unwrap();

        let word = format!("{}{}cl", dir.to_str().unwrap(), std::path::MAIN_SEPARATOR);
        let out = complete(&word);
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(out.iter().any(|c| c.ends_with("clip_001.mp4")));
        assert!(out.iter().any(|c| c.ends_with("clip_002.mp4")));
        assert!(out.iter().all(|c| !c.contains("%0")), "{out:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn splits_last_digit_run() {
        assert_eq!(
            split_index("frame_00012.tif"),
            Some(("frame_".into(), 12, 5, ".tif".into()))
        );
    }
}
