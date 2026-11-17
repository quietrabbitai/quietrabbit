// src-tauri/src/conductor/focus_dev/preset.rs
//
// Presets (decisions.id=942 Q7, decisions.id=951 ruling 5): shipped, named, parameterised
// fragments of composition rows, held as DATA in block_data/focus_presets.json (the
// document_types.json pattern: include_str!, parsed once, validated by one function), and
// EXPANDED AT SEED TIME into ordinary rows. The runtime never sees a preset.
//
// A preset row is a spec row (spec::SpecRow) written as JSON in which any string may contain
// a slot, %name%, that a declared param fills. Params are typed: slug (a lowercase
// identifier), enum (a closed list of slugs) and int (a range). A param value never contains
// a brace, so a value cannot introduce a {token} (Prompt Authoring Rule).
//
// Collision rule: a use names a prefix (`as`). Every action_id becomes "{as}_{action_id}",
// and every name the preset lists under `locals` becomes "{as}_{name}" everywhere it
// appears: output_var, declared_inputs, a guard's var, a store argument's variable and a
// {name} token in a prompt_template. A name the caller supplies (a slug param) is NOT
// renamed. A param value equal to a local is refused. Two uses with different prefixes
// therefore cannot collide. Expansion is deterministic and the result is parsed back as
// ordinary spec rows, so it passes through the same validators as hand-written rows.
//
// A literal percent sign in a template is fine unless it forms %lowercase_name%; a template
// that needs one writes it some other way.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::Value;

use crate::conductor::focus_dev::spec::{PresetUse, SpecRow};
use crate::conductor::focus_dev::FocusDevError;
use crate::conductor::model_block::config::is_slug;

pub const PRESET_FORMAT_VERSION: u32 = 1;

const SHIPPED: &str = include_str!("../../../block_data/focus_presets.json");

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetFile {
    pub format_version: u32,
    pub presets: Vec<Preset>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub params: Vec<ParamDef>,
    /// Names the preset defines, renamed with the use's prefix.
    #[serde(default)]
    pub locals: Vec<String>,
    /// Template rows: spec rows as JSON, with %name% slots in strings.
    pub rows: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(try_from = "RawParam")]
pub struct ParamDef {
    pub name: String,
    pub kind: ParamKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParamKind {
    Slug,
    Enum { values: Vec<String> },
    Int { min: i64, max: i64 },
}

/// The JSON form of a param: {"name", "type": slug|enum|int, "values", "min", "max"}.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawParam {
    name: String,
    #[serde(rename = "type")]
    ty: String,
    values: Option<Vec<String>>,
    min: Option<i64>,
    max: Option<i64>,
}

impl TryFrom<RawParam> for ParamDef {
    type Error = String;
    fn try_from(r: RawParam) -> Result<Self, String> {
        let kind = match (r.ty.as_str(), r.values, r.min, r.max) {
            ("slug", None, None, None) => ParamKind::Slug,
            ("enum", Some(values), None, None) => ParamKind::Enum { values },
            ("int", None, Some(min), Some(max)) => ParamKind::Int { min, max },
            (t, ..) => {
                return Err(format!(
                    "param '{}': type '{t}' with these keys is not slug, enum (values) or int (min, max)",
                    r.name
                ))
            }
        };
        Ok(ParamDef { name: r.name, kind })
    }
}

/// The shipped presets, parsed and validated once. A malformed file is a build-time bug,
/// caught by the test that calls this.
pub fn shipped() -> Result<&'static PresetFile, FocusDevError> {
    static CELL: OnceLock<Result<PresetFile, FocusDevError>> = OnceLock::new();
    CELL.get_or_init(|| parse(SHIPPED))
        .as_ref()
        .map_err(|e| e.clone())
}

/// Parses and validates a preset file.
pub fn parse(text: &str) -> Result<PresetFile, FocusDevError> {
    let f: PresetFile =
        serde_json::from_str(text).map_err(|e| FocusDevError::Preset(e.to_string()))?;
    validate(&f)?;
    Ok(f)
}

fn bad(id: &str, msg: impl std::fmt::Display) -> FocusDevError {
    FocusDevError::Preset(format!("'{id}': {msg}"))
}

fn brace_free(v: &str) -> bool {
    !v.contains('{') && !v.contains('}')
}

/// Slot names written %name% in `s`, in order.
fn slots_of(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 1 < b.len() && b[i + 1].is_ascii_lowercase() {
            let mut j = i + 1;
            while j < b.len()
                && (b[j].is_ascii_lowercase() || b[j].is_ascii_digit() || b[j] == b'_')
            {
                j += 1;
            }
            if j < b.len() && b[j] == b'%' {
                out.push(s[i + 1..j].to_owned());
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn collect_slots(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.extend(slots_of(s)),
        Value::Array(a) => a.iter().for_each(|x| collect_slots(x, out)),
        Value::Object(m) => m.values().for_each(|x| collect_slots(x, out)),
        _ => {}
    }
}

fn fill(v: &Value, f: &dyn Fn(&str) -> Option<String>) -> Value {
    match v {
        Value::String(s) => {
            let mut out = String::new();
            let b = s.as_bytes();
            let mut i = 0;
            let mut last = 0;
            while i < b.len() {
                if b[i] == b'%' && i + 1 < b.len() && b[i + 1].is_ascii_lowercase() {
                    let mut j = i + 1;
                    while j < b.len()
                        && (b[j].is_ascii_lowercase() || b[j].is_ascii_digit() || b[j] == b'_')
                    {
                        j += 1;
                    }
                    if j < b.len() && b[j] == b'%' {
                        if let Some(rep) = f(&s[i + 1..j]) {
                            out.push_str(&s[last..i]);
                            out.push_str(&rep);
                            last = j + 1;
                        }
                        i = j + 1;
                        continue;
                    }
                }
                i += 1;
            }
            out.push_str(&s[last..]);
            Value::String(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| fill(x, f)).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), fill(x, f))).collect()),
        other => other.clone(),
    }
}

fn validate(f: &PresetFile) -> Result<(), FocusDevError> {
    if f.format_version != PRESET_FORMAT_VERSION {
        return Err(FocusDevError::Preset(format!(
            "format_version {} is not {PRESET_FORMAT_VERSION}",
            f.format_version
        )));
    }
    let mut ids = BTreeSet::new();
    for p in &f.presets {
        if !is_slug(&p.id) || !ids.insert(p.id.as_str()) {
            return Err(bad(&p.id, "id must be a unique lowercase identifier"));
        }
        if p.description.trim().is_empty() || !brace_free(&p.description) {
            return Err(bad(&p.id, "description must be non-empty and brace-free"));
        }
        let mut names = BTreeSet::new();
        for d in &p.params {
            if !is_slug(&d.name) || !names.insert(d.name.as_str()) {
                return Err(bad(
                    &p.id,
                    format!("param '{}' must be a unique identifier", d.name),
                ));
            }
            match &d.kind {
                ParamKind::Slug => {}
                ParamKind::Enum { values } => {
                    if values.is_empty()
                        || values.iter().any(|v| !is_slug(v))
                        || values.iter().collect::<BTreeSet<_>>().len() != values.len()
                    {
                        return Err(bad(
                            &p.id,
                            format!("enum param '{}' needs unique identifier values", d.name),
                        ));
                    }
                }
                ParamKind::Int { min, max } => {
                    if min > max {
                        return Err(bad(
                            &p.id,
                            format!("int param '{}' has min above max", d.name),
                        ));
                    }
                }
            }
        }
        let mut locals = BTreeSet::new();
        for l in &p.locals {
            if !is_slug(l) || !locals.insert(l.as_str()) || names.contains(l.as_str()) {
                return Err(bad(
                    &p.id,
                    format!("local '{l}' must be a unique identifier and not a param"),
                ));
            }
        }
        if p.rows.is_empty() || p.rows.iter().any(|r| !r.is_object()) {
            return Err(bad(&p.id, "rows must be a non-empty list of objects"));
        }
        let mut used = Vec::new();
        p.rows.iter().for_each(|r| collect_slots(r, &mut used));
        for u in &used {
            if !names.contains(u.as_str()) {
                return Err(bad(&p.id, format!("slot %{u}% names no param")));
            }
        }
        for n in &names {
            if !used.iter().any(|u| u == n) {
                return Err(bad(&p.id, format!("param '{n}' is never used")));
            }
        }
        // A trial expansion with a sample of every param proves the rows parse as spec rows.
        let sample = sample_use(p);
        expand_preset(p, &sample)?;
    }
    Ok(())
}

/// A valid use of `p`: the first enum value, the minimum, a fixed slug per slug param.
pub fn sample_use(p: &Preset) -> PresetUse {
    let mut params = std::collections::BTreeMap::new();
    for d in &p.params {
        let v = match &d.kind {
            ParamKind::Slug => Value::String(format!("sample_{}", d.name)),
            ParamKind::Enum { values } => Value::String(values[0].clone()),
            ParamKind::Int { min, .. } => Value::from(*min),
        };
        params.insert(d.name.clone(), v);
    }
    PresetUse {
        preset: p.id.clone(),
        prefix: "sample".to_owned(),
        params,
    }
}

/// Expands one use against a preset file.
pub fn expand(file: &PresetFile, u: &PresetUse) -> Result<Vec<SpecRow>, FocusDevError> {
    let p = file
        .presets
        .iter()
        .find(|p| p.id == u.preset)
        .ok_or_else(|| FocusDevError::Preset(format!("unknown preset '{}'", u.preset)))?;
    expand_preset(p, u)
}

fn expand_preset(p: &Preset, u: &PresetUse) -> Result<Vec<SpecRow>, FocusDevError> {
    if !is_slug(&u.prefix) || u.prefix.len() > 24 {
        return Err(bad(
            &p.id,
            "the use's `as` prefix must be a short lowercase identifier",
        ));
    }
    for k in u.params.keys() {
        if !p.params.iter().any(|d| &d.name == k) {
            return Err(bad(&p.id, format!("unknown param '{k}'")));
        }
    }
    let mut values: Vec<(&str, String)> = Vec::new();
    for d in &p.params {
        let v = u
            .params
            .get(&d.name)
            .ok_or_else(|| bad(&p.id, format!("missing param '{}'", d.name)))?;
        let text = match (&d.kind, v) {
            (ParamKind::Slug, Value::String(s)) if is_slug(s) => s.clone(),
            (ParamKind::Enum { values }, Value::String(s)) if values.contains(s) => s.clone(),
            (ParamKind::Int { min, max }, Value::Number(n))
                if n.as_i64().is_some_and(|i| i >= *min && i <= *max) =>
            {
                n.to_string()
            }
            _ => {
                return Err(bad(
                    &p.id,
                    format!("param '{}' is not a valid {}", d.name, kind_name(&d.kind)),
                ))
            }
        };
        if !brace_free(&text) {
            return Err(bad(&p.id, format!("param '{}' contains a brace", d.name)));
        }
        if p.locals.iter().any(|l| l == &text) {
            return Err(bad(
                &p.id,
                format!("param '{}' equals a name the preset defines", d.name),
            ));
        }
        values.push((d.name.as_str(), text));
    }
    let lookup = |name: &str| {
        values
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.clone())
    };
    let mut out = Vec::new();
    for tpl in &p.rows {
        let filled = fill(tpl, &lookup);
        let mut row: SpecRow = serde_json::from_value(filled)
            .map_err(|e| bad(&p.id, format!("a row does not parse as a spec row: {e}")))?;
        rename(&mut row, &u.prefix, &p.locals);
        out.push(row);
    }
    Ok(out)
}

fn kind_name(k: &ParamKind) -> &'static str {
    match k {
        ParamKind::Slug => "identifier",
        ParamKind::Enum { .. } => "member of its list",
        ParamKind::Int { .. } => "integer in range",
    }
}

fn rename(row: &mut SpecRow, prefix: &str, locals: &[String]) {
    let re = |name: &str| -> String {
        if locals.iter().any(|l| l == name) {
            format!("{prefix}_{name}")
        } else {
            name.to_owned()
        }
    };
    row.action_id = format!("{prefix}_{}", row.action_id);
    if let Some(g) = &mut row.guard {
        g.var = re(&g.var);
    }
    for g in &mut row.guard_clauses {
        g.var = re(&g.var);
    }
    if let Some(s) = &mut row.store {
        if let Some(Value::Object(m)) = &mut s.args {
            for v in m.values_mut() {
                if let Value::String(name) = v {
                    *name = re(name);
                }
            }
        }
    }
    if let Value::Object(cfg) = &mut row.config {
        if let Some(Value::String(v)) = cfg.get_mut("output_var") {
            *v = re(v);
        }
        if let Some(Value::Array(a)) = cfg.get_mut("declared_inputs") {
            for x in a {
                if let Value::String(name) = x {
                    *name = re(name);
                }
            }
        }
        if let Some(Value::String(t)) = cfg.get_mut("prompt_template") {
            for l in locals {
                *t = t.replace(&format!("{{{l}}}"), &format!("{{{prefix}_{l}}}"));
            }
        }
    }
}
