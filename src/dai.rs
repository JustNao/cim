//! Auxiliary per-image information (**DAI**) found next to a media's files.
//!
//! Some acquisitions ship metadata beside the pixels: a **`.csv`** for a
//! sequence (one row per image, after a free-form header of variable length —
//! the column names are the first `;`-separated line as wide as the data rows,
//! see [`Table::parse_csv`]), or a
//! **`.yaml`/`.yml`** — or **`.dai`/`.dai2`**, yaml under another extension —
//! for a single image (one flat-or-nested dictionary). The file is matched by
//! name: the same stem (`seqA.tif` → `seqA.csv`, or `seqA_dai.csv`), or one
//! whose name components are a prefix of the other's (`seqA_variation.tif` →
//! `seqA.csv`).
//!
//! DAI is an **opportunity, never a requirement**: a media without one is not
//! an error, and nothing here reports a missing file. Only a file that *was*
//! matched but couldn't be read surfaces, as a transient status note.
//!
//! The data is kept whole and typed ([`Table`] of [`Value`]s, every column, every
//! row) rather than reduced to the one field cim uses today — the camera
//! **rotation** (radians), which the pane's display rotation adds by default —
//! and the DAI window graphs any column over the sequence. The rotation
//! column's *name* is not in this source: it comes from `proprietary.yaml`
//! ([`crate::proprietary`]), as a list of csv columns and yaml keys to try.
//!
//! Loading lists directories and parses files, which is real I/O on a shared
//! mount, so it runs on a dedicated thread ([`DaiLoader`]) mirroring
//! `crate::offsets`: results are keyed by pane id **and a generation**, so a
//! load that lands after a reload is recognised as stale and dropped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use rust_i18n::t;

use crate::media::Media;

/// One DAI cell. Cells are typed one by one (a numeric column may hold an
/// empty cell or a `NaN`, a text column a number); a column's [`ColType`] is
/// derived from its cells. **No booleans:** some columns flag with 0/1 and
/// others are ordinary integers that happen to hold only 0 and 1, which can't be
/// told apart — so `0`/`1` are ints and `true`/`false` text.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Empty,
    Int(i64),
    Float(f64),
    Text(String),
}

impl Value {
    /// Parse one scalar. `decimal_comma` also reads `0,5` as a float — the csv
    /// is `;`-separated precisely so a French-locale export can use the comma.
    pub fn parse(raw: &str, decimal_comma: bool) -> Value {
        let s = raw.trim();
        if s.is_empty() {
            return Value::Empty;
        }
        match s.to_ascii_lowercase().as_str() {
            "nan" | ".nan" => return Value::Float(f64::NAN),
            "inf" | ".inf" | "+inf" | "+.inf" | "infinity" => return Value::Float(f64::INFINITY),
            "-inf" | "-.inf" | "-infinity" => return Value::Float(f64::NEG_INFINITY),
            _ => {}
        }
        if let Ok(i) = s.parse::<i64>() {
            return Value::Int(i);
        }
        if let Ok(f) = s.parse::<f64>() {
            return Value::Float(f);
        }
        if decimal_comma && s.matches(',').count() == 1 && !s.contains('.') {
            if let Ok(f) = s.replace(',', ".").parse::<f64>() {
                return Value::Float(f);
            }
        }
        Value::Text(s.to_string())
    }

    /// The cell as a number, for plotting / numeric use: ints and floats (NaN
    /// included); `None` for empty and text cells.
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Value::Int(i) => Some(i as f64),
            Value::Float(f) => Some(f),
            Value::Empty | Value::Text(_) => None,
        }
    }

    /// The cell as the DAI panel shows it (`—` for empty).
    pub fn display(&self) -> String {
        match self {
            Value::Empty => "—".into(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => fmt_float(*f),
            Value::Text(t) => t.clone(),
        }
    }

    fn col_type(&self) -> ColType {
        match self {
            Value::Empty => ColType::Empty,
            Value::Int(_) => ColType::Int,
            Value::Float(_) => ColType::Float,
            Value::Text(_) => ColType::Text,
        }
    }
}

/// A float for display: plain with up to 6 decimals in the everyday range,
/// scientific outside it.
pub fn fmt_float(f: f64) -> String {
    if !f.is_finite() {
        return format!("{f}");
    }
    let a = f.abs();
    if a != 0.0 && !(1e-4..1e7).contains(&a) {
        format!("{f:.4e}")
    } else {
        let s = format!("{f:.6}");
        let s = s.trim_end_matches('0');
        s.strip_suffix('.').unwrap_or(s).to_string()
    }
}

/// What a whole column holds: the widest of its cells' types — ints only →
/// `Int`, any float among them → `Float`, any text → `Text`; empty cells don't
/// count, so an all-empty column is `Empty`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ColType {
    Empty,
    Int,
    Float,
    Text,
}

impl ColType {
    fn widen(self, other: ColType) -> ColType {
        self.max(other)
    }

    /// Plotable: an int or a float column.
    pub fn numeric(self) -> bool {
        matches!(self, ColType::Int | ColType::Float)
    }
}

/// Which kind of DAI file a table came from — it decides how its rows map to
/// frames: a csv has one row per image, a yaml describes every page of its file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Csv,
    Yaml,
}

impl Kind {
    fn of(path: &Path) -> Option<Kind> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "csv" => Some(Kind::Csv),
            // `.dai` / `.dai2` are yaml inside, under their own extension.
            "yaml" | "yml" | "dai" | "dai2" => Some(Kind::Yaml),
            _ => None,
        }
    }
}

/// One parsed DAI file: named columns and rows of cells (a yaml is one row,
/// nested keys flattened as `parent.child`). Every row has `columns.len()` cells.
#[derive(Clone, Debug)]
pub struct Table {
    pub path: PathBuf,
    pub kind: Kind,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    /// Index of the rotation column, resolved once (it is read every paint).
    rotation_col: Option<usize>,
}

impl Table {
    fn new(path: PathBuf, kind: Kind, columns: Vec<String>, rows: Vec<Vec<Value>>) -> Table {
        // The first of `proprietary.yaml`'s names the file has, compared
        // trimmed and case-insensitively (exports differ in both).
        let names = crate::proprietary::get();
        let keys = match kind {
            Kind::Csv => &names.csv_rotation,
            Kind::Yaml => &names.yaml_rotation,
        };
        let rotation_col = keys.iter().find_map(|key| {
            columns
                .iter()
                .position(|c| c.trim().eq_ignore_ascii_case(key.trim()))
        });
        Table {
            path,
            kind,
            columns,
            rows,
            rotation_col,
        }
    }

    /// Cell `(row, col)`, if the table has it.
    pub fn get(&self, row: usize, col: usize) -> Option<&Value> {
        self.rows.get(row)?.get(col)
    }

    /// Parse a csv: skip the free-form header up to the first line holding a
    /// `;`, which names the columns; every later non-blank line is a row.
    ///
    /// The header is the first `;` line **as wide as the data**: a free-form
    /// preamble may hold `;` too (`Date;2024-05-01`), so "the first line with a
    /// `;`" would take that for the column names. Width is the field count
    /// without trailing empty fields, and the data's is the most common width
    /// among the `;` lines (the rows far outnumber the preamble).
    pub fn parse_csv(path: PathBuf, text: &str) -> Result<Table, String> {
        let (at, header) =
            csv_header(text).ok_or_else(|| "no ';'-separated header line".to_string())?;
        let lines = text.lines().skip(at + 1);
        let columns: Vec<String> = split_csv_line(header)
            .into_iter()
            .map(|c| c.trim().to_string())
            .collect();
        let rows = lines
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let mut cells: Vec<Value> = split_csv_line(l)
                    .iter()
                    .map(|c| Value::parse(c, true))
                    .collect();
                cells.resize(columns.len(), Value::Empty);
                cells
            })
            .collect();
        Ok(Table::new(path, Kind::Csv, columns, rows))
    }

    /// Parse a yaml dictionary into a one-row table. A deliberately small
    /// subset — block mappings (nested by indentation, flattened `a.b`),
    /// scalars, comments, quoted strings; sequences and flow collections are
    /// kept as their raw text — which is all a per-image record needs, without
    /// a YAML dependency.
    pub fn parse_yaml(path: PathBuf, text: &str) -> Result<Table, String> {
        let (columns, row): (Vec<String>, Vec<Value>) = yaml_entries(text)?
            .into_iter()
            .map(|(k, e)| {
                let v = match e {
                    YamlEntry::Value(v) => v,
                    YamlEntry::List(items) => Value::Text(format!("[{}]", items.join(", "))),
                };
                (k, v)
            })
            .unzip();
        let rows = if columns.is_empty() {
            Vec::new()
        } else {
            vec![row]
        };
        Ok(Table::new(path, Kind::Yaml, columns, rows))
    }
}

/// One flattened yaml entry: a scalar, or a list of scalars (a block sequence
/// or a `[a, b]` flow list) kept as their text.
#[derive(Clone, Debug, PartialEq)]
pub enum YamlEntry {
    Value(Value),
    List(Vec<String>),
}

/// Read a yaml dictionary as `(flattened.key, entry)` pairs, in file order. A
/// deliberately small subset — block mappings (nested by indentation, keys
/// joined with `.`), scalars, comments, quoted strings, block and flow lists —
/// which is all a per-image record or `proprietary.yaml` needs, without a YAML
/// dependency. A key with nothing under it is an empty value.
pub fn yaml_entries(text: &str) -> Result<Vec<(String, YamlEntry)>, String> {
    let mut out: Vec<(String, YamlEntry)> = Vec::new();
    // (indent, key) of the open parent mappings.
    let mut stack: Vec<(usize, String)> = Vec::new();
    // The entry a block sequence may follow: (index in `out`, items, indent).
    let mut list: Option<(usize, Vec<String>, usize)> = None;
    let flush = |list: &mut Option<(usize, Vec<String>, usize)>,
                 out: &mut Vec<(String, YamlEntry)>| {
        // An item-less "list" was a nested mapping's parent: it stays empty.
        if let Some((c, items, _)) = list.take().filter(|(_, items, _)| !items.is_empty()) {
            out[c].1 = YamlEntry::List(items);
        }
    };
    for (n, raw) in text.lines().enumerate() {
        let line = strip_yaml_comment(raw);
        let body = line.trim_start();
        let trimmed = body.trim_end();
        if trimmed.is_empty() || trimmed == "---" || trimmed == "..." || trimmed.starts_with('%') {
            continue;
        }
        let indent = line.len() - body.len();
        if let Some(item) = trimmed
            .strip_prefix('-')
            .filter(|r| r.is_empty() || r.starts_with(' '))
        {
            match &mut list {
                Some((_, items, li)) if indent >= *li => {
                    items.push(unquote(item.trim()).to_string());
                    continue;
                }
                _ => return Err(format!("line {}: unexpected list item", n + 1)),
            }
        }
        flush(&mut list, &mut out);
        let Some((key, value)) = split_yaml_key(trimmed) else {
            return Err(format!("line {}: expected 'key: value'", n + 1));
        };
        while stack.last().is_some_and(|(i, _)| *i >= indent) {
            stack.pop();
        }
        let full = stack
            .iter()
            .map(|(_, k)| k.as_str())
            .chain(std::iter::once(key.as_str()))
            .collect::<Vec<_>>()
            .join(".");
        if value.is_empty() {
            // A nested mapping or a block sequence follows (or nothing).
            out.push((full, YamlEntry::Value(Value::Empty)));
            list = Some((out.len() - 1, Vec::new(), indent));
            stack.push((indent, key));
            continue;
        }
        let entry = if is_quoted(value) {
            YamlEntry::Value(Value::Text(unquote(value).to_string()))
        } else if let Some(inner) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
            YamlEntry::List(
                inner
                    .split(',')
                    .map(|i| unquote(i.trim()).to_string())
                    .filter(|i| !i.is_empty())
                    .collect(),
            )
        } else if matches!(value, "~" | "null" | "Null" | "NULL") {
            YamlEntry::Value(Value::Empty)
        } else {
            YamlEntry::Value(Value::parse(value, false))
        };
        out.push((full, entry));
    }
    flush(&mut list, &mut out);
    Ok(out)
}

/// The csv's header line and its index (see [`Table::parse_csv`]).
pub fn csv_header(text: &str) -> Option<(usize, &str)> {
    let width = |l: &str| {
        let cells = split_csv_line(l);
        cells.len()
            - cells
                .iter()
                .rev()
                .take_while(|c| c.trim().is_empty())
                .count()
    };
    let semi: Vec<(usize, &str, usize)> = text
        .lines()
        .enumerate()
        .filter(|(_, l)| l.contains(';'))
        .map(|(i, l)| (i, l, width(l)))
        .collect();
    let mut counts: HashMap<usize, usize> = HashMap::new();
    for &(_, _, w) in &semi {
        *counts.entry(w).or_default() += 1;
    }
    // The most common width; a tie goes to the wider one.
    let data = counts.into_iter().max_by_key(|&(w, n)| (n, w))?.0;
    semi.iter()
        .find(|&&(_, _, w)| w >= data)
        .or(semi.first())
        .map(|&(i, l, _)| (i, l))
}

/// Text of a DAI file whatever its encoding: UTF-16 (with a byte-order mark, or
/// detected from its NUL bytes — what Windows tools write for "Unicode"), UTF-8
/// (BOM dropped), else Latin-1, the usual 8-bit encoding of older exports —
/// rather than mangling every accented byte.
pub fn decode_text(b: &[u8]) -> String {
    let utf16 = |b: &[u8], le: bool| {
        let units: Vec<u16> = b
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| {
                if le {
                    u16::from_le_bytes(c)
                } else {
                    u16::from_be_bytes(c)
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match b {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, true),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, false),
        [a, 0, c, 0, ..] if *a != 0 && *c != 0 => utf16(b, true),
        [0, a, 0, c, ..] if *a != 0 && *c != 0 => utf16(b, false),
        _ => match std::str::from_utf8(b) {
            Ok(s) => s.strip_prefix('\u{feff}').unwrap_or(s).to_string(),
            Err(_) => b.iter().map(|&c| c as char).collect(),
        },
    }
}

/// Split one `;`-separated line, honouring double quotes (`""` is a literal
/// quote inside a quoted cell).
fn split_csv_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ';' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// A yaml line without its `# comment` (a `#` inside quotes, or not preceded by
/// whitespace, is content).
fn strip_yaml_comment(line: &str) -> &str {
    let (mut single, mut double) = (false, false);
    let mut prev = ' ';
    for (i, c) in line.char_indices() {
        match c {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '#' if !single && !double && prev.is_whitespace() => return &line[..i],
            _ => {}
        }
        prev = c;
    }
    line
}

/// `key: value` → `(key, value)`; the key may be quoted.
fn split_yaml_key(s: &str) -> Option<(String, &str)> {
    let (key, value) = if is_quoted_prefix(s) {
        let q = s.chars().next()?;
        let end = s[1..].find(q)? + 1;
        let rest = s[end + 1..].trim_start().strip_prefix(':')?;
        (s[1..end].to_string(), rest)
    } else {
        let at = s
            .find(": ")
            .or_else(|| s.strip_suffix(':').map(|k| k.len()))?;
        (s[..at].trim().to_string(), &s[at + 1..])
    };
    Some((key, value.trim()))
}

fn is_quoted_prefix(s: &str) -> bool {
    s.starts_with('"') || s.starts_with('\'')
}

fn is_quoted(s: &str) -> bool {
    s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
}

fn unquote(s: &str) -> &str {
    if is_quoted(s) {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// How one of the media's files maps onto a DAI table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Binding {
    /// No DAI for this file.
    None,
    /// A yaml record: describes every page of the file.
    Record(usize),
    /// A csv: one row per frame, counted from page 0 of `first_file` — the first
    /// of the media's files bound to the same table, so one csv beside a
    /// numbered run covers the whole run, and a csv per file covers its own.
    Rows { table: usize, first_file: usize },
}

/// One column of a media's DAI, merged across its tables by name (a numbered
/// run may have one csv per file, or a yaml per image).
#[derive(Clone, Debug)]
pub struct Column {
    pub name: String,
    pub ty: ColType,
    /// The column's index in each table (`None` where that table lacks it).
    in_table: Vec<Option<usize>>,
}

/// A media's DAI: its parsed tables and how each of its files maps onto them.
#[derive(Clone, Debug)]
pub struct Dai {
    /// The media's files, in its own order (`paths[k]` ↔ `files[k]`).
    pub paths: Vec<PathBuf>,
    pub files: Vec<Binding>,
    pub tables: Vec<Table>,
    /// Every column of every table, in first-seen order.
    pub columns: Vec<Column>,
}

impl Dai {
    fn new(paths: Vec<PathBuf>, files: Vec<Binding>, tables: Vec<Table>) -> Dai {
        let mut columns: Vec<Column> = Vec::new();
        let mut by_name: HashMap<String, usize> = HashMap::new();
        for (t, table) in tables.iter().enumerate() {
            for (c, name) in table.columns.iter().enumerate() {
                if name.is_empty() {
                    continue; // a trailing `;` makes an unnamed, empty column
                }
                let k = *by_name.entry(name.clone()).or_insert_with(|| {
                    columns.push(Column {
                        name: name.clone(),
                        ty: ColType::Empty,
                        in_table: vec![None; tables.len()],
                    });
                    columns.len() - 1
                });
                let col = &mut columns[k];
                col.in_table[t] = Some(c);
                col.ty = table
                    .rows
                    .iter()
                    .filter_map(|r| r.get(c))
                    .fold(col.ty, |ty, v| ty.widen(v.col_type()));
            }
        }
        Dai {
            paths,
            files,
            tables,
            columns,
        }
    }

    /// Cell of merged column `col` for global frame `frame` of `media`.
    pub fn value(&self, media: &Media, frame: usize, col: usize) -> Option<&Value> {
        let (table, row) = self.row_of(media, frame)?;
        let t = self.tables.iter().position(|t| std::ptr::eq(t, table))?;
        table.get(row, self.columns.get(col)?.in_table[t]?)
    }

    /// How many frames the DAI panel spans: the media's known length — or, for a
    /// single file with a csv, the csv's row count when that is longer, since a
    /// lazily discovered sequence knows its first pages only (each row is still
    /// its page, so a row past the frontier is a frame to seek to).
    pub fn span(&self, media: &Media) -> usize {
        let known = media.frame_count();
        match (self.paths.len(), self.files.first()) {
            (1, Some(&Binding::Rows { table, .. })) if media.local_file(0).is_none() => {
                known.max(self.tables[table].rows.len())
            }
            _ => known,
        }
    }

    /// The table and row describing global frame `frame` of `media` (the media
    /// these DAI were loaded for), or `None` when that frame has none.
    pub fn row_of(&self, media: &Media, frame: usize) -> Option<(&Table, usize)> {
        let (file, page) = match media.local_file(frame) {
            Some((path, page)) => (self.paths.iter().position(|p| p == path)?, page),
            None => (0, frame), // a single file: the frame is its page
        };
        match *self.files.get(file)? {
            Binding::None => None,
            Binding::Record(t) => Some((&self.tables[t], 0)),
            Binding::Rows { table, first_file } => {
                let row = if first_file == file {
                    page
                } else {
                    frame.checked_sub(media.locate_file(&self.paths[first_file], 0)?)?
                };
                Some((&self.tables[table], row))
            }
        }
    }

    /// Whether any of the tables carries a rotation column.
    pub fn has_rotation(&self) -> bool {
        self.tables.iter().any(|t| t.rotation_col.is_some())
    }

    /// The camera rotation recorded for `frame`, in **degrees** (the file stores
    /// radians). `None` when the frame has no DAI row, or the cell is empty, text
    /// or non-finite.
    pub fn rotation_deg(&self, media: &Media, frame: usize) -> Option<f32> {
        let (t, row) = self.row_of(media, frame)?;
        let v = t.get(row, t.rotation_col?)?.as_f64()?;
        v.is_finite().then(|| v.to_degrees() as f32)
    }
}

/// The result of looking for a media's DAI: `dai` is `None` when nothing usable
/// was found, and `errors` names each matched file that failed to read.
pub struct Loaded {
    pub dai: Option<Dai>,
    pub errors: Vec<String>,
}

/// Find, read and bind the DAI for a media made of `paths`. `sequence` says the
/// media is multi-frame, which makes a csv the preferred match over a yaml of
/// equal name affinity (and the reverse for a still).
pub fn load(paths: &[PathBuf], sequence: bool) -> Loaded {
    let mut listings: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    let mut tables: Vec<Table> = Vec::new();
    let mut by_path: HashMap<PathBuf, Option<usize>> = HashMap::new();
    let mut errors = Vec::new();
    let mut files = Vec::with_capacity(paths.len());
    let mut first_of: HashMap<usize, usize> = HashMap::new();
    for (k, path) in paths.iter().enumerate() {
        let dir = dai_dir(path);
        let candidates = listings
            .entry(dir.clone())
            .or_insert_with(|| list_dai(&dir));
        let Some(found) = best_match(path, candidates, sequence) else {
            files.push(Binding::None);
            continue;
        };
        let slot = *by_path
            .entry(found.clone())
            .or_insert_with(|| match read_table(&found) {
                Ok(t) => {
                    tables.push(t);
                    Some(tables.len() - 1)
                }
                Err(e) => {
                    errors.push(format!("{}: {e}", found.display()));
                    None
                }
            });
        files.push(match slot {
            None => Binding::None,
            Some(t) if tables[t].kind == Kind::Yaml => Binding::Record(t),
            // A csv's rows start at the first of the media's files its name fits
            // — not the first one it won, since a file before it may have been
            // claimed by its own yaml and still be a row of the csv.
            Some(t) => Binding::Rows {
                table: t,
                first_file: *first_of.entry(t).or_insert_with(|| {
                    let dai = found.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                    paths[..k]
                        .iter()
                        .position(|p| {
                            p.file_stem()
                                .and_then(|s| s.to_str())
                                .is_some_and(|s| affinity(s, dai).is_some())
                        })
                        .unwrap_or(k)
                }),
            },
        });
    }
    let dai = (!tables.is_empty()).then(|| Dai::new(paths.to_vec(), files, tables));
    Loaded { dai, errors }
}

/// The directory a media file's DAI is looked for in: its own. A bare file
/// name (`cim seq.tif`, run from the file's folder) has the **empty** path as
/// parent, which can't be listed — that is the working directory.
fn dai_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// The DAI files (csv / yaml) in `dir`; an unreadable directory has none.
fn list_dai(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| Kind::of(p).is_some() && p.is_file())
        .collect();
    out.sort(); // deterministic tie-breaks
    out
}

fn read_table(path: &Path) -> Result<Table, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let text = decode_text(&bytes);
    match Kind::of(path) {
        Some(Kind::Csv) => Table::parse_csv(path.to_path_buf(), &text),
        Some(Kind::Yaml) => Table::parse_yaml(path.to_path_buf(), &text),
        None => Err("not a DAI file".into()),
    }
}

/// The name components of a file stem, lower-cased: split on `_ - . space` and
/// at letter→digit / digit→letter changes (`seqA00012` → `seqa`, `00012`).
fn components(stem: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in stem.chars() {
        if matches!(c, '_' | '-' | '.' | ' ') {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if let Some(prev) = cur.chars().last() {
            if prev.is_ascii_digit() != c.is_ascii_digit() {
                out.push(std::mem::take(&mut cur));
            }
        }
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// How well a DAI file named `dai` fits a media file named `media` (both stems),
/// higher is better; `None` = unrelated. Same name beats a DAI name that is a
/// component prefix of the media's (longer prefix better), which beats a media
/// name that is a prefix of the DAI's (shorter DAI name better). Anything else
/// — `seqA_1` against `seqA_2` — doesn't match.
///
/// A DAI name may carry a `_dai` tag (`seqA.tif` → `seqA_dai.csv`), which is
/// set aside before comparing: such a file counts as the same name, ranked just
/// under an untagged one. Below those ranks a processed version of an
/// acquisition reading the DAI of its **raw** one — `seq.versionA.tif` →
/// `seq.raw.csv` (or `seq.raw_dai.csv`), the last dotted part of the media's
/// name replaced by `raw`.
fn affinity(media: &str, dai: &str) -> Option<(u8, isize)> {
    let (core, tagged) = match dai
        .len()
        .checked_sub(4)
        .filter(|&at| dai.is_char_boundary(at))
    {
        Some(at) if at > 0 && dai[at..].eq_ignore_ascii_case("_dai") => (&dai[..at], true),
        _ => (dai, false),
    };
    let (a, b) = (components(media), components(core));
    if a.is_empty() || b.is_empty() {
        return None;
    }
    if a == b {
        return Some((3, if tagged { -1 } else { 0 }));
    }
    if let (Some((base, _)), Some((dai_base, tag))) =
        (media.rsplit_once('.'), core.rsplit_once('.'))
    {
        if tag.eq_ignore_ascii_case("raw")
            && !base.is_empty()
            && base.eq_ignore_ascii_case(dai_base)
        {
            return Some((3, -2));
        }
    }
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    if common == b.len() {
        Some((2, common as isize))
    } else if common == a.len() {
        Some((1, -(b.len() as isize)))
    } else {
        None
    }
}

/// The best DAI file for the media file `path` among `candidates`.
fn best_match(path: &Path, candidates: &[PathBuf], sequence: bool) -> Option<PathBuf> {
    let stem = path.file_stem()?.to_str()?;
    let preferred = if sequence { Kind::Csv } else { Kind::Yaml };
    candidates
        .iter()
        .filter(|c| c.as_path() != path)
        .filter_map(|c| {
            let score = affinity(stem, c.file_stem()?.to_str()?)?;
            Some(((score, Kind::of(c)? == preferred), c))
        })
        // `max_by_key` keeps the last of equals; candidates are sorted, so
        // reverse to keep the first.
        .rev()
        .max_by_key(|(key, _)| *key)
        .map(|(_, c)| c.clone())
}

/// Fuzzy match of a typed `query` against a column `name`, case-insensitive:
/// every query character must appear in order. Higher is better — consecutive
/// runs, word starts (after `_ . - space` or a lower→upper change) and an early
/// first hit score up, gaps score down (the best alignment counts, not the
/// leftmost). `None` = no match; an empty query
/// matches everything equally.
pub fn fuzzy_score(query: &str, name: &str) -> Option<i32> {
    let q: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    if q.is_empty() {
        return Some(0);
    }
    let n: Vec<char> = name.chars().collect();
    let lower: Vec<char> = n
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    let bonus = |i: usize| {
        let word_start = i == 0
            || matches!(n[i - 1], '_' | '.' | '-' | ' ')
            || (n[i - 1].is_lowercase() && n[i].is_uppercase());
        1 + if word_start { 8 } else { 0 }
    };
    // best[i]: the best score of the query so far with its last character
    // matched at name position `i` — the best *alignment*, not the leftmost,
    // so `roll` finds the trailing `_roll` rather than scattered letters.
    const NO: i32 = i32::MIN / 2;
    let mut best: Vec<i32> = (0..n.len())
        .map(|i| {
            if lower[i] == q[0] {
                bonus(i) - i.min(3) as i32
            } else {
                NO
            }
        })
        .collect();
    for &qc in &q[1..] {
        let mut next = vec![NO; n.len()];
        for i in 1..n.len() {
            if lower[i] != qc {
                continue;
            }
            // Only from a position the query so far really matched at: `NO`
            // plus a bonus is still no match.
            let run = (best[i - 1] > NO).then(|| best[i - 1] + 8);
            let gap = (0..i - 1)
                .filter(|&j| best[j] > NO)
                .map(|j| best[j] - (i - j - 1).min(5) as i32)
                .max();
            if let Some(prev) = run.max(gap) {
                next[i] = prev + bonus(i);
            }
        }
        best = next;
    }
    best.into_iter().max().filter(|&b| b > NO)
}

/// A human-readable account of how `path`'s DAI is (or isn't) found and read —
/// every step [`load`] takes, with what it saw — for `cim --dai-check FILE`.
/// DAI is silent by design, so this is the way to find out why a file isn't
/// picked up: the rotation names in effect, each candidate file and its
/// affinity, the encoding and header line chosen, the columns, and whether a
/// rotation column was found (with the nearest names when not).
pub fn diagnose(path: &Path) -> String {
    let mut report = diagnose_steps(path);
    // Last, the lookup exactly as a pane runs it (`DaiLoader` → `load`), so the
    // report's verdict is the app's, not a re-derivation of it.
    let media = crate::media::load(path).ok();
    let sequence = media.as_ref().is_none_or(|m| m.is_sequence());
    let loaded = load(&[path.to_path_buf()], sequence);
    let verdict = match (&loaded.dai, &media) {
        (None, _) => t!("daicheck.app_none").into_owned(),
        (Some(d), Some(m)) if d.has_rotation() => {
            let r = d
                .rotation_deg(m, 0)
                .map_or_else(|| "—".into(), |v| format!("{v}°"));
            t!("daicheck.app_rotation", cols = d.columns.len(), deg = r).into_owned()
        }
        (Some(d), _) => t!("daicheck.app_no_rotation", cols = d.columns.len()).into_owned(),
    };
    report.push_str(&verdict);
    report.push('\n');
    for e in loaded.errors {
        report.push_str(&format!("  {e}\n"));
    }
    report
}

fn diagnose_steps(path: &Path) -> String {
    let mut out = Vec::new();
    let mut say = |s: String| out.push(s);
    let names = crate::proprietary::get();
    say(t!("daicheck.media", path = path.display()).into_owned());
    match crate::proprietary::source() {
        Some(src) => say(t!("daicheck.names_from", path = src).into_owned()),
        None => say(t!("daicheck.names_none", file = crate::proprietary::FILE).into_owned()),
    }
    if let Some(e) = crate::proprietary::load_error() {
        say(t!("daicheck.names_error", err = e).into_owned());
    }
    say(format!("  csv:  {:?}", names.csv_rotation));
    say(format!("  yaml: {:?}", names.yaml_rotation));
    let sequence = crate::media::load(path)
        .map(|m| m.is_sequence())
        .unwrap_or(true);
    say(t!(
        "daicheck.kind",
        kind = if sequence { "csv" } else { "yaml" }
    )
    .into_owned());
    // The very function `load` uses: this report once diverged from it here,
    // hiding the bug it is meant to find.
    let dir = dai_dir(path);
    let candidates = list_dai(&dir);
    say(t!(
        "daicheck.candidates",
        n = candidates.len(),
        dir = dir.display()
    )
    .into_owned());
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    for c in &candidates {
        let score = c
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|d| affinity(stem, d));
        let name = c
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match score {
            Some(sc) => say(format!("  + {name}  {sc:?}")),
            None => say(format!("  - {name}  {}", t!("daicheck.no_match"))),
        }
    }
    let Some(found) = best_match(path, &candidates, sequence) else {
        say(t!("daicheck.none_chosen").into_owned());
        return out.join("\n") + "\n";
    };
    say(t!("daicheck.chosen", path = found.display()).into_owned());
    let bytes = match std::fs::read(&found) {
        Ok(b) => b,
        Err(e) => {
            say(t!("daicheck.read_error", err = e).into_owned());
            return out.join("\n") + "\n";
        }
    };
    let encoding = match bytes.as_slice() {
        [0xFF, 0xFE, ..] | [_, 0, _, 0, ..] => "UTF-16 LE",
        [0xFE, 0xFF, ..] | [0, _, 0, _, ..] => "UTF-16 BE",
        b if std::str::from_utf8(b).is_ok() => "UTF-8",
        _ => "Latin-1",
    };
    say(t!("daicheck.encoding", bytes = bytes.len(), enc = encoding).into_owned());
    let text = decode_text(&bytes);
    if Kind::of(&found) == Some(Kind::Csv) {
        match csv_header(&text) {
            Some((at, line)) => say(t!(
                "daicheck.header",
                line = at + 1,
                text = ellipsize(line, 160)
            )
            .into_owned()),
            None => say(t!("daicheck.no_header").into_owned()),
        }
    }
    let table = match Kind::of(&found) {
        Some(Kind::Csv) => Table::parse_csv(found.clone(), &text),
        _ => Table::parse_yaml(found.clone(), &text),
    };
    let table = match table {
        Ok(t) => t,
        Err(e) => {
            say(t!("daicheck.parse_error", err = e).into_owned());
            return out.join("\n") + "\n";
        }
    };
    say(t!(
        "daicheck.table",
        cols = table.columns.len(),
        rows = table.rows.len()
    )
    .into_owned());
    say(format!("  {}", ellipsize(&table.columns.join(" | "), 400)));
    match table.rotation_col {
        Some(c) => {
            let first: Vec<String> = table
                .rows
                .iter()
                .take(5)
                .map(|r| r.get(c).map_or_else(|| "—".into(), Value::display))
                .collect();
            say(t!(
                "daicheck.rotation",
                name = table.columns[c],
                values = first.join(", ")
            )
            .into_owned());
        }
        None => {
            say(t!("daicheck.no_rotation").into_owned());
            // The nearest column names, so a renamed or differently cased
            // column stands out.
            let wanted = match table.kind {
                Kind::Csv => &names.csv_rotation,
                Kind::Yaml => &names.yaml_rotation,
            };
            for w in wanted {
                let mut near: Vec<(i32, &String)> = table
                    .columns
                    .iter()
                    .filter_map(|c| Some((fuzzy_score(w, c)?, c)))
                    .collect();
                near.sort_by_key(|&(sc, _)| std::cmp::Reverse(sc));
                let near: Vec<&str> = near.iter().take(3).map(|(_, c)| c.as_str()).collect();
                say(format!("  {w}: {}", near.join(", ")));
            }
        }
    }
    out.join("\n") + "\n"
}

fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

struct LoadJob {
    id: u64,
    gen: u64,
    paths: Vec<PathBuf>,
    sequence: bool,
}

/// A finished DAI lookup for pane `id`, generation `gen`.
pub struct DaiDone {
    pub id: u64,
    pub gen: u64,
    pub dai: Option<Arc<Dai>>,
    pub errors: Vec<String>,
}

/// The single worker thread that finds and parses DAI files.
pub struct DaiLoader {
    job_tx: Sender<LoadJob>,
    done_rx: Receiver<DaiDone>,
}

impl DaiLoader {
    /// `ctx` is woken when a lookup finishes, so the UI applies it next update.
    pub fn new(ctx: eframe::egui::Context) -> Self {
        let (job_tx, job_rx) = channel::<LoadJob>();
        let (done_tx, done_rx) = channel::<DaiDone>();
        thread::spawn(move || {
            while let Ok(job) = job_rx.recv() {
                let Loaded { dai, errors } = load(&job.paths, job.sequence);
                let done = DaiDone {
                    id: job.id,
                    gen: job.gen,
                    dai: dai.map(Arc::new),
                    errors,
                };
                if done_tx.send(done).is_err() {
                    break; // app shutting down
                }
                ctx.request_repaint();
            }
        });
        Self { job_tx, done_rx }
    }

    pub fn request(&self, id: u64, gen: u64, paths: Vec<PathBuf>, sequence: bool) {
        let _ = self.job_tx.send(LoadJob {
            id,
            gen,
            paths,
            sequence,
        });
    }

    /// Every finished lookup available right now (non-blocking).
    pub fn drain(&self) -> Vec<DaiDone> {
        self.done_rx.try_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{fixture_dir, write_multipage_tiff_u16, write_png_run};

    #[test]
    fn csv_skips_the_free_form_header() {
        let text = "Acquisition report\nOperator: someone\n\n\
                    frame;camRotation;label;ok\n\
                    0;0,5;\"a;b\";true\n\
                    1;NaN;;false\n\
                    2;1.25\n";
        let t = Table::parse_csv("x.csv".into(), text).unwrap();
        assert_eq!(t.columns, ["frame", "camRotation", "label", "ok"]);
        assert_eq!(t.rows.len(), 3);
        assert_eq!(t.rows[0][1], Value::Float(0.5)); // decimal comma
        assert_eq!(t.rows[0][2], Value::Text("a;b".into())); // quoted separator
        assert_eq!(t.rows[0][3], Value::Text("true".into())); // no booleans
        assert!(t.rows[1][1].as_f64().unwrap().is_nan());
        assert_eq!(t.rows[1][2], Value::Empty);
        assert_eq!(t.rows[2][1], Value::Float(1.25));
        assert_eq!(t.rows[2][3], Value::Empty); // short row padded
        assert_eq!(t.rotation_col, Some(1));
        assert!(Table::parse_csv("x.csv".into(), "no separators\nhere").is_err());
        // The names come from `proprietary.yaml` (test stand-ins here), tried
        // in order, case-insensitively.
        let old = Table::parse_csv("x.csv".into(), "a;camRotationOld\n1;2\n").unwrap();
        assert_eq!(old.rotation_col, Some(1));
        let both = Table::parse_csv("x.csv".into(), "camRotationOld;camRotation\n1;2\n").unwrap();
        assert_eq!(both.rotation_col, Some(1));
    }

    /// A preamble line holding a `;` isn't taken for the column names: the
    /// header is the first line as wide as the data.
    #[test]
    fn csv_header_skips_a_preamble_with_semicolons() {
        let text =
            "Date;2024-05-01\nOperator;someone;x\na;camRotation;c;d\n1;0,5;3;9\n2;0,25;;\n3;1;4;9\n";
        let t = Table::parse_csv("x.csv".into(), text).unwrap();
        assert_eq!(t.columns, ["a", "camRotation", "c", "d"]);
        assert_eq!(t.rows.len(), 3);
        assert_eq!(t.rotation_col, Some(1));
        let case = Table::parse_csv("x.csv".into(), "x; CAMROTATION \n1;2\n").unwrap();
        assert_eq!(case.rotation_col, Some(1));
    }

    #[test]
    fn text_decodes_whatever_the_encoding() {
        let s = "a;é\n";
        let utf16le: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain(s.encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(decode_text(&utf16le), s);
        let no_bom: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_text(&no_bom), s);
        let be: Vec<u8> = [0xFE, 0xFF]
            .into_iter()
            .chain(s.encode_utf16().flat_map(u16::to_be_bytes))
            .collect();
        assert_eq!(decode_text(&be), s);
        assert_eq!(decode_text(b"\xEF\xBB\xBFa;b"), "a;b");
        assert_eq!(decode_text(b"a;\xE9"), "a;é"); // Latin-1
    }

    #[test]
    fn yaml_flattens_a_nested_record() {
        let text = "# per-image record\n---\n\
                    cam:\n  roll: -0.25  # radians\n\
                    name: \"img #1\"\n\
                    camera:\n  model: X1\n  gain: 3\n\
                    tags:\n  - a\n  - b\n\
                    missing: ~\n";
        let t = Table::parse_yaml("x.yaml".into(), text).unwrap();
        let get = |k: &str| &t.rows[0][t.columns.iter().position(|c| c == k).unwrap()];
        assert_eq!(get("cam.roll"), &Value::Float(-0.25));
        assert_eq!(
            t.rotation_col,
            t.columns.iter().position(|c| c == "cam.roll")
        );
        assert_eq!(get("name"), &Value::Text("img #1".into()));
        assert_eq!(get("camera.model"), &Value::Text("X1".into()));
        assert_eq!(get("camera.gain"), &Value::Int(3));
        assert_eq!(get("tags"), &Value::Text("[a, b]".into()));
        assert_eq!(get("missing"), &Value::Empty);
        assert!(Table::parse_yaml("x.yaml".into(), "just text").is_err());
    }

    #[test]
    fn columns_are_typed_and_merged_across_files() {
        let t = Table::parse_csv(
            "a.csv".into(),
            "i;f;s;flag;none\n1;1;x;0;\n2;2,5;3;1;\n;NaN;;1;\n",
        )
        .unwrap();
        let u = Table::parse_csv("b.csv".into(), "i;extra\n3;7\n").unwrap();
        let dai = Dai::new(vec![], vec![], vec![t, u]);
        let ty: Vec<(&str, ColType)> = dai
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.ty))
            .collect();
        assert_eq!(
            ty,
            [
                ("i", ColType::Int),
                ("f", ColType::Float), // an int and a float (and NaN) → float
                ("s", ColType::Text),
                ("flag", ColType::Int), // 0/1 stays an int, never a boolean
                ("none", ColType::Empty),
                ("extra", ColType::Int),
            ]
        );
        assert_eq!(dai.columns[0].in_table, [Some(0), Some(0)]);
        assert_eq!(dai.columns[5].in_table, [None, Some(1)]);
    }

    #[test]
    fn fuzzy_search_ranks_word_starts_and_runs() {
        assert!(fuzzy_score("crt", "camRotation").is_some());
        assert!(fuzzy_score("xyz", "camRotation").is_none());
        assert!(fuzzy_score("rq", "camRotation").is_none()); // `r` matches, `q` never does
        assert!(fuzzy_score("ab", "ba").is_none()); // order matters
        assert_eq!(fuzzy_score("", "anything"), Some(0));
        // Contiguous / word-start hits beat scattered ones.
        let a = fuzzy_score("roll", "cam_absolute_orient_roll").unwrap();
        let b = fuzzy_score("roll", "rate_of_lateral_lift").unwrap();
        assert!(a > b, "{a} vs {b}");
        let c = fuzzy_score("rot", "camRotation").unwrap();
        let d = fuzzy_score("rot", "dummy_error_offset").unwrap();
        assert!(c > d, "{c} vs {d}");
        assert!(fuzzy_score("ROLL", "cam_absolute_orient_roll").is_some());
    }

    #[test]
    fn floats_display_compactly() {
        assert_eq!(fmt_float(0.5), "0.5");
        assert_eq!(fmt_float(2.0), "2");
        assert_eq!(fmt_float(1.0 / 3.0), "0.333333");
        assert_eq!(fmt_float(1.5e-7), "1.5000e-7");
        assert_eq!(fmt_float(f64::NAN), "NaN");
    }

    /// A file opened by its bare name looks in the working directory: its
    /// parent is the empty path, which `read_dir` refuses — so DAI was silently
    /// never found for `cim seq.tif` run from the file's own folder.
    #[test]
    fn a_bare_file_name_looks_in_the_working_directory() {
        assert_eq!(dai_dir(Path::new("seq.tif")), Path::new("."));
        assert_eq!(dai_dir(Path::new("d/seq.tif")), Path::new("d"));
        assert_eq!(dai_dir(Path::new("/seq.tif")), Path::new("/"));
        assert!(std::fs::read_dir(dai_dir(Path::new("seq.tif"))).is_ok());
    }

    #[test]
    fn names_match_by_common_components() {
        assert_eq!(affinity("seqA", "seqA"), Some((3, 0)));
        assert!(affinity("seqA_variation", "seqA").is_some());
        assert!(affinity("seqA", "seqA_dai").is_some());
        assert!(affinity("seqA00012", "seqA").is_some());
        assert_eq!(affinity("seqA_1", "seqA_2"), None);
        assert_eq!(affinity("seqA", "seqB"), None);
        // A processed version reads its raw acquisition's DAI.
        assert_eq!(affinity("seq.versionA", "seq.raw"), Some((3, -2)));
        assert_eq!(affinity("seq_1.VersionB", "seq_1.RAW"), Some((3, -2)));
        assert_eq!(affinity("seq.versionA", "seq.raw_dai"), Some((3, -2)));
        // A `_dai` tag counts as the same name, just under an untagged one.
        assert_eq!(affinity("seqA", "seqA_dai"), Some((3, -1)));
        assert_eq!(affinity("seqA", "seqA_DAI"), Some((3, -1)));
        assert!(affinity("seqA", "seqA_dai") < affinity("seqA", "seqA"));
        assert!(affinity("seqA", "seqA_dai") > affinity("seqA", "seqA_other"));
        assert!(affinity("seqA_variation", "seqA_dai").is_some());
        assert_eq!(affinity("seqB", "seqA_dai"), None);
        assert_eq!(affinity("x", "_dai"), None);
        let tagged: Vec<PathBuf> = ["d/seqA_dai.csv", "d/seqA_x.csv", "d/img.dai", "d/pic.dai2"]
            .iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(
            best_match(Path::new("d/seqA.tif"), &tagged, true).unwrap(),
            Path::new("d/seqA_dai.csv")
        );
        assert_eq!(
            best_match(Path::new("d/img.png"), &tagged, false).unwrap(),
            Path::new("d/img.dai")
        );
        assert_eq!(
            best_match(Path::new("d/pic.png"), &tagged, false).unwrap(),
            Path::new("d/pic.dai2")
        );
        assert_eq!(affinity("seq.versionA", "other.raw"), None);
        assert!(affinity("seq.versionA", "seq.raw") < affinity("seq.versionA", "seq.versionA"));
        assert!(affinity("seq.versionA", "seq.raw") > affinity("seq.versionA", "seq"));
        let raw: Vec<PathBuf> = ["d/seq.csv", "d/seq.raw.csv", "d/seq.versionB.csv"]
            .iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(
            best_match(Path::new("d/seq.versionA.tif"), &raw, true).unwrap(),
            Path::new("d/seq.raw.csv")
        );
        let c: Vec<PathBuf> = [
            "d/seqA.csv",
            "d/seqA.yaml",
            "d/seqA_variation.csv",
            "d/seqB.csv",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let pick = |p: &str, seq| best_match(Path::new(p), &c, seq).unwrap();
        assert_eq!(
            pick("d/seqA_variation.tif", true),
            Path::new("d/seqA_variation.csv")
        );
        assert_eq!(pick("d/seqA.tif", true), Path::new("d/seqA.csv"));
        assert_eq!(pick("d/seqA.tif", false), Path::new("d/seqA.yaml"));
        assert_eq!(pick("d/seqA_other.tif", true), Path::new("d/seqA.csv"));
        assert!(best_match(Path::new("d/seqC.tif"), &c, true).is_none());
    }

    /// One csv beside a multi-page TIFF gives each page its own row.
    #[test]
    fn a_tiff_sequence_reads_its_csv_rows() {
        let dir = fixture_dir("dai_tiff");
        let tif = dir.join("seqA_variation.tif");
        write_multipage_tiff_u16(&tif, &[[4, 4]; 3]);
        std::fs::write(
            dir.join("seqA.csv"),
            "header\ncamRotation;x\n0;a\n1,5;b\n;c\n",
        )
        .unwrap();
        let media = crate::media::load(&tif).unwrap();
        let dai = load(std::slice::from_ref(&tif), true).dai.unwrap();
        assert!(dai.has_rotation());
        assert_eq!(dai.rotation_deg(&media, 0), Some(0.0));
        assert_eq!(
            dai.rotation_deg(&media, 1),
            Some(1.5f64.to_degrees() as f32)
        );
        assert_eq!(dai.rotation_deg(&media, 2), None); // empty cell
        assert_eq!(dai.rotation_deg(&media, 9), None); // no such row
    }

    /// A numbered still run: per-image yamls win over a run-wide csv, and with
    /// only the csv its rows follow the run's frames.
    #[test]
    fn a_still_run_binds_per_file_yaml_or_one_csv() {
        let dir = fixture_dir("dai_run");
        let files = write_png_run(&dir, 3, 4, 4);
        let stem = files[0].file_stem().unwrap().to_str().unwrap().to_string();
        let prefix = components(&stem)[0].clone();
        std::fs::write(
            dir.join(format!("{prefix}.csv")),
            "camRotation\n0\n0.1\n0.2\n".replace('\n', ";\n"),
        )
        .unwrap();
        let media = crate::media::load_sequence(&files, "run".into()).unwrap();
        let dai = load(&files, true).dai.unwrap();
        for f in 0..3 {
            let want = (f as f64 * 0.1).to_degrees() as f32;
            assert_eq!(dai.rotation_deg(&media, f), Some(want), "frame {f}");
        }
        std::fs::write(dir.join(format!("{stem}.yaml")), "cam:\n  roll: 1\n").unwrap();
        let dai = load(&files, true).dai.unwrap();
        assert_eq!(dai.rotation_deg(&media, 0), Some(1f64.to_degrees() as f32));
        assert_eq!(
            dai.rotation_deg(&media, 1),
            Some(0.1f64.to_degrees() as f32)
        );
    }

    #[test]
    fn an_unreadable_match_is_reported_not_fatal() {
        let dir = fixture_dir("dai_bad");
        let tif = dir.join("seqA.tif");
        write_multipage_tiff_u16(&tif, &[[4, 4]]);
        std::fs::write(dir.join("seqA.csv"), "no header here\n").unwrap();
        let loaded = load(std::slice::from_ref(&tif), true);
        assert!(loaded.dai.is_none());
        assert_eq!(loaded.errors.len(), 1);
        // A `.dai2` file is yaml inside.
        let still = dir.join("pic.tif");
        write_multipage_tiff_u16(&still, &[[4, 4]]);
        std::fs::write(dir.join("pic.dai2"), "cam:\n  roll: 0.5\n").unwrap();
        let media = crate::media::load(&still).unwrap();
        let dai = load(std::slice::from_ref(&still), false).dai.unwrap();
        assert_eq!(
            dai.rotation_deg(&media, 0),
            Some(0.5f64.to_degrees() as f32)
        );
        // No DAI file at all: nothing, and nothing to report.
        let other = dir.join("lonely.tif");
        write_multipage_tiff_u16(&other, &[[4, 4]]);
        let loaded = load(std::slice::from_ref(&other), true);
        assert!(loaded.dai.is_none() && loaded.errors.is_empty());
    }
}
