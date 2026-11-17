// src-tauri/src/conductor/focus_dev/render.rs
//
// The SQL renderer (items.id=898, decisions.id=951 ruling 3): FocusSeed -> a data-only
// migration file (shared_NNN.sql) or a baseline fragment. Pure text, no database.
//
// Shape: header comments (no semicolons), DELETE of every row the Focus owns in shared.db,
// plain INSERT of every row (never INSERT OR IGNORE: it cannot repair a stale row), and for
// a migration the schema_version row. The same row structs the one writer
// (focus_composition_store::write_focus) takes are rendered here, and a test applies this
// text to a scratch database and compares every seeded table with what the writer made.
//
// The runner splits a file with migrations::parse_statements, which is not a SQL parser:
// it drops blank lines and lines starting "--" even inside a string literal, ends a
// statement at a line ending in a semicolon, and treats a line starting "CREATE TRIGGER"
// specially. So:
//   * a string containing a semicolon is REFUSED (Schema Authoring Rule), naming the row;
//   * a multi-line string is written literally, unless one of its lines is blank, starts
//     with "--", starts with "create trigger" or is "end": then its newlines are written
//     as char(10) joins, which gives the same value on lines that cannot be misread;
//   * a carriage return or NUL is refused;
//   * the finished text is parsed back with parse_statements and must equal the statements
//     that were rendered, so anything the runner would mangle is an error here.

use sha2::{Digest, Sha256};

use crate::conductor::focus_dev::FocusDevError;
use crate::persistence::focus_composition_store::FocusSeed;
use crate::persistence::migrations::parse_statements;

/// What is being emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmitKind {
    /// A numbered data-only schema file, with its schema_version row.
    Migration { number: u32 },
    /// The same statements without the schema_version row, a fragment for the
    /// one-baseline-per-prefix file (items.id=633, decisions.id=857).
    Baseline,
}

/// SHA-256 of the spec's text bytes, lowercase hex.
pub fn spec_sha256(spec_text: &str) -> String {
    let mut h = Sha256::new();
    h.update(spec_text.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The header line the drift check reads.
pub fn header_line(focus_id: &str, sha: &str) -> String {
    format!("-- focus_dev: {focus_id} spec-sha256 {sha}")
}

fn render_err(msg: impl std::fmt::Display) -> FocusDevError {
    FocusDevError::Render(msg.to_string())
}

fn needs_char10(s: &str) -> bool {
    s.lines().any(|l| {
        let t = l.trim();
        let up = t.to_uppercase();
        t.is_empty()
            || t.starts_with("--")
            || up.starts_with("CREATE TRIGGER")
            || up.starts_with("CREATE OR REPLACE TRIGGER")
            || up == "END"
            || up == "END;"
    })
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// One SQL string expression for `s`, or an error naming `ctx`.
fn lit(ctx: &str, s: &str) -> Result<String, FocusDevError> {
    if s.contains(';') {
        return Err(render_err(format!(
            "{ctx}: a text value contains a semicolon, which the migration runner cannot read inside a string literal (Schema Authoring Rule), reword it"
        )));
    }
    if s.contains('\r') || s.contains('\u{0}') {
        return Err(render_err(format!(
            "{ctx}: a text value contains a carriage return or NUL"
        )));
    }
    if s.contains('\n') && needs_char10(s) {
        let parts: Vec<String> = s.split('\n').map(quote).collect();
        return Ok(parts.join(" || char(10) || "));
    }
    Ok(quote(s))
}

fn opt(ctx: &str, s: &Option<String>) -> Result<String, FocusDevError> {
    match s {
        Some(v) => lit(ctx, v),
        None => Ok("NULL".to_owned()),
    }
}

fn insert(table: &str, cols: &[&str], vals: Vec<String>) -> String {
    let mut out = format!(
        "INSERT INTO {table}\n    ({})\nVALUES\n    (",
        cols.join(", ")
    );
    out.push_str(&vals.join(",\n     "));
    out.push(')');
    out
}

const NOW: &str = "datetime('now')";

/// The statements, in order, without the trailing semicolons.
fn statements(seed: &FocusSeed, kind: EmitKind, sha: &str) -> Result<Vec<String>, FocusDevError> {
    let f = &seed.focus;
    let id = f.focus_id.as_str();
    let fid = lit(id, id)?;
    let mut out: Vec<String> = Vec::new();
    out.push(format!(
        "DELETE FROM focus_block_customization WHERE composition_id IN\n    (SELECT id FROM focus_block_compositions WHERE focus_id = {fid})"
    ));
    // Children first, the focuses row last.
    for table in [
        "focus_block_compositions",
        "focus_launch_options",
        "focus_mode_profiles",
        "focus_launch_questions",
        "focus_launch_display",
        "focuses",
    ] {
        out.push(format!("DELETE FROM {table} WHERE focus_id = {fid}"));
    }
    let suggest = serde_json::to_string(&f.suggest_in_focuses).map_err(render_err)?;
    out.push(insert(
        "focuses",
        &[
            "focus_id",
            "display_name",
            "description",
            "version",
            "max_routing_tier",
            "output_type",
            "suggest_in_focuses",
            "multi_source_validation",
            "generic_title_template",
            "high_priority_trigger_anchor_field",
            "high_priority_trigger_offset",
            "status",
            "collection_posture",
            "created_at",
            "updated_at",
        ],
        vec![
            fid.clone(),
            lit(id, &f.display_name)?,
            lit(id, &f.description)?,
            lit(id, &f.version)?,
            lit(id, &f.max_routing_tier)?,
            lit(id, &f.output_type)?,
            lit(id, &suggest)?,
            i64::from(f.multi_source_validation).to_string(),
            lit(id, &f.generic_title_template)?,
            opt(id, &f.high_priority_trigger_anchor_field)?,
            opt(id, &f.high_priority_trigger_offset)?,
            lit(id, &f.status)?,
            lit(id, &f.collection_posture)?,
            NOW.to_owned(),
            NOW.to_owned(),
        ],
    ));
    for (i, c) in seed.compositions.iter().enumerate() {
        let ctx = format!("{id} row '{}'", c.action_id);
        let comp = format!("{id}:{}", c.action_id);
        out.push(insert(
            "focus_block_compositions",
            &[
                "id",
                "focus_id",
                "action_id",
                "block_stable_id",
                "sequence_index",
                "schedule_trigger_anchor_field",
                "schedule_trigger_offset",
                "guard_var",
                "guard_op",
                "guard_value",
                "guard_clauses",
                "store_op",
                "store_target",
                "store_args",
                "created_at",
                "updated_at",
            ],
            vec![
                lit(&ctx, &comp)?,
                fid.clone(),
                lit(&ctx, &c.action_id)?,
                opt(&ctx, &c.block_stable_id)?,
                i.to_string(),
                opt(&ctx, &c.schedule_trigger_anchor_field)?,
                opt(&ctx, &c.schedule_trigger_offset)?,
                opt(&ctx, &c.guard_var)?,
                opt(&ctx, &c.guard_op)?,
                opt(&ctx, &c.guard_value)?,
                opt(&ctx, &c.guard_clauses)?,
                opt(&ctx, &c.store_op)?,
                opt(&ctx, &c.store_target)?,
                opt(&ctx, &c.store_args)?,
                NOW.to_owned(),
                NOW.to_owned(),
            ],
        ));
        out.push(insert(
            "focus_block_customization",
            &[
                "id",
                "composition_id",
                "persona_id",
                "customization",
                "created_at",
                "updated_at",
            ],
            vec![
                lit(&ctx, &format!("{comp}:default"))?,
                lit(&ctx, &comp)?,
                "NULL".to_owned(),
                lit(&ctx, &c.customization)?,
                NOW.to_owned(),
                NOW.to_owned(),
            ],
        ));
    }
    for (qi, q) in seed.questions.iter().enumerate() {
        let ctx = format!("{id} question '{}'", q.question_id);
        out.push(insert(
            "focus_launch_questions",
            &[
                "focus_id",
                "question_id",
                "slot",
                "kind",
                "sort_order",
                "asked",
                "default_option",
                "prompt",
            ],
            vec![
                fid.clone(),
                lit(&ctx, &q.question_id)?,
                lit(&ctx, &q.slot)?,
                lit(&ctx, &q.kind)?,
                qi.to_string(),
                i64::from(q.asked).to_string(),
                lit(&ctx, &q.default_option)?,
                lit(&ctx, &q.prompt)?,
            ],
        ));
        for (oi, o) in q.options.iter().enumerate() {
            let octx = format!("{ctx} option '{}'", o.option_id);
            out.push(insert(
                "focus_launch_options",
                &[
                    "focus_id",
                    "question_id",
                    "option_id",
                    "sort_order",
                    "label",
                    "hint",
                ],
                vec![
                    fid.clone(),
                    lit(&octx, &q.question_id)?,
                    lit(&octx, &o.option_id)?,
                    oi.to_string(),
                    lit(&octx, &o.label)?,
                    lit(&octx, &o.hint)?,
                ],
            ));
            if let Some(env) = &o.envelope {
                out.push(insert(
                    "focus_mode_profiles",
                    &["focus_id", "option_id", "envelope"],
                    vec![fid.clone(), lit(&octx, &o.option_id)?, lit(&octx, env)?],
                ));
            }
        }
    }
    if let Some(t) = &seed.ui_one_liner {
        out.push(insert(
            "focus_launch_display",
            &["focus_id", "ui_one_liner"],
            vec![fid.clone(), lit(id, t)?],
        ));
    }
    if let EmitKind::Migration { number } = kind {
        let desc = format!(
            "items.id=898: focus_dev emit of Focus {id} from its spec (sha256 {}) -- data only",
            &sha[..sha.len().min(12)]
        );
        out.push(format!(
            "INSERT OR IGNORE INTO schema_version (version, applied_at, description)\nVALUES ({number}, {NOW}, {})",
            lit(id, &desc)?
        ));
    }
    Ok(out)
}

/// The whole file text.
pub fn render(seed: &FocusSeed, spec_sha: &str, kind: EmitKind) -> Result<String, FocusDevError> {
    let id = seed.focus.focus_id.as_str();
    let stmts = statements(seed, kind, spec_sha)?;
    let mut text = String::new();
    match kind {
        EmitKind::Migration { number } => {
            text.push_str(&format!("-- shared_{number:03}.sql\n"));
        }
        EmitKind::Baseline => {
            text.push_str(&format!("-- focus_dev baseline fragment for Focus {id}\n"));
        }
    }
    text.push_str("--\n");
    text.push_str(
        "-- GENERATED by focus_dev (items.id=898) from the Focus spec. Do not hand-edit:\n",
    );
    text.push_str(
        "-- edit the spec and emit again. A test fails if this header's hash is not the spec's.\n",
    );
    text.push_str(&header_line(id, spec_sha));
    text.push_str("\n--\n");
    text.push_str(
        "-- Data only: DELETE then INSERT of every row this Focus owns in shared.db. No table or\n",
    );
    text.push_str(
        "-- column is added, so the schema-shape golden does not change. Plain INSERT, never\n",
    );
    text.push_str("-- INSERT OR IGNORE, because that cannot repair a stale row.\n");
    text.push_str("--\n");
    text.push_str(
        "-- CONTENT CLASS (decisions.id=862): shipped core Focus content, identical for every\n",
    );
    text.push_str("-- install, no personal values.\n");
    match kind {
        EmitKind::Migration { .. } => {
            text.push_str(
                "-- items.id=633 folds this file into the shared baseline with the others.\n",
            );
            text.push_str(
                "-- Register it in migrations.rs SCHEMA_FILES and re-pin the version tests.\n",
            );
        }
        EmitKind::Baseline => {
            text.push_str("-- Paste into the shared baseline (decisions.id=857, items.id=633). It carries no\n");
            text.push_str("-- schema_version row.\n");
        }
    }
    text.push('\n');
    for s in &stmts {
        text.push_str(s);
        text.push_str(";\n\n");
    }
    // The runner must read back exactly what was rendered, and only data statements.
    let parsed = parse_statements(&text);
    if parsed.len() != stmts.len() || parsed.iter().zip(&stmts).any(|(a, b)| a != b.trim()) {
        return Err(render_err(
            "the migration runner would read these statements differently from how they were rendered",
        ));
    }
    for s in &parsed {
        let up = s.to_uppercase();
        let ok = up.starts_with("DELETE FROM ")
            || up.starts_with("INSERT INTO ")
            || up.starts_with("INSERT OR IGNORE INTO SCHEMA_VERSION ");
        if !ok {
            return Err(render_err("a rendered statement is not DELETE or INSERT"));
        }
    }
    Ok(text)
}

/// Picks the number of the next shared schema file. `registered` is the SCHEMA_FILES list
/// and `on_disk` the schema directory listing: they must agree, or the tree is
/// inconsistent and the tool will not guess. `requested` must be above the highest.
pub fn next_shared_number(
    registered: &[u32],
    on_disk: &[u32],
    requested: Option<u32>,
) -> Result<u32, FocusDevError> {
    let mut a = registered.to_vec();
    let mut b = on_disk.to_vec();
    a.sort_unstable();
    b.sort_unstable();
    a.dedup();
    b.dedup();
    if a != b {
        let only_reg: Vec<_> = a.iter().filter(|v| !b.contains(v)).collect();
        let only_disk: Vec<_> = b.iter().filter(|v| !a.contains(v)).collect();
        return Err(render_err(format!(
            "SCHEMA_FILES and the schema directory disagree (registered only: {only_reg:?}, on disk only: {only_disk:?})"
        )));
    }
    let max = a.last().copied().unwrap_or(0);
    match requested {
        None => Ok(max + 1),
        Some(n) if n > max => Ok(n),
        Some(n) => Err(render_err(format!(
            "--number {n} is not above the highest existing shared version {max}"
        ))),
    }
}
