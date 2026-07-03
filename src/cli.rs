//! Command-line handling: `--help`, shell completion, and expansion of the
//! compact numbered-sequence token used both on the command line and by the
//! completion suggestions.
//!
//! A numbered run is written `PREFIX%0Nd,START,END.EXT` — for example
//! `frame_%05d,0,12.tif` stands for `frame_00000.tif` … `frame_00012.tif`.
//! The GUI never has to enumerate a directory: the token carries the whole
//! range, so the shell offers it on Tab and the app expands it on launch.

use std::path::{Path, PathBuf};

/// File extensions the app can open (stills + multi-page TIFF). Shared by the
/// file dialog and the completion filter so they never drift apart.
pub const LOADABLE_EXTS: &[&str] = &["tif", "tiff", "png", "jpg", "jpeg", "bmp", "webp"];

/// Outcome of parsing argv.
pub enum Cli {
    /// Launch the GUI, opening these inputs at the initial view described by
    /// `view` (empty unless `--mode`/`--zoom`/… were given).
    Run { inputs: Vec<Input>, view: ViewState },
    /// A CLI-only request (help / completion) was handled; exit with this code.
    Exit(i32),
}

/// One thing to open. A bare path becomes a single media; a compact
/// `PREFIX%0Nd,START,END.EXT` token that names two or more files becomes **one**
/// image sequence (a single pane), not a pane per file.
pub enum Input {
    Single(PathBuf),
    /// A numbered still sequence: `token` is the original compact argument (so
    /// the view-command panel can round-trip it) and `files` are its frames.
    Sequence { token: String, files: Vec<PathBuf> },
}

/// Which layout a `--mode` flag selects. Mirrors the app's `Mode` but lives here
/// so the CLI stays decoupled from the GUI module.
#[derive(Clone, Copy, PartialEq)]
pub enum ViewMode {
    Grid,
    Single,
    Ab,
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

fn help_text() -> String {
    format!(
        "\
cim {ver} — Compare Images & Sequences

Lossless side-by-side viewer for images and sequences.

USAGE:
    cim [OPTIONS] [FILES|SEQUENCES]...

ARGS:
    <FILES|SEQUENCES>...
        Any number of images or sequences to open ({exts}).
        A numbered run may be given compactly as PREFIX%0Nd,START,END.EXT,
        e.g. frame_%05d,0,12.tif expands to frame_00000.tif .. frame_00012.tif.

OPTIONS:
    -h, --help                 Print this help and exit
    -V, --version              Print version and exit
        --complete <WORD>      List loadable completions for WORD, one per line
                               (used by the shell completers below; consecutive
                               numbered files collapse into the compact
                               PREFIX%0Nd,START,END.EXT form)
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

SHELL COMPLETION:
    bash         eval \"$(cim --completions bash)\"
    PowerShell   cim --completions powershell | Out-String | Invoke-Expression
",
        ver = env!("CARGO_PKG_VERSION"),
        exts = LOADABLE_EXTS.join(", "),
    )
}

// ---- sequence-token expansion -------------------------------------------

/// Turn one positional argument into an `Input`. A sequence token naming two or
/// more files becomes a single `Sequence`; a token that resolves to one file, or
/// any plain path, becomes a `Single`.
fn expand_arg(arg: &str, out: &mut Vec<Input>) {
    match expand_sequence_token(arg) {
        Some(files) if files.len() >= 2 => out.push(Input::Sequence {
            token: arg.to_string(),
            files,
        }),
        Some(mut files) => out.push(Input::Single(files.pop().unwrap_or_default())),
        None => out.push(Input::Single(PathBuf::from(arg))),
    }
}

/// Expand `PREFIX%0Nd,START,END.SUFFIX` into the files it stands for, or return
/// `None` when `arg` isn't a well-formed sequence token.
pub fn expand_sequence_token(arg: &str) -> Option<Vec<PathBuf>> {
    let (prefix, width, rest) = split_at_specifier(arg)?;
    let rest = rest.strip_prefix(',')?;
    let (start, rest) = take_uint(rest)?;
    let rest = rest.strip_prefix(',')?;
    let (end, suffix) = take_uint(rest)?;
    if end < start {
        return None;
    }
    let files = (start..=end)
        .map(|n| PathBuf::from(format!("{prefix}{n:0width$}{suffix}")))
        .collect();
    Some(files)
}

/// Split at a `%0Nd` (or `%Nd` / `%d`) conversion, returning
/// `(prefix, pad_width, rest_after_the_d)`.
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
    if bytes.get(j) != Some(&b'd') {
        return None;
    }
    Some((prefix, width, &after[j + 1..]))
}

/// Take a run of leading ASCII digits, returning `(digits, rest)`.
fn take_uint(s: &str) -> Option<(usize, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let value = s[..end].parse().ok()?;
    Some((value, &s[end..]))
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
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !starts_with_ci(&name, partial) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            dirs.push(format!("{dir_disp}{name}{}", std::path::MAIN_SEPARATOR));
        } else if is_loadable(&name) {
            files.push(name);
        }
    }

    let mut out = group_files(&files, dir_disp);
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
                buckets.entry((prefix, width, suffix)).or_default().push(idx);
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
                    "{dir_disp}{prefix}%0{width}d,{start},{end}{suffix}"
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
            LOADABLE_EXTS.contains(&e.as_str())
        })
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
}
complete -F _cim cim
"#;

const POWERSHELL_COMPLETION: &str = r#"Register-ArgumentCompleter -Native -CommandName cim -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)
    cim --complete "$wordToComplete" | ForEach-Object {
        [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)
    }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_padded_range() {
        let files = expand_sequence_token("frame_%05d,0,12.tif").unwrap();
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
        assert_eq!(out, vec!["f_%03d,0,2.tif".to_string(), "solo.png".to_string()]);
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
    fn view_flags_absent_stay_none() {
        let Cli::Run { inputs, view } = parse(vec!["a.tif".into()]) else {
            panic!("expected Run");
        };
        assert_eq!(inputs.len(), 1);
        assert!(view.mode.is_none() && view.zoom.is_none() && view.ab.is_none());
    }

    #[test]
    fn token_becomes_one_sequence_input() {
        let Cli::Run { inputs, .. } = parse(vec![
            "frame_%03d,0,11.png".into(),
            "solo.png".into(),
        ]) else {
            panic!("expected Run");
        };
        assert_eq!(inputs.len(), 2);
        match &inputs[0] {
            Input::Sequence { token, files } => {
                assert_eq!(token, "frame_%03d,0,11.png");
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
            "partA_%03d,0,49.png".into(),
            "partB_%03d,0,99.png".into(),
        ]) else {
            panic!("expected Run");
        };
        assert_eq!(inputs.len(), 2);
        match (&inputs[0], &inputs[1]) {
            (
                Input::Sequence { token: t0, files: f0 },
                Input::Sequence { token: t1, files: f1 },
            ) => {
                assert_eq!(t0, "partA_%03d,0,49.png");
                assert_eq!(f0.len(), 50);
                assert_eq!(t1, "partB_%03d,0,99.png");
                assert_eq!(f1.len(), 100);
            }
            _ => panic!("both inputs should be sequences"),
        }
    }

    #[test]
    fn splits_last_digit_run() {
        assert_eq!(
            split_index("frame_00012.tif"),
            Some(("frame_".into(), 12, 5, ".tif".into()))
        );
    }
}
