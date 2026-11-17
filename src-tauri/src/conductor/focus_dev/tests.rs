// Tests for the pure half of focus_dev (items.id=898 stage 2).

use serde_json::json;

use super::preset::{self, ParamKind, PresetFile};
use super::shape;
use super::spec::{FocusSpec, PresetUse};
use super::FocusDevError;
use crate::persistence::focus_composition_store as store;

const BASIC: &str = include_str!("../../../tests/fixtures/focus_dev/spec_basic.json");
const PRESETS: &str = include_str!("../../../tests/fixtures/focus_dev/presets_fixture.json");
const WITH_PRESET: &str = include_str!("../../../tests/fixtures/focus_dev/spec_with_preset.json");

fn empty_presets() -> PresetFile {
    PresetFile {
        format_version: 1,
        presets: Vec::new(),
    }
}

fn fixture_presets() -> PresetFile {
    preset::parse(PRESETS).expect("fixture presets")
}

fn use_of(params: serde_json::Value) -> PresetUse {
    serde_json::from_value(json!({"preset": "fixture_review", "as": "a", "params": params}))
        .unwrap()
}

fn ok_params() -> serde_json::Value {
    json!({"source": "brief", "tone": "plain", "words": 100})
}

// -- spec ---------------------------------------------------------------------------------

#[test]
fn the_basic_spec_converts_to_the_writers_row_shape() {
    let seed = FocusSpec::from_json(BASIC)
        .unwrap()
        .to_seed(&empty_presets())
        .unwrap();
    shape::check(&seed).unwrap();
    assert_eq!(seed.focus.focus_id, "fixture-focus");
    assert_eq!(seed.focus.version, "1.0");
    assert_eq!(seed.focus.collection_posture, "per_field");
    let ids: Vec<&str> = seed
        .compositions
        .iter()
        .map(|c| c.action_id.as_str())
        .collect();
    assert_eq!(ids, ["intake", "polish", "plain_reply"]);
    let polish = &seed.compositions[1];
    assert_eq!(polish.guard_var.as_deref(), Some("launch_answer_1"));
    assert_eq!(polish.guard_op.as_deref(), Some("eq"));
    assert_eq!(polish.guard_value.as_deref(), Some("warm"));
    assert_eq!(seed.questions.len(), 1);
    assert_eq!(seed.questions[0].options[1].hint, "Friendlier");
    assert_eq!(seed.ui_one_liner.as_deref(), Some("A fixture Focus."));
}

#[test]
fn an_unknown_key_or_a_wrong_format_version_is_refused() {
    let mut v: serde_json::Value = serde_json::from_str(BASIC).unwrap();
    v["focus"]["colour"] = json!("red");
    assert!(matches!(
        FocusSpec::from_json(&v.to_string()),
        Err(FocusDevError::Spec(_))
    ));
    let mut v: serde_json::Value = serde_json::from_str(BASIC).unwrap();
    v["rows"][0]["guards"] = json!({});
    assert!(FocusSpec::from_json(&v.to_string()).is_err());
    let mut v: serde_json::Value = serde_json::from_str(BASIC).unwrap();
    v["format_version"] = json!(2);
    assert!(FocusSpec::from_json(&v.to_string()).is_err());
}

#[test]
fn guards_become_the_loaders_columns_and_a_wrong_value_type_is_refused() {
    let mut v: serde_json::Value = serde_json::from_str(BASIC).unwrap();
    v["rows"][1]["guard"] = json!({"var": "intake_note", "op": "in", "value": ["a", "b"]});
    v["rows"][2]["guard"] = json!({"var": "intake_note", "op": "eq", "value": "x"});
    let seed = FocusSpec::from_json(&v.to_string())
        .unwrap()
        .to_seed(&empty_presets())
        .unwrap();
    assert_eq!(
        seed.compositions[1].guard_value.as_deref(),
        Some(r#"["a","b"]"#)
    );
    assert_eq!(seed.compositions[2].guard_value.as_deref(), Some("x"));
    for (op, value) in [
        ("eq", json!(["a"])),
        ("in", json!("a")),
        ("present", json!("a")),
        ("zz", json!("a")),
    ] {
        let mut v: serde_json::Value = serde_json::from_str(BASIC).unwrap();
        v["rows"][1]["guard"] = json!({"var": "intake_note", "op": op, "value": value});
        let r = FocusSpec::from_json(&v.to_string())
            .unwrap()
            .to_seed(&empty_presets());
        assert!(matches!(r, Err(FocusDevError::Spec(_))), "{op}");
    }
}

// -- shape --------------------------------------------------------------------------------

fn basic_seed() -> store::FocusSeed {
    FocusSpec::from_json(BASIC)
        .unwrap()
        .to_seed(&empty_presets())
        .unwrap()
}

fn problems_after(edit: impl FnOnce(&mut store::FocusSeed)) -> Vec<String> {
    let mut s = basic_seed();
    edit(&mut s);
    shape::problems(&s)
}

#[test]
fn every_shape_rule_has_a_failing_case() {
    let cases: Vec<(&str, Vec<String>)> = vec![
        (
            "bad id",
            problems_after(|s| s.focus.focus_id = "Bad_Id".into()),
        ),
        (
            "tier",
            problems_after(|s| s.focus.max_routing_tier = "open".into()),
        ),
        ("status", problems_after(|s| s.focus.status = "live".into())),
        (
            "posture",
            problems_after(|s| s.focus.collection_posture = "x".into()),
        ),
        (
            "one-liner",
            problems_after(|s| s.ui_one_liner = Some("x".repeat(81))),
        ),
        ("no rows", problems_after(|s| s.compositions.clear())),
        (
            "duplicate action",
            problems_after(|s| s.compositions[1].action_id = "intake".into()),
        ),
        (
            "action id",
            problems_after(|s| s.compositions[0].action_id = "Bad-Id".into()),
        ),
        (
            "unknown key",
            problems_after(|s| {
                s.compositions[0].customization = r#"{"task_typ":"general"}"#.into()
            }),
        ),
        (
            "not object",
            problems_after(|s| s.compositions[0].customization = "[]".into()),
        ),
        (
            "half guard",
            problems_after(|s| s.compositions[1].guard_op = None),
        ),
        (
            "bad op",
            problems_after(|s| s.compositions[1].guard_op = Some("zz".into())),
        ),
        (
            "clauses",
            problems_after(|s| s.compositions[1].guard_clauses = Some("[]".into())),
        ),
        (
            "store orphan",
            problems_after(|s| s.compositions[0].store_target = Some("t".into())),
        ),
        (
            "trigger",
            problems_after(|s| s.compositions[0].schedule_trigger_offset = Some("1d".into())),
        ),
        (
            "slot",
            problems_after(|s| s.questions[0].slot = "other".into()),
        ),
        ("kind", problems_after(|s| s.questions[0].kind = "x".into())),
        (
            "default",
            problems_after(|s| s.questions[0].default_option = "zz".into()),
        ),
        (
            "no options",
            problems_after(|s| s.questions[0].options.clear()),
        ),
        (
            "dup option",
            problems_after(|s| {
                let o = s.questions[0].options[0].clone();
                s.questions[0].options.push(o)
            }),
        ),
        (
            "envelope on choice",
            problems_after(|s| s.questions[0].options[0].envelope = Some("{}".into())),
        ),
        (
            "mode slot",
            problems_after(|s| s.questions[0].kind = "mode".into()),
        ),
    ];
    for (name, p) in cases {
        assert!(!p.is_empty(), "{name}: no problem reported");
    }
    assert!(shape::problems(&basic_seed()).is_empty());
}

#[test]
fn a_block_row_is_not_checked_against_the_ordinary_step_keys() {
    let p = problems_after(|s| {
        s.compositions[0].block_stable_id = Some("cb-06".into());
        s.compositions[0].customization = r#"{"any_key":1}"#.into();
    });
    assert!(p.is_empty(), "{p:?}");
}

#[test]
fn the_ordinary_step_key_list_equals_the_loaders_struct_fields() {
    let src = include_str!("../lifecycle.rs");
    let start = src.find("struct RawStepFromDb {").expect("struct");
    let body = &src[start..src[start..].find("\n}").map(|e| start + e).unwrap()];
    let mut fields: Vec<&str> = body
        .lines()
        .skip(1)
        .filter_map(|l| {
            let l = l.trim();
            if l.starts_with("//") || l.is_empty() {
                return None;
            }
            l.split(':').next().filter(|n| !n.contains(' '))
        })
        .collect();
    fields.sort_unstable();
    let mut listed: Vec<&str> = crate::conductor::lifecycle::ORDINARY_STEP_KEYS.to_vec();
    listed.sort_unstable();
    assert_eq!(fields, listed);
}

// -- presets ------------------------------------------------------------------------------

#[test]
fn a_use_expands_to_ordinary_rows_with_the_prefix_applied_everywhere() {
    let rows = preset::expand(&fixture_presets(), &use_of(ok_params())).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].action_id, "a_review");
    assert_eq!(rows[0].config["output_var"], "a_review_out");
    assert_eq!(
        rows[0].config["prompt_template"],
        "Review {brief} in a plain tone, about 100 words."
    );
    assert_eq!(rows[1].action_id, "a_finish");
    assert_eq!(rows[1].guard.as_ref().unwrap().var, "a_review_out");
    assert_eq!(rows[1].config["output_var"], "a_final_out");
    assert_eq!(rows[1].config["prompt_template"], "Finish {a_review_out}");
}

#[test]
fn expansion_is_deterministic_and_two_uses_do_not_collide() {
    let f = fixture_presets();
    assert_eq!(
        preset::expand(&f, &use_of(ok_params())).unwrap(),
        preset::expand(&f, &use_of(ok_params())).unwrap()
    );
    let spec = FocusSpec::from_json(WITH_PRESET).unwrap();
    let rows = spec.expanded_rows(&f).unwrap();
    let ids: Vec<&str> = rows.iter().map(|r| r.action_id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "intake",
            "first_review",
            "first_finish",
            "second_review",
            "second_finish"
        ]
    );
    let seed = spec.to_seed(&f).unwrap();
    shape::check(&seed).unwrap();
    let outs: Vec<String> = seed
        .compositions
        .iter()
        .map(|c| {
            serde_json::from_str::<serde_json::Value>(&c.customization).unwrap()["output_var"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    let mut dedup = outs.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(
        outs.len(),
        dedup.len(),
        "output variables are unique: {outs:?}"
    );
}

#[test]
fn a_use_with_a_missing_extra_or_ill_typed_param_is_refused() {
    let f = fixture_presets();
    let cases = vec![
        json!({"tone": "plain", "words": 100}),
        json!({"source": "brief", "tone": "plain", "words": 100, "extra": 1}),
        json!({"source": "Not A Slug", "tone": "plain", "words": 100}),
        json!({"source": "brief", "tone": "loud", "words": 100}),
        json!({"source": "brief", "tone": "plain", "words": 49}),
        json!({"source": "brief", "tone": "plain", "words": 501}),
        json!({"source": "brief", "tone": "plain", "words": "100"}),
        json!({"source": "{x}", "tone": "plain", "words": 100}),
        json!({"source": "review_out", "tone": "plain", "words": 100}),
    ];
    for (i, p) in cases.into_iter().enumerate() {
        assert!(
            matches!(
                preset::expand(&f, &use_of(p)),
                Err(FocusDevError::Preset(_))
            ),
            "case {i}"
        );
    }
    let mut u = use_of(ok_params());
    u.preset = "nope".into();
    assert!(preset::expand(&f, &u).is_err());
    let mut u = use_of(ok_params());
    u.prefix = "Bad Prefix".into();
    assert!(preset::expand(&f, &u).is_err());
}

fn preset_text_with(edit: impl FnOnce(&mut serde_json::Value)) -> String {
    let mut v: serde_json::Value = serde_json::from_str(PRESETS).unwrap();
    edit(&mut v["presets"][0]);
    v.to_string()
}

#[test]
fn a_malformed_preset_file_is_refused() {
    let cases: Vec<String> = vec![
        preset_text_with(|p| p["rows"][0]["config"]["prompt_template"] = json!("Review %nope%")),
        preset_text_with(|p| {
            p["params"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name": "unused", "type": "slug"}))
        }),
        preset_text_with(|p| p["params"][1]["values"] = json!([])),
        preset_text_with(|p| {
            p["params"][2] = json!({"name": "words", "type": "int", "min": 5, "max": 1})
        }),
        preset_text_with(|p| p["locals"] = json!(["source"])),
        preset_text_with(|p| p["id"] = json!("Bad Id")),
        preset_text_with(|p| p["rows"] = json!([])),
        preset_text_with(|p| p["extra"] = json!(1)),
        preset_text_with(|p| p["rows"][0]["bogus"] = json!(1)),
        r#"{"format_version": 2, "presets": []}"#.to_owned(),
    ];
    for (i, t) in cases.iter().enumerate() {
        assert!(preset::parse(t).is_err(), "case {i}");
    }
}

/// The preset admission check: every preset of `file` expands for every combination of
/// its params' extreme values and each result passes the shape rules and parses back.
fn admit_every_preset(file: &PresetFile) -> usize {
    let mut checked = 0;
    for p in &file.presets {
        let mut combos: Vec<serde_json::Map<String, serde_json::Value>> = vec![Default::default()];
        for d in &p.params {
            let options: Vec<serde_json::Value> = match &d.kind {
                ParamKind::Slug => vec![json!(format!("in_{}", d.name))],
                ParamKind::Enum { values } => values.iter().map(|v| json!(v)).collect(),
                ParamKind::Int { min, max } => vec![json!(min), json!(max)],
            };
            combos = combos
                .into_iter()
                .flat_map(|c| {
                    options.iter().map(move |o| {
                        let mut c = c.clone();
                        c.insert(d.name.clone(), o.clone());
                        c
                    })
                })
                .collect();
        }
        for params in combos {
            let u: PresetUse =
                serde_json::from_value(json!({"preset": p.id, "as": "x", "params": params}))
                    .unwrap();
            let rows = preset::expand(file, &u).expect("expands");
            let spec = FocusSpec {
                format_version: 1,
                focus: serde_json::from_value(json!({
                    "focus_id": "admit", "display_name": "A", "max_routing_tier": "local_only",
                    "status": "designed"}))
                .unwrap(),
                rows: rows
                    .into_iter()
                    .map(|r| super::spec::SpecEntry::Row(Box::new(r)))
                    .collect(),
                launch: vec![],
                ui_one_liner: None,
            };
            shape::check(&spec.to_seed(&empty_presets()).unwrap()).expect("shape");
            checked += 1;
        }
    }
    checked
}

#[test]
fn every_shipped_preset_expands_for_every_param_combination() {
    // The shipped list is empty until items.id=918 adds the first preset, so this is
    // vacuous today. The same function over the fixture file below proves it bites.
    let shipped = preset::shipped().expect("shipped file parses");
    assert_eq!(shipped.format_version, 1);
    admit_every_preset(shipped);
}

#[test]
fn the_admission_check_bites_on_a_real_preset() {
    // 1 slug x 2 enum x 2 int endpoints.
    assert_eq!(admit_every_preset(&fixture_presets()), 4);
}

// -- the writer and the loader ------------------------------------------------------------

async fn load_through_the_real_loader(
    spec: &FocusSpec,
    presets: &PresetFile,
) -> Result<(), String> {
    let _lock = crate::test_support::ENV_MUTEX.lock().await;
    let saved = std::env::var("QR_DATA_ROOT").ok();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("QR_DATA_ROOT", dir.path());
    crate::persistence::migrations::migrate_shared_db()
        .await
        .unwrap();
    let pool =
        sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
            &crate::providers::utils::db_path_shared(),
        ))
        .await
        .unwrap();
    let seed = spec.to_seed(presets).unwrap();
    let mut conn = pool.acquire().await.unwrap();
    store::write_focus(&mut conn, &seed).await.unwrap();
    drop(conn);
    let r = crate::conductor::lifecycle::load_focus_definition_from_db(&pool, &seed.focus.focus_id)
        .await
        .map(|d| assert!(d.is_some()))
        .map_err(|e| e.to_string());
    pool.close().await;
    match saved {
        Some(v) => std::env::set_var("QR_DATA_ROOT", v),
        None => std::env::remove_var("QR_DATA_ROOT"),
    }
    r
}

#[tokio::test]
async fn a_spec_written_by_the_one_writer_loads_through_the_real_loader() {
    let spec = FocusSpec::from_json(BASIC).unwrap();
    load_through_the_real_loader(&spec, &empty_presets())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_spec_with_two_preset_uses_loads_through_the_real_loader() {
    let spec = FocusSpec::from_json(WITH_PRESET).unwrap();
    load_through_the_real_loader(&spec, &fixture_presets())
        .await
        .unwrap();
}

#[tokio::test]
async fn delete_focus_removes_every_row_the_writer_wrote() {
    let _lock = crate::test_support::ENV_MUTEX.lock().await;
    let saved = std::env::var("QR_DATA_ROOT").ok();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("QR_DATA_ROOT", dir.path());
    crate::persistence::migrations::migrate_shared_db()
        .await
        .unwrap();
    let pool =
        sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
            &crate::providers::utils::db_path_shared(),
        ))
        .await
        .unwrap();
    let seed = basic_seed();
    let mut conn = pool.acquire().await.unwrap();
    store::write_focus(&mut conn, &seed).await.unwrap();
    store::delete_focus(&mut conn, "fixture-focus")
        .await
        .unwrap();
    for table in [
        "focuses",
        "focus_block_compositions",
        "focus_launch_questions",
        "focus_launch_options",
        "focus_launch_display",
    ] {
        let n: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {table} WHERE focus_id = 'fixture-focus'"
        ))
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        assert_eq!(n, 0, "{table}");
    }
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM focus_block_customization WHERE composition_id LIKE 'fixture-focus:%'")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(n, 0);
    // And the Focus can be written again.
    store::write_focus(&mut conn, &seed).await.unwrap();
    drop(conn);
    pool.close().await;
    match saved {
        Some(v) => std::env::set_var("QR_DATA_ROOT", v),
        None => std::env::remove_var("QR_DATA_ROOT"),
    }
}
