//! Tunable construction parameters of the proprietary operators (the optional
//! `cim_<op>_param_*` / `_preset_*` / `_create_with` ABI, `INTEGRATION_CPP.md`).
//!
//! An operator library that exports them describes its parameters as a
//! [`Schema`] — each a fixed-length list of float or int values — plus its named
//! presets. A pane picks values through its tone ([`OpParamSel`]), the render
//! workers hand them to `create_with`, and the export does the same, so the view
//! and the output agree.
//!
//! **Values are interned** ([`intern`] / [`get`]): the tone options, the render
//! job and the export plan carry only a [`ParamsRef`] — a hash of the values —
//! so they stay `Copy` (as [`crate::curve::Curve`] does with its fixed capacity),
//! `tone_sig` has a ready identity to fold in, and a worker thread resolves the
//! values itself. The table only grows by committed edits (one per released
//! slider), so it is never pruned.
//!
//! **User presets** are kept by parameter *key* rather than by position
//! ([`UserPresets`], `op_presets.json` beside the config), so a library that
//! reorders or adds parameters still reads them: a missing key takes the
//! library default's value and an unknown one is dropped ([`from_keys`]).

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

/// One parameter: `len` values (a scalar when 1), float or int, bounded by
/// `min`/`max` (`NaN` = unbounded), at `offset` in the flat value array.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamInfo {
    pub key: String,
    pub int: bool,
    pub len: usize,
    pub min: f64,
    pub max: f64,
    pub offset: usize,
}

impl ParamInfo {
    /// `v` clamped to this parameter's bounds, and rounded when it is an int.
    pub fn clamp(&self, v: f64) -> f64 {
        let mut v = if self.int { v.round() } else { v };
        if self.min.is_finite() {
            v = v.max(self.min);
        }
        if self.max.is_finite() {
            v = v.min(self.max);
        }
        v
    }
}

/// A preset the library ships: its name and its flat values.
#[derive(Clone, Debug, PartialEq)]
pub struct Preset {
    pub name: String,
    pub values: Vec<f64>,
}

/// An operator library's parameters and presets, copied out of the library once
/// at load (no string crosses the boundary later).
#[derive(Clone, Debug, PartialEq)]
pub struct Schema {
    pub params: Vec<ParamInfo>,
    pub presets: Vec<Preset>,
    /// The preset plain `create` uses — what a pane with no selection shows.
    pub default: usize,
    /// The flat value count, `Σ len`.
    pub n: usize,
}

impl Schema {
    /// Build from the parameter list `(key, int, len, min, max)` and presets,
    /// computing offsets. `None` when it isn't usable: no parameter, no preset,
    /// a zero-length parameter, a default out of range, or a preset whose value
    /// count disagrees with the parameters.
    pub fn new(
        params: Vec<(String, bool, usize, f64, f64)>,
        presets: Vec<Preset>,
        default: usize,
    ) -> Option<Self> {
        let mut offset = 0;
        let mut out = Vec::with_capacity(params.len());
        for (key, int, len, min, max) in params {
            if len == 0 {
                return None;
            }
            out.push(ParamInfo {
                key,
                int,
                len,
                min,
                max,
                offset,
            });
            offset += len;
        }
        let ok = !out.is_empty()
            && default < presets.len()
            && presets.iter().all(|p| p.values.len() == offset);
        ok.then_some(Schema {
            params: out,
            presets,
            default,
            n: offset,
        })
    }

    /// The default preset's values.
    pub fn default_values(&self) -> &[f64] {
        &self.presets[self.default].values
    }
}

/// Interned parameter values: the hash of the value bits. Equal values intern
/// to the same reference.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ParamsRef(u64);

static TABLE: RwLock<Option<HashMap<u64, Arc<[f64]>>>> = RwLock::new(None);

fn hash_values(values: &[f64]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    values.len().hash(&mut h);
    for v in values {
        v.to_bits().hash(&mut h);
    }
    h.finish()
}

/// Intern `values`, returning their reference.
pub fn intern(values: &[f64]) -> ParamsRef {
    let key = hash_values(values);
    let mut table = TABLE.write().unwrap();
    table
        .get_or_insert_with(HashMap::new)
        .entry(key)
        .or_insert_with(|| values.into());
    ParamsRef(key)
}

/// The values behind a reference.
pub fn get(r: ParamsRef) -> Option<Arc<[f64]>> {
    TABLE.read().unwrap().as_ref()?.get(&r.0).cloned()
}

/// Where a pane's values started from, for the preset dropdown's label: a
/// library preset (by index) or a user preset (by its stable id).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Origin {
    Library(usize),
    User(u64),
}

/// A pane's chosen values for one operator plus where they came from.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Sel {
    pub values: ParamsRef,
    pub origin: Origin,
}

/// A pane's selections, one per tunable operator. `None` = the library's own
/// default (plain `create`, exactly the behaviour without this feature).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct OpParamSel {
    pub lut_alpha: Option<Sel>,
    pub boost: Option<Sel>,
}

impl OpParamSel {
    /// The selection for `op` (`None` for an operator without parameters).
    pub fn get(&self, op: crate::imageproc::Op) -> Option<Sel> {
        match op {
            crate::imageproc::Op::LutAlpha => self.lut_alpha,
            crate::imageproc::Op::Boost => self.boost,
            crate::imageproc::Op::Details => None,
        }
    }

    /// Set the selection for `op` (ignored for an operator without parameters).
    pub fn set(&mut self, op: crate::imageproc::Op, sel: Option<Sel>) {
        match op {
            crate::imageproc::Op::LutAlpha => self.lut_alpha = sel,
            crate::imageproc::Op::Boost => self.boost = sel,
            crate::imageproc::Op::Details => {}
        }
    }
}

/// `values` by parameter key (for user presets and the view command).
pub fn to_keys(schema: &Schema, values: &[f64]) -> BTreeMap<String, Vec<f64>> {
    schema
        .params
        .iter()
        .filter_map(|p| {
            let v = values.get(p.offset..p.offset + p.len)?;
            Some((p.key.clone(), v.to_vec()))
        })
        .collect()
}

/// Flat values from values by key: a missing key (or one of the wrong length)
/// takes the library default's value, an unknown key is dropped, and every
/// value is clamped to its parameter's bounds.
pub fn from_keys(schema: &Schema, keyed: &BTreeMap<String, Vec<f64>>) -> Vec<f64> {
    let mut out = schema.default_values().to_vec();
    for p in &schema.params {
        if let Some(v) = keyed.get(&p.key).filter(|v| v.len() == p.len) {
            for (slot, &x) in out[p.offset..p.offset + p.len].iter_mut().zip(v) {
                *slot = p.clamp(x);
            }
        }
    }
    out
}

/// The view command's text for `values`: the parameters that differ from the
/// library default, `KEY=v/v/v` joined by `,`. Empty when nothing differs.
pub fn to_token(schema: &Schema, values: &[f64]) -> String {
    let def = schema.default_values();
    schema
        .params
        .iter()
        .filter(|p| values[p.offset..p.offset + p.len] != def[p.offset..p.offset + p.len])
        .map(|p| {
            let v: Vec<String> = values[p.offset..p.offset + p.len]
                .iter()
                .map(|x| x.to_string())
                .collect();
            format!("{}={}", p.key, v.join("/"))
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Parse [`to_token`]'s text into values by key (resolved against a schema
/// later, by [`from_keys`], since the CLI parses before any library loads).
pub fn parse_token(s: &str) -> Result<BTreeMap<String, Vec<f64>>, String> {
    let mut out = BTreeMap::new();
    for part in s.split(',').filter(|p| !p.trim().is_empty()) {
        let (k, v) = part
            .split_once('=')
            .ok_or_else(|| format!("expected KEY=VALUE, got {part:?}"))?;
        let vals = v
            .split('/')
            .map(|x| {
                x.trim()
                    .parse::<f64>()
                    .map_err(|_| format!("bad number {x:?}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        out.insert(k.trim().to_string(), vals);
    }
    Ok(out)
}

/// One user preset: a stable id (so a rename keeps a pane's origin), a name and
/// the values by key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserPreset {
    pub id: u64,
    pub name: String,
    pub params: BTreeMap<String, Vec<f64>>,
}

/// The user's presets, per operator id (`lut_alpha`, `boost` — stable ids, never
/// the proprietary names). Saved as `op_presets.json` beside the config.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UserPresets {
    #[serde(flatten)]
    pub ops: BTreeMap<String, Vec<UserPreset>>,
}

impl UserPresets {
    fn path() -> Option<PathBuf> {
        Some(crate::settings::config_dir()?.join("op_presets.json"))
    }

    /// Load the saved presets (none when there is no file or it doesn't parse).
    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Write the presets; `Err` carries the reason for the status line.
    pub fn save(&self) -> Result<(), String> {
        let path = Self::path().ok_or_else(|| "no config directory".to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let s = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, s).map_err(|e| e.to_string())
    }

    /// The presets of operator `op`.
    pub fn of(&self, op: &str) -> &[UserPreset] {
        self.ops.get(op).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The preset of `op` with `id`.
    pub fn find(&self, op: &str, id: u64) -> Option<&UserPreset> {
        self.of(op).iter().find(|p| p.id == id)
    }

    /// Add a preset to `op`, returning its new id (one past the largest in use).
    pub fn add(&mut self, op: &str, name: String, params: BTreeMap<String, Vec<f64>>) -> u64 {
        let id = self
            .ops
            .values()
            .flatten()
            .map(|p| p.id)
            .max()
            .map_or(1, |m| m + 1);
        self.ops
            .entry(op.to_string())
            .or_default()
            .push(UserPreset { id, name, params });
        id
    }

    /// Rename preset `id` of `op`.
    pub fn rename(&mut self, op: &str, id: u64, name: String) {
        if let Some(p) = self
            .ops
            .get_mut(op)
            .and_then(|v| v.iter_mut().find(|p| p.id == id))
        {
            p.name = name;
        }
    }

    /// Delete preset `id` of `op`.
    pub fn delete(&mut self, op: &str, id: u64) {
        if let Some(v) = self.ops.get_mut(op) {
            v.retain(|p| p.id != id);
            if v.is_empty() {
                self.ops.remove(op);
            }
        }
    }
}

#[cfg(test)]
pub fn test_schema() -> Schema {
    Schema::new(
        vec![
            ("gamma".into(), false, 1, 0.1, 10.0),
            ("output".into(), true, 2, 0.0, 65535.0),
        ],
        vec![
            Preset {
                name: "Standard".into(),
                values: vec![1.0, 0.0, 65535.0],
            },
            Preset {
                name: "Bright".into(),
                values: vec![0.6, 0.0, 65535.0],
            },
        ],
        0,
    )
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_values_intern_to_one_reference() {
        let a = intern(&[1.0, 2.0]);
        let b = intern(&[1.0, 2.0]);
        let c = intern(&[1.0, 2.5]);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(&*get(a).unwrap(), &[1.0, 2.0]);
        assert_eq!(&*get(c).unwrap(), &[1.0, 2.5]);
    }

    #[test]
    fn schema_computes_offsets_and_refuses_inconsistent_presets() {
        let s = test_schema();
        assert_eq!(s.n, 3);
        assert_eq!(s.params[1].offset, 1);
        let bad = Schema::new(
            vec![("g".into(), false, 2, f64::NAN, f64::NAN)],
            vec![Preset {
                name: "x".into(),
                values: vec![1.0],
            }],
            0,
        );
        assert!(bad.is_none());
        assert!(Schema::new(vec![], vec![], 0).is_none());
    }

    #[test]
    fn values_round_trip_by_key_with_missing_and_unknown_keys() {
        let s = test_schema();
        let keyed = to_keys(&s, &[0.5, 10.0, 200.0]);
        assert_eq!(from_keys(&s, &keyed), vec![0.5, 10.0, 200.0]);
        // A missing key takes the default; an unknown key and a wrong-length
        // list are dropped; values are clamped and ints rounded.
        let mut partial = BTreeMap::new();
        partial.insert("gamma".to_string(), vec![50.0]);
        partial.insert("nope".to_string(), vec![1.0]);
        assert_eq!(from_keys(&s, &partial), vec![10.0, 0.0, 65535.0]);
        partial.insert("output".to_string(), vec![1.4, 2.6]);
        assert_eq!(from_keys(&s, &partial), vec![10.0, 1.0, 3.0]);
        partial.insert("output".to_string(), vec![1.0]);
        assert_eq!(from_keys(&s, &partial)[1..], [0.0, 65535.0]);
    }

    #[test]
    fn token_names_only_what_differs_and_parses_back() {
        let s = test_schema();
        assert_eq!(to_token(&s, s.default_values()), "");
        let v = [0.6, 12.0, 65535.0];
        let tok = to_token(&s, &v);
        assert_eq!(tok, "gamma=0.6,output=12/65535");
        assert_eq!(from_keys(&s, &parse_token(&tok).unwrap()), v);
        assert!(parse_token("gamma").is_err());
        assert!(parse_token("gamma=x").is_err());
    }

    #[test]
    fn user_presets_add_rename_delete_and_serialise() {
        let mut u = UserPresets::default();
        let s = test_schema();
        let a = u.add("lut_alpha", "mine".into(), to_keys(&s, &[2.0, 1.0, 2.0]));
        let b = u.add("lut_alpha", "other".into(), BTreeMap::new());
        assert_ne!(a, b);
        u.rename("lut_alpha", a, "renamed".into());
        assert_eq!(u.find("lut_alpha", a).unwrap().name, "renamed");
        let json = serde_json::to_string(&u).unwrap();
        let back: UserPresets = serde_json::from_str(&json).unwrap();
        assert_eq!(back, u);
        u.delete("lut_alpha", a);
        u.delete("lut_alpha", b);
        assert!(u.of("lut_alpha").is_empty());
        assert!(u.ops.is_empty());
        // A new id never reuses one that's still in use.
        let c = u.add("boost", "x".into(), BTreeMap::new());
        let d = u.add("lut_alpha", "y".into(), BTreeMap::new());
        assert_ne!(c, d);
    }
}
