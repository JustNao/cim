//! Names that must not live in this repository, from a **`proprietary.yaml`**
//! that is never committed (see `.gitignore`):
//!
//! - the DAI **rotation column** names (csv) and keys (yaml) — [`crate::dai`];
//! - the three proprietary **operators**' display names, library file names and
//!   C symbol prefixes — [`crate::imageproc`].
//!
//! Two places supply it, the first found winning:
//!
//! 1. a `proprietary.yaml` **beside the executable**, else in the working
//!    directory (like `help.md`) — to change the names without a rebuild;
//! 2. the one **embedded at build time**: `build.rs` copies the crate root's
//!    `proprietary.yaml` (or `$CIM_PROPRIETARY`) into the binary, so a build made
//!    with it ships as a single file. A build without it embeds nothing.
//!
//! The names then live in the executable — readable with `strings`, as any
//! literal is; this keeps them out of the source, not out of the binary.
//!
//! Every entry is optional. Without either, the operators fall back to the
//! generic placeholder names the source has always used, and **no DAI column is
//! read as a rotation** (there is no generic name to fall back to). The format
//! is documented by the committed `proprietary.example.yaml`.
//!
//! Read **once**, before anything uses it (`init` in `main`); changing the file
//! takes a restart. A file that exists but can't be read is reported on the
//! status line (`load_error`), never fatal.

use std::path::PathBuf;
use std::sync::OnceLock;

use crate::dai::{yaml_entries, Value, YamlEntry};

/// The file's name, resolved beside the executable then in the working dir.
pub const FILE: &str = "proprietary.yaml";

/// One proprietary operator's names. `None` = the built-in placeholder.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Operator {
    /// Shown in the UI (tone picker, Settings, the Details toggle).
    pub name: Option<String>,
    /// The library's file name, looked up in the Library folder.
    pub library: Option<String>,
    /// The C symbol prefix: the library exports `<symbol>_create` / `_apply` /
    /// `_destroy`.
    pub symbol: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Proprietary {
    /// csv column names holding the camera rotation, in order of preference.
    pub csv_rotation: Vec<String>,
    /// yaml keys (flattened, `a.b`) holding the camera rotation.
    pub yaml_rotation: Vec<String>,
    pub lut_alpha: Operator,
    pub boost: Operator,
    pub details: Operator,
}

impl Proprietary {
    /// Parse the file's text. Unknown keys are ignored, so the file can grow
    /// ahead of the program reading it.
    pub fn parse(text: &str) -> Result<Proprietary, String> {
        let mut p = Proprietary::default();
        for (key, entry) in yaml_entries(text)? {
            let list = || -> Vec<String> {
                match &entry {
                    YamlEntry::List(items) => items.clone(),
                    YamlEntry::Value(Value::Empty) => Vec::new(),
                    YamlEntry::Value(v) => vec![scalar(v)],
                }
            };
            let text = || match &entry {
                YamlEntry::Value(Value::Empty) => None,
                YamlEntry::Value(v) => Some(scalar(v)),
                YamlEntry::List(items) => items.first().cloned(),
            };
            match key.as_str() {
                "dai.csv_rotation" => p.csv_rotation = list(),
                "dai.yaml_rotation" => p.yaml_rotation = list(),
                _ => {
                    let Some((op, field)) = key
                        .strip_prefix("operators.")
                        .and_then(|k| k.split_once('.'))
                    else {
                        continue;
                    };
                    let op = match op {
                        "lut_alpha" => &mut p.lut_alpha,
                        "boost" => &mut p.boost,
                        "details" => &mut p.details,
                        _ => continue,
                    };
                    match field {
                        "name" => op.name = text(),
                        "library" => op.library = text(),
                        "symbol" => op.symbol = text(),
                        _ => {}
                    }
                }
            }
        }
        Ok(p)
    }

    /// Stand-in names for the test suite, which must not carry the real ones.
    #[cfg(test)]
    fn for_tests() -> Proprietary {
        Proprietary {
            csv_rotation: vec!["camRotation".into(), "camRotationOld".into()],
            yaml_rotation: vec!["cam.roll".into()],
            ..Default::default()
        }
    }
}

/// A scalar as the text it was written as (a name like `2` is still a name).
fn scalar(v: &Value) -> String {
    match v {
        Value::Empty => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Text(t) => t.clone(),
    }
}

static NAMES: OnceLock<Proprietary> = OnceLock::new();
static ERROR: OnceLock<String> = OnceLock::new();
static SOURCE: OnceLock<String> = OnceLock::new();

/// The `proprietary.yaml` embedded at build time (empty when there was none).
const EMBEDDED: &str = include_str!(concat!(env!("OUT_DIR"), "/proprietary.yaml"));

/// Where `proprietary.yaml` is looked for, in order.
pub fn candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(PathBuf::from))
    {
        v.push(dir.join(FILE));
    }
    v.push(PathBuf::from(FILE));
    v
}

/// Read the names: a `proprietary.yaml` beside the executable / in the working
/// directory (first of [`candidates`]), else the embedded one. Call once at
/// startup, before the names are used. Neither existing is the generic build;
/// one that fails to parse is kept for [`load_error`].
pub fn init() {
    let found = candidates().into_iter().find(|p| p.is_file());
    let (label, read) = match found {
        Some(path) => (
            path.display().to_string(),
            std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|b| Proprietary::parse(&crate::dai::decode_text(&b))),
        ),
        None if !EMBEDDED.trim().is_empty() => (
            rust_i18n::t!("daicheck.embedded").into_owned(),
            Proprietary::parse(EMBEDDED),
        ),
        None => {
            let _ = NAMES.set(Proprietary::default());
            return;
        }
    };
    let names = match read {
        Ok(p) => p,
        Err(e) => {
            let _ = ERROR.set(format!("{label}: {e}"));
            Proprietary::default()
        }
    };
    let _ = SOURCE.set(label);
    let _ = NAMES.set(names);
}

/// The names in effect (the defaults until [`init`] runs; stand-ins in tests).
pub fn get() -> &'static Proprietary {
    NAMES.get_or_init(|| {
        #[cfg(test)]
        {
            Proprietary::for_tests()
        }
        #[cfg(not(test))]
        {
            Proprietary::default()
        }
    })
}

/// Where the names came from — a file's path, or "embedded at build" — when
/// any were found.
pub fn source() -> Option<&'static str> {
    SOURCE.get().map(String::as_str)
}

/// Why `proprietary.yaml` couldn't be used, when it exists but failed to read.
pub fn load_error() -> Option<&'static str> {
    ERROR.get().map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_names_lists_and_operators() {
        let text = "\
# names that stay out of the repository
dai:
  csv_rotation:
    - colA
    - colB
  yaml_rotation: [a.b, 'c']
operators:
  lut_alpha:
    name: Alpha
    library: libalpha.so
    symbol: alpha_op
  details:
    name: Fine
  unknown:
    name: ignored
other: ignored
";
        let p = Proprietary::parse(text).unwrap();
        assert_eq!(p.csv_rotation, ["colA", "colB"]);
        assert_eq!(p.yaml_rotation, ["a.b", "c"]);
        assert_eq!(
            p.lut_alpha,
            Operator {
                name: Some("Alpha".into()),
                library: Some("libalpha.so".into()),
                symbol: Some("alpha_op".into()),
            }
        );
        assert_eq!(p.details.name.as_deref(), Some("Fine"));
        assert_eq!(p.boost, Operator::default());
        // A single name is a one-item list.
        let one = Proprietary::parse("dai:\n  csv_rotation: colA\n").unwrap();
        assert_eq!(one.csv_rotation, ["colA"]);
        assert_eq!(Proprietary::parse("").unwrap(), Proprietary::default());
        assert!(Proprietary::parse("not yaml").is_err());
    }

    /// The committed example parses, and holds no more than placeholders.
    #[test]
    fn the_example_file_parses() {
        let text = include_str!("../proprietary.example.yaml");
        let p = Proprietary::parse(text).unwrap();
        assert!(!p.csv_rotation.is_empty() && !p.yaml_rotation.is_empty());
        assert!(p.lut_alpha.library.is_some());
    }
}
