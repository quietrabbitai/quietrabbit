// src-tauri/src/conductor/model_block/admission_tests.rs
//
// items.id=894 part 1 (decisions.id=941 point 11, decisions.id=942 point 11): the
// ADMISSION TEST over the block registry. A block is admitted to the registry only
// if it passes every check here, for every id in REGISTERED_STABLE_IDS, so a new
// block that skips one fails CI instead of waiting for a review to notice:
//
//   1  its kind is consistent (a pure block has a handler and no spec, a model
//      block a spec and no handler, and no id is both)
//   2  it is a mirror row (shared_043.sql adds the model blocks) and a member of
//      the building_blocks mirror as inline_composable
//   3  its catalog row, from the checked-in export of qr_docs.db, has the floor
//      declaration, capability toggles and customization notes filled in
//   4  its declared configuration schema accepts its example, and rejects the
//      example with an unknown key at every level, with each required key removed,
//      and with each closed vocabulary set to a value outside it
//   5  it names an output variable, which is not a system token or a bound name
//   6  a Focus built from it loads through the REAL loader (lint, guard and
//      placement analysis included)
//   7  a pure block leaves the track alone and leaks no input text, and a model
//      block expands to an ordinary local step (no block_stable_id, a requirement,
//      no handoff, no {user_input} or {persona_context})
//   8  the hosted rule: allowing hosted_api and naming {user_input} or
//      {persona_context} is refused
//   9  tighten() never widens an envelope (property test, config.rs) and no
//      configuration key can switch a gate off (the pinned key list below)
//
// The catalog check reads a checked-in file, not qr_docs.db: the running app and
// CI never query the coordination database. Chat-PM re-exports the file after
// writing the catalog rows for a new block.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::conductor::block_dispatch::{
    block_handler, block_kind, registered_block, RegisteredBlock, PURE_STABLE_IDS,
    REGISTERED_STABLE_IDS,
};
use crate::conductor::model_block::config::{tighten, Envelope, EnvelopeOverride};
use crate::conductor::model_block::document_type::{
    registry, DocumentType, Settings, SettingsPatch,
};
use crate::conductor::model_block::{
    BlockKind, ExpandInput, EXTERNAL_MODEL_BLOCKS_ALLOWED, MODEL_STABLE_IDS,
};
use crate::conductor::tokens::{is_system_token, ExternalAccess, StepType};
use crate::conductor::types::{TaskStep, TaskTrack};

const CATALOG: &str = include_str!("../../../tests/fixtures/building_blocks_catalog.json");

// ---------------------------------------------------------------------------
// Every JSON key a model block's configuration may carry. A test serialises a
// maximal example of each block, the document type and the two override shapes,
// and requires set equality with this list: adding a field forces a reviewed edit
// of it. No key may name a gate, a scan, a review, consent, an audit, a bypass, a
// skip or a disable (FORBIDDEN_SUBSTRINGS), so no configuration field can switch
// a gate off.
// ---------------------------------------------------------------------------

const PINNED_KEYS: &[&str] = &[
    "based_on",
    "brief_var",
    "change_var",
    "class_preference",
    "common",
    "context_fields",
    "context_var",
    "declared_inputs",
    "destinations",
    "difficulty",
    "directness",
    "display_name",
    "document_type",
    "draft",
    "draft_var",
    "flag_unsupplied_references",
    "form",
    "formality",
    "format_note",
    "highest_tier",
    "id",
    "input_var",
    "latitude",
    "length",
    "length_words",
    "max",
    "min",
    "mode",
    "name",
    "need",
    "output_size",
    "output_var",
    "pacing",
    "params",
    "quality_floor",
    "range",
    "revise",
    "sender",
    "sender_var",
    "settings",
    "spec",
    "task_type",
    "temperature",
    "tone",
    "top_p",
    "voice",
];

const FORBIDDEN_SUBSTRINGS: &[&str] = &[
    "gate",
    "scan",
    "review",
    "consent",
    "audit",
    "bypass",
    "skip",
    "disable",
    "override_tier",
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A valid customization of a registered block, whatever its kind.
fn example_of(id: &str) -> Value {
    match registered_block(id).unwrap() {
        RegisteredBlock::Pure(h) => (h.example)(),
        RegisteredBlock::Model(s) => (s.example)(),
    }
}

/// Parse and validate a customization the way the loader does.
fn check(id: &str, v: &Value) -> Result<(), String> {
    match registered_block(id).unwrap() {
        RegisteredBlock::Pure(h) => (h.validate)(v),
        RegisteredBlock::Model(s) => (s.parse)(v, EXTERNAL_MODEL_BLOCKS_ALLOWED).map(|_| ()),
    }
}

/// (JSON pointer of an object, key) for every key of every object in `v`.
fn key_paths(v: &Value, prefix: &str, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                out.push((prefix.to_owned(), k.clone()));
                key_paths(child, &format!("{prefix}/{k}"), out);
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                key_paths(child, &format!("{prefix}/{i}"), out);
            }
        }
        _ => {}
    }
}

fn all_keys(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                out.insert(k.clone());
                all_keys(child, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|c| all_keys(c, out)),
        _ => {}
    }
}

/// The variable names a block's example reads: the names a test Focus must
/// produce earlier. A system token (voice_profile) needs no producer.
fn input_names(id: &str, ex: &Value) -> Vec<String> {
    match registered_block(id).unwrap() {
        RegisteredBlock::Pure(_) => vec![ex["input_var"].as_str().unwrap().to_owned()],
        RegisteredBlock::Model(_) => ex["common"]["declared_inputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap().to_owned())
            .filter(|n| !is_system_token(n))
            .collect(),
    }
}

fn output_var_of(id: &str, ex: &Value) -> Option<String> {
    match registered_block(id).unwrap() {
        RegisteredBlock::Pure(h) => (h.output_var)(ex),
        RegisteredBlock::Model(_) => ex["common"]["output_var"].as_str().map(str::to_owned),
    }
}

// ---------------------------------------------------------------------------
// 1. Kind consistency
// ---------------------------------------------------------------------------

#[test]
fn every_registered_id_has_exactly_one_kind_and_the_lists_agree() {
    let mut pure = BTreeSet::new();
    let mut model = BTreeSet::new();
    for id in REGISTERED_STABLE_IDS {
        let kind = block_kind(id).unwrap_or_else(|| panic!("{id} is listed but not registered"));
        match (registered_block(id).unwrap(), kind) {
            (RegisteredBlock::Pure(h), BlockKind::Pure) => {
                assert_eq!(h.stable_id, *id);
                assert!(crate::conductor::model_block::model_block_spec(id).is_none());
                pure.insert(*id);
            }
            (RegisteredBlock::Model(s), BlockKind::Model) => {
                assert_eq!(s.stable_id, *id);
                assert!(
                    block_handler(id).is_none(),
                    "{id} must not also have a handler"
                );
                model.insert(*id);
            }
            _ => panic!("{id}: kind does not match its registration"),
        }
    }
    assert_eq!(
        pure,
        PURE_STABLE_IDS.iter().copied().collect::<BTreeSet<_>>()
    );
    assert_eq!(
        model,
        MODEL_STABLE_IDS.iter().copied().collect::<BTreeSet<_>>()
    );
    assert_eq!(pure.len() + model.len(), REGISTERED_STABLE_IDS.len());
    // No seed block is registered yet: its contract belongs to B7a.
    assert!(REGISTERED_STABLE_IDS
        .iter()
        .all(|id| block_kind(id) != Some(BlockKind::Seed)));
    // Nothing unregistered resolves.
    for id in [
        "cb-01", "cb-02", "cb-10", "pc-03", "pc-04", "cb-15", "nope", "",
    ] {
        assert!(registered_block(id).is_none(), "{id}");
    }
}

// ---------------------------------------------------------------------------
// 3. Catalog row
// ---------------------------------------------------------------------------

fn filled(v: &Value, key: &str) -> bool {
    v[key]
        .as_str()
        .is_some_and(|s| !s.trim().is_empty() && s.trim() != "None")
}

#[test]
fn every_registered_block_has_a_filled_catalog_row_in_the_checked_in_export() {
    let export: Value = serde_json::from_str(CATALOG).expect("catalog export must parse");
    let rows = export["rows"].as_array().expect("rows");
    for id in REGISTERED_STABLE_IDS {
        let row = rows
            .iter()
            .find(|r| r["stable_id"] == *id)
            .unwrap_or_else(|| panic!("{id} has no catalog row in the export"));
        for col in [
            "floor_declaration",
            "capability_toggles",
            "generic_customization_notes",
        ] {
            assert!(filled(row, col), "{id}: catalog column {col} is empty");
        }
        assert_eq!(row["invocation_mode"], "inline_composable", "{id}");
        let want = match block_kind(id).unwrap() {
            BlockKind::Pure => "pure",
            BlockKind::Model => "model",
            BlockKind::Seed => "seed",
        };
        assert_eq!(
            row["kind"], want,
            "{id}: the export records a different kind"
        );
    }
    // The export carries no row for a block that is not registered.
    for row in rows {
        let id = row["stable_id"].as_str().unwrap();
        assert!(
            REGISTERED_STABLE_IDS.contains(&id),
            "{id} is in the export but not registered"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Schema accepts the example and rejects every mutation
// ---------------------------------------------------------------------------

/// Keys whose value is a closed vocabulary: set to a value outside it, the
/// configuration must stop parsing.
const VOCABULARY_KEYS: &[&str] = &[
    "task_type",
    "difficulty",
    "output_size",
    "class_preference",
    "highest_tier",
    "min",
    "max",
    "mode",
    "kind",
    "on_fail",
    "document_type",
];

#[test]
fn every_example_validates_and_every_mutation_of_it_fails() {
    for id in REGISTERED_STABLE_IDS {
        let ex = example_of(id);
        check(id, &ex).unwrap_or_else(|e| panic!("{id}: its own example must validate: {e}"));

        let mut keys = Vec::new();
        key_paths(&ex, "", &mut keys);
        assert!(!keys.is_empty());

        // An unknown key at every object, the root included.
        let mut objects: BTreeSet<String> = keys.iter().map(|(p, _)| p.clone()).collect();
        objects.insert(String::new());
        for ptr in &objects {
            let mut m = ex.clone();
            m.pointer_mut(ptr)
                .and_then(Value::as_object_mut)
                .unwrap_or_else(|| panic!("{id}: {ptr} is not an object"))
                .insert("zz_unknown".to_owned(), json!(1));
            assert!(
                check(id, &m).is_err(),
                "{id}: an unknown key at '{ptr}' was accepted"
            );
        }

        // Each required key removed (a null example value marks an optional one).
        for (ptr, key) in &keys {
            if ex.pointer(ptr).unwrap()[key].is_null() {
                continue;
            }
            let mut m = ex.clone();
            m.pointer_mut(ptr)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(key);
            assert!(
                check(id, &m).is_err(),
                "{id}: '{key}' at '{ptr}' was removed and accepted"
            );
        }

        // Each closed vocabulary set outside it.
        for (ptr, key) in &keys {
            let v = &ex.pointer(ptr).unwrap()[key];
            if VOCABULARY_KEYS.contains(&key.as_str()) && v.is_string() {
                let mut m = ex.clone();
                m.pointer_mut(ptr).unwrap()[key.as_str()] = json!("zz_bad");
                assert!(
                    check(id, &m).is_err(),
                    "{id}: '{key}' = zz_bad at '{ptr}' was accepted"
                );
            }
        }
        if ex.pointer("/common/range/destinations").is_some() {
            let mut m = ex.clone();
            m["common"]["range"]["destinations"] = json!(["zz_bad"]);
            assert!(
                check(id, &m).is_err(),
                "{id}: an unknown destination was accepted"
            );
        }

        // Not an object at all, and empty.
        assert!(check(id, &json!([1, 2])).is_err());
        assert!(check(id, &json!({})).is_err());
    }
}

// ---------------------------------------------------------------------------
// 5. Output variable
// ---------------------------------------------------------------------------

#[test]
fn every_block_names_an_output_variable_that_is_not_reserved() {
    for id in REGISTERED_STABLE_IDS {
        let out =
            output_var_of(id, &example_of(id)).unwrap_or_else(|| panic!("{id}: no output_var"));
        assert!(!out.is_empty());
        assert!(!is_system_token(&out), "{id}: {out} is a system token");
        assert!(
            !crate::conductor::declared_inputs::bound_names().contains(&out.as_str()),
            "{id}: {out} is a bound name"
        );
        // A model block that names a reserved output is refused by its own parser.
        if block_kind(id) == Some(BlockKind::Model) {
            for reserved in [
                "user_input",
                "persona_context",
                "voice_profile",
                "previous_output",
            ] {
                let mut m = example_of(id);
                m["common"]["output_var"] = json!(reserved);
                assert!(
                    check(id, &m).is_err(),
                    "{id}: output_var {reserved} accepted"
                );
            }
            for bound in crate::conductor::declared_inputs::bound_names() {
                let mut m = example_of(id);
                m["common"]["output_var"] = json!(bound);
                assert!(check(id, &m).is_err(), "{id}: output_var {bound} accepted");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 6. Loads through the real loader (and 2: the mirror)
// ---------------------------------------------------------------------------

struct TestEnv {
    _tempdir: tempfile::TempDir,
    _lock: tokio::sync::MutexGuard<'static, ()>,
    saved_root: Option<String>,
    pool: sqlx::SqlitePool,
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        match &self.saved_root {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }
}

async fn setup() -> TestEnv {
    let lock = crate::test_support::ENV_MUTEX.lock().await;
    let saved_root = std::env::var("QR_DATA_ROOT").ok();
    let tempdir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("QR_DATA_ROOT", tempdir.path());
    crate::persistence::migrations::migrate_shared_db()
        .await
        .expect("shared.db migration");
    let pool =
        sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
            &crate::providers::utils::db_path_shared(),
        ))
        .await
        .expect("shared.db pool");
    TestEnv {
        _tempdir: tempdir,
        _lock: lock,
        saved_root,
        pool,
    }
}

/// One composition row: (action_id, block_stable_id, customization).
type Row = (String, Option<String>, Value);

async fn seed_focus(pool: &sqlx::SqlitePool, focus_id: &str, ceiling: &str, rows: &[Row]) {
    sqlx::query(
        "INSERT INTO focuses
            (focus_id, display_name, description, version, max_routing_tier,
             output_type, suggest_in_focuses, multi_source_validation,
             generic_title_template, status, created_at, updated_at)
         VALUES (?, 'Admission Focus', '', '1.0', ?, 'general', '[]', 0, 'Hidden item',
                 'shipped', datetime('now'), datetime('now'))",
    )
    .bind(focus_id)
    .bind(ceiling)
    .execute(pool)
    .await
    .unwrap();
    for (i, (action, block, custom)) in rows.iter().enumerate() {
        let comp = format!("{focus_id}:{action}");
        sqlx::query(
            "INSERT INTO focus_block_compositions
                (id, focus_id, action_id, block_stable_id, sequence_index, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, datetime('now'), datetime('now'))",
        )
        .bind(&comp)
        .bind(focus_id)
        .bind(action)
        .bind(block)
        .bind(i as i64)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO focus_block_customization
                (id, composition_id, persona_id, customization, created_at, updated_at)
             VALUES (?, ?, NULL, ?, datetime('now'), datetime('now'))",
        )
        .bind(format!("{comp}:default"))
        .bind(&comp)
        .bind(custom.to_string())
        .execute(pool)
        .await
        .unwrap();
    }
}

/// A local ordinary step that produces `var`.
fn supply(var: &str) -> Row {
    (
        format!("supply_{var}"),
        None,
        json!({"routing_tier": 1, "task_type": "quick_response", "output_var": var,
               "prompt_template": "Say hello."}),
    )
}

fn ordinary_tail() -> Row {
    (
        "tail".to_owned(),
        None,
        json!({"routing_tier": 1, "task_type": "quick_response", "output_var": "tail_output",
               "prompt_template": "Say goodbye."}),
    )
}

/// The rows of a test Focus for a block: producers of its inputs, the block, and
/// (for a pure block, which may not be last) an ordinary tail.
fn rows_for(id: &str, ex: &Value, with_tail: bool) -> Vec<Row> {
    let mut rows: Vec<Row> = input_names(id, ex).iter().map(|n| supply(n)).collect();
    rows.push(("the_block".to_owned(), Some(id.to_owned()), ex.clone()));
    if with_tail {
        rows.push(ordinary_tail());
    }
    rows
}

#[tokio::test]
async fn the_mirror_has_a_row_for_every_registered_block() {
    let env = setup().await;
    for id in REGISTERED_STABLE_IDS {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM building_blocks
             WHERE stable_id = ? AND invocation_mode = 'inline_composable'",
        )
        .bind(id)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(n, 1, "{id}: no inline_composable mirror row");
    }
}

#[tokio::test]
async fn every_example_loads_through_the_real_loader_as_the_right_kind_of_step() {
    let env = setup().await;
    for id in REGISTERED_STABLE_IDS {
        let ex = example_of(id);
        let pure = block_kind(id) == Some(BlockKind::Pure);
        // A model block expands to an ordinary step, so it may be the LAST row, which
        // the placement lint forbids for a pure block.
        let focus_id = format!("admission-{id}");
        seed_focus(&env.pool, &focus_id, "local_only", &rows_for(id, &ex, pure)).await;

        let def = crate::conductor::lifecycle::load_focus_definition_from_db(&env.pool, &focus_id)
            .await
            .unwrap_or_else(|e| panic!("{id}: its example Focus must load: {e}"))
            .unwrap();
        let step = def
            .steps
            .iter()
            .find(|s| s.action_id == "the_block")
            .unwrap();

        if pure {
            assert_eq!(step.block_stable_id.as_deref(), Some(*id));
            assert!(step.requirement.is_none());
            assert_eq!(step.routing_tier, ExternalAccess::LocalOnly);
        } else {
            // 7: an ordinary local step.
            assert_eq!(
                step.block_stable_id, None,
                "{id}: a model block keeps no block id"
            );
            assert!(step.block_customization.is_none());
            let req = step
                .requirement
                .as_ref()
                .unwrap_or_else(|| panic!("{id}: no requirement"));
            assert_eq!(req.block_stable_id, *id);
            assert_eq!(step.routing_tier, ExternalAccess::LocalOnly);
            assert_eq!(
                step.external_access_override,
                Some(ExternalAccess::LocalOnly)
            );
            assert!(!step.requires_user_handoff);
            assert!(step.store_op.is_none() && !step.is_guarded());
            assert_eq!(
                step.output_var.as_deref(),
                output_var_of(id, &ex).as_deref()
            );
            assert!(!step.prompt_template.is_empty());
            assert!(matches!(
                step.step_type,
                StepType::Generate | StepType::VoiceTransform
            ));
            assert_eq!(
                step.guide_id,
                crate::conductor::model_block::MODEL_BLOCK_GUIDE_ID
            );
            // The template names exactly the declared inputs, and never the two names a
            // hosted block may not read.
            let named: BTreeSet<String> =
                crate::conductor::executor::find_tokens(&step.prompt_template)
                    .into_iter()
                    .collect();
            let declared: BTreeSet<String> = step.declared_inputs.iter().cloned().collect();
            assert_eq!(named, declared, "{id}");
            assert!(
                !named.contains("user_input") && !named.contains("persona_context"),
                "{id}"
            );
            // Its range is within the Focus ceiling and the block's own.
            assert!(req.range.highest_tier <= ExternalAccess::LocalOnly);
            // The output variable is what a later row can read.
            assert!(def
                .steps
                .iter()
                .all(|s| s.action_id != "tail" || s.output_var.is_some()));
        }
    }
}

#[tokio::test]
async fn a_model_block_may_not_load_above_local_while_external_blocks_are_not_allowed() {
    let env = setup().await;
    // This test describes the interim state: B10 flips the constant and replaces it.
    if EXTERNAL_MODEL_BLOCKS_ALLOWED {
        return;
    }
    for id in MODEL_STABLE_IDS {
        let mut ex = example_of(id);
        ex["common"]["range"]["destinations"] = json!(["local", "hosted_api"]);
        ex["common"]["range"]["highest_tier"] = json!("anonymous_preferred");
        let focus_id = format!("external-{id}");
        seed_focus(
            &env.pool,
            &focus_id,
            "unrestricted",
            &rows_for(id, &ex, false),
        )
        .await;
        let e = crate::conductor::lifecycle::load_focus_definition_from_db(&env.pool, &focus_id)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("not allowed yet"), "{id}: {e}");
        assert!(e.contains(id) && e.contains("the_block"), "{e}");
    }
}

#[tokio::test]
async fn a_cataloged_block_without_a_registration_still_fails_at_load() {
    let env = setup().await;
    seed_focus(
        &env.pool,
        "no-handler",
        "local_only",
        &[("the_block".to_owned(), Some("pc-03".to_owned()), json!({}))],
    )
    .await;
    let e = crate::conductor::lifecycle::load_focus_definition_from_db(&env.pool, "no-handler")
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("no compiled-in handler"), "{e}");
}

/// items.id=894 B0: an out-of-range routing_tier in a database row is a LOAD error,
/// not a panic.
#[tokio::test]
async fn an_out_of_range_routing_tier_in_a_database_row_is_a_load_error() {
    let env = setup().await;
    for (n, bad) in [0u64, 4, 9].into_iter().enumerate() {
        let focus_id = format!("bad-tier-{n}");
        seed_focus(
            &env.pool,
            &focus_id,
            "local_only",
            &[(
                "step_a".to_owned(),
                None,
                json!({"routing_tier": bad, "task_type": "quick_response",
                       "output_var": "x", "prompt_template": "hi"}),
            )],
        )
        .await;
        let e = crate::conductor::lifecycle::load_focus_definition_from_db(&env.pool, &focus_id)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("step_a") && e.contains(&format!("routing_tier value {bad}")),
            "{e}"
        );
    }
}

#[tokio::test]
async fn a_model_block_with_a_wrong_input_name_fails_the_declared_inputs_lint() {
    // The producers are missing, so the declared names are not real: the existing
    // declared-inputs lint (items.id=879) rejects the expanded step.
    let env = setup().await;
    seed_focus(
        &env.pool,
        "no-producer",
        "local_only",
        &[(
            "the_block".to_owned(),
            Some("cb-12".to_owned()),
            example_of("cb-12"),
        )],
    )
    .await;
    let e = crate::conductor::lifecycle::load_focus_definition_from_db(&env.pool, "no-producer")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("content_brief") || e.contains("sender_details"),
        "{e}"
    );
}

// ---------------------------------------------------------------------------
// 7. A pure block leaves the track alone and leaks nothing
// ---------------------------------------------------------------------------

#[test]
fn a_pure_block_does_not_change_the_track_and_leaks_no_input_text() {
    let marker = "ZX9-ADMISSION-MARKER";
    for id in PURE_STABLE_IDS {
        let ex = example_of(id);
        let h = block_handler(id).unwrap();
        let mut track = TaskTrack::new();
        track.add_step(TaskStep {
            action_id: "earlier".to_owned(),
            output_var: Some(ex["input_var"].as_str().unwrap().to_owned()),
            content: format!("{marker} {marker}"),
            sensitivity_severity: 1,
            routing_tier_used: 1,
            personal_flagged: false,
            audit_unavailable: false,
        });
        let before = track.steps().len();
        let out = (h.run)(&ex, &track).unwrap();
        assert_eq!(
            track.steps().len(),
            before,
            "{id}: run must not mutate the track"
        );
        assert!(
            !out.content.contains(marker),
            "{id}: the verdict leaked the input text"
        );
        assert_eq!(Some(out.output_var), output_var_of(id, &ex));
    }
}

// ---------------------------------------------------------------------------
// 8. The hosted rule, for every model block
// ---------------------------------------------------------------------------

/// The key of the spec variable a model block's example reads that a hosted block
/// must not point at user_input or persona_context.
fn spec_var_key(id: &str) -> &'static str {
    match id {
        "cb-12" => "brief_var",
        "cb-13" => "input_var",
        "cb-14" => "draft_var",
        other => panic!("no hosted-rule probe for {other}"),
    }
}

#[test]
fn a_model_block_that_allows_hosted_api_names_neither_user_input_nor_persona_context() {
    for id in MODEL_STABLE_IDS {
        let RegisteredBlock::Model(spec) = registered_block(id).unwrap() else {
            unreachable!()
        };
        let hosted = |mut ex: Value| {
            ex["common"]["range"]["destinations"] = json!(["local", "hosted_api"]);
            ex["common"]["range"]["highest_tier"] = json!("anonymous_preferred");
            ex
        };
        let expand = |ex: &Value| {
            let parsed = (spec.parse)(ex, true)?;
            (spec.expand)(
                &parsed,
                &ExpandInput {
                    action_id: "the_block",
                    focus_ceiling: ExternalAccess::Unrestricted,
                    settings: None,
                    external_allowed: true,
                },
            )
        };
        // As authored: allowed.
        assert!(expand(&hosted(example_of(id))).is_ok(), "{id}");

        for banned in ["user_input", "persona_context"] {
            let mut ex = hosted(example_of(id));
            let key = spec_var_key(id);
            let old = ex["spec"][key].as_str().unwrap().to_owned();
            ex["spec"][key] = json!(banned);
            let declared: Vec<Value> = ex["common"]["declared_inputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| {
                    if n == old.as_str() {
                        json!(banned)
                    } else {
                        n.clone()
                    }
                })
                .collect();
            ex["common"]["declared_inputs"] = json!(declared);
            let e = expand(&ex).unwrap_err();
            assert!(
                e.contains(&format!("{{{banned}}}")) && e.contains("hosted_api"),
                "{id}: {e}"
            );

            // The same name is fine in a block that cannot reach a hosted API.
            let mut local = example_of(id);
            let declared: Vec<Value> = local["common"]["declared_inputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| {
                    if n == old.as_str() {
                        json!(banned)
                    } else {
                        n.clone()
                    }
                })
                .collect();
            local["common"]["declared_inputs"] = json!(declared);
            local["spec"][key] = json!(banned);
            let parsed = (spec.parse)(&local, false).unwrap();
            assert!((spec.expand)(
                &parsed,
                &ExpandInput {
                    action_id: "the_block",
                    focus_ceiling: ExternalAccess::LocalOnly,
                    settings: None,
                    external_allowed: false,
                },
            )
            .is_ok());
        }
    }
}

// ---------------------------------------------------------------------------
// 9. No key can switch a gate off, and the envelope only narrows
// ---------------------------------------------------------------------------

#[test]
fn every_configuration_key_is_pinned_and_none_names_a_gate() {
    let mut keys = BTreeSet::new();
    // A maximal example of each model block: every optional key set.
    let mut draft = crate::conductor::model_block::draft::example();
    draft["common"]["class_preference"] = json!("small");
    draft["spec"]["context_var"] = json!("writer_context");
    let voice = crate::conductor::model_block::voice::example();
    let revise = crate::conductor::model_block::revise::example();
    for mut ex in [draft, voice, revise] {
        ex["common"]["class_preference"] = json!("small");
        all_keys(&ex, &mut keys);
    }
    // The document type (its length_words map keys are data, not configuration),
    // the settings and the two override shapes.
    let reg = registry().unwrap();
    let mut t: DocumentType = reg.document_type("rma_letter").unwrap().clone();
    t.based_on = Some("x".to_owned());
    t.params.draft.top_p = Some(0.9);
    let mut tj = serde_json::to_value(&t).unwrap();
    tj["length_words"] = json!(null);
    all_keys(&tj, &mut keys);
    all_keys(
        &serde_json::to_value(Settings::clone(&t.settings)).unwrap(),
        &mut keys,
    );
    all_keys(
        &serde_json::to_value(SettingsPatch {
            directness: Some("a".into()),
            formality: Some("a".into()),
            pacing: Some("a".into()),
            tone: Some("a".into()),
            length: Some("a".into()),
            latitude: Some(crate::conductor::model_block::document_type::Latitude::Free),
        })
        .unwrap(),
        &mut keys,
    );
    let envelope = EnvelopeOverride {
        quality_floor: Some(crate::conductor::model_block::config::QualityFloor {
            min: crate::conductor::model_block::config::QualityBar::Basic,
            max: crate::conductor::model_block::config::QualityBar::High,
        }),
        highest_tier: Some(ExternalAccess::LocalOnly),
        destinations: Some(vec![]),
        declared_inputs: Some(vec![]),
    };
    all_keys(&serde_json::to_value(&envelope).unwrap(), &mut keys);

    let pinned: BTreeSet<String> = PINNED_KEYS.iter().map(|k| (*k).to_owned()).collect();
    assert_eq!(
        keys, pinned,
        "a configuration key was added or removed: review it against FORBIDDEN_SUBSTRINGS and \
         update PINNED_KEYS"
    );
    for k in &keys {
        for bad in FORBIDDEN_SUBSTRINGS {
            assert!(!k.contains(bad), "configuration key '{k}' names '{bad}'");
        }
    }
}

#[test]
fn an_empty_override_is_the_identity_and_the_focus_ceiling_only_narrows_every_example() {
    for id in MODEL_STABLE_IDS {
        let RegisteredBlock::Model(spec) = registered_block(id).unwrap() else {
            unreachable!()
        };
        let parsed = (spec.parse)(&example_of(id), false).unwrap();
        let env: Envelope = parsed.common.envelope();
        assert_eq!(
            tighten(&env, &EnvelopeOverride::default()).as_ref(),
            Ok(&env)
        );
        for ceiling in [
            ExternalAccess::LocalOnly,
            ExternalAccess::AnonymousRequired,
            ExternalAccess::AnonymousPreferred,
            ExternalAccess::Unrestricted,
        ] {
            let step = (spec.expand)(
                &parsed,
                &ExpandInput {
                    action_id: "the_block",
                    focus_ceiling: ceiling,
                    settings: None,
                    external_allowed: false,
                },
            )
            .unwrap();
            assert!(step.routing_tier <= ceiling.min(env.highest_tier));
            let req = step.requirement.unwrap();
            assert!(req.range.highest_tier <= env.highest_tier);
            assert!(req
                .range
                .destinations
                .iter()
                .all(|d| env.destinations.contains(d)));
        }
    }
}
