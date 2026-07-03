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
    /// Launch the GUI, opening these already-expanded paths.
    Run(Vec<PathBuf>),
    /// A CLI-only request (help / completion) was handled; exit with this code.
    Exit(i32),
}

/// Parse the arguments after argv[0].
pub fn parse(args: Vec<String>) -> Cli {
    let mut paths = Vec::new();
    let mut i = 0;
    while i < args.len() {
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
            other => expand_arg(other, &mut paths),
        }
        i += 1;
    }
    Cli::Run(paths)
}

fn help_text() -> String {
    format!(
        "\
cim {ver} — Compare Images & Media

Lossless side-by-side viewer for images and multi-page TIFF sequences.

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

SHELL COMPLETION:
    bash         eval \"$(cim --completions bash)\"
    PowerShell   cim --completions powershell | Out-String | Invoke-Expression
",
        ver = env!("CARGO_PKG_VERSION"),
        exts = LOADABLE_EXTS.join(", "),
    )
}

// ---- sequence-token expansion -------------------------------------------

/// If `arg` is a sequence token, push every file it names; otherwise push it
/// as a plain path.
fn expand_arg(arg: &str, out: &mut Vec<PathBuf>) {
    match expand_sequence_token(arg) {
        Some(files) => out.extend(files),
        None => out.push(PathBuf::from(arg)),
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
    fn splits_last_digit_run() {
        assert_eq!(
            split_index("frame_00012.tif"),
            Some(("frame_".into(), 12, 5, ".tif".into()))
        );
    }
}
