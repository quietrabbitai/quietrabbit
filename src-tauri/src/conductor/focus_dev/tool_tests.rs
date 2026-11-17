// Stage 3 tests for focus_dev (items.id=898): renderer, equivalence, dev-root guard,
// validate and reseed, drift, the app-code scan and the gating statement.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::*;
use crate::conductor::focus_dev::devroot::{self, check_dev_root, mark_dev_root};
use crate::conductor::focus_dev::drift;
use crate::conductor::focus_dev::preset::PresetFile;
use crate::conductor::focus_dev::render::{self, EmitKind};
use crate::conductor::focus_dev::spec::FocusSpec;
use crate::persistence::migrations::parse_statements;

const BASIC: &str = include_str!("../../../tests/fixtures/focus_dev/spec_basic.json");

fn no_presets() -> PresetFile {
    PresetFile {
        format_version: 1,
        presets: Vec::new(),
    }
}

fn seed_of(text: &str) -> store::FocusSeed {
    FocusSpec::from_json(text)
        .unwrap()
        .to_seed(&no_presets())
        .unwrap()
}

fn basic_seed() -> store::FocusSeed {
    seed_of(BASIC)
}

/// The basic spec with every awkward string the runner could misread.
fn tricky_seed() -> store::FocusSeed {
    let mut s = basic_seed();
    s.compositions[0].customization = json!({
        "task_type": "quick_response", "routing_tier": 1, "output_var": "intake_note",
        "prompt_template": "Line one\n\nafter a blank line\n-- a dash line\nend\nCreate trigger x\nIt's \"quoted\" é ü 100%\n"
    })
    .to_string();
    s.compositions[1].guard_clauses = Some(
        r#"[{"var":"launch_answer_1","op":"ne","value":"x"},{"var":"intake_note","op":"present"}]"#
            .into(),
    );
    s.compositions[1].schedule_trigger_anchor_field = Some("due".into());
    s.compositions[1].schedule_trigger_offset = Some("-1d".into());
    s.compositions[2].store_op = Some("session_put".into());
    s.compositions[2].store_target = Some("t".into());
    s.compositions[2].store_args = Some(r#"{"value":"plain"}"#.into());
    s.focus.description = "Multi\nline\n\ndescription".into();
    s.focus.high_priority_trigger_anchor_field = Some("start".into());
    s.focus.high_priority_trigger_offset = Some("2h".into());
    s.focus.suggest_in_focuses = vec!["a".into(), "b".into()];
    s.questions.push(store::LaunchQuestionSeed {
        question_id: "how".into(),
        slot: "launch_mode".into(),
        kind: "mode".into(),
        asked: true,
        default_option: "quick".into(),
        prompt: "How?".into(),
        options: vec![store::LaunchOptionSeed {
            option_id: "quick".into(),
            label: "Quick".into(),
            hint: String::new(),
            envelope: Some(r#"{"highest_tier":"local_only"}"#.into()),
        }],
    });
    s
}

async fn scratch_db(dir: &Path) -> (PathBuf, SqlitePool) {
    let db = dir.join("instance").join("shared.db");
    crate::persistence::migrations::migrate_shared_db_at(&db)
        .await
        .unwrap();
    let pool = open_pool(&db).await.unwrap();
    (db, pool)
}

/// Every seeded table, all columns but the two timestamps, as sorted text rows.
async fn dump(pool: &SqlitePool) -> Vec<String> {
    let mut out = Vec::new();
    for t in [
        "focuses",
        "focus_block_compositions",
        "focus_block_customization",
        "focus_launch_questions",
        "focus_launch_options",
        "focus_mode_profiles",
        "focus_launch_display",
    ] {
        let cols: Vec<(String,)> =
            sqlx::query_as(&format!("SELECT name FROM pragma_table_info('{t}')"))
                .fetch_all(pool)
                .await
                .unwrap();
        let exprs: Vec<String> = cols
            .into_iter()
            .map(|c| c.0)
            .filter(|c| c != "created_at" && c != "updated_at")
            .map(|c| format!("quote({c})"))
            .collect();
        let sql = format!("SELECT {} FROM {t} ORDER BY 1", exprs.join(" || '|' || "));
        let rows: Vec<(String,)> = sqlx::query_as(&sql).fetch_all(pool).await.unwrap();
        out.extend(rows.into_iter().map(|r| format!("{t}: {}", r.0)));
    }
    out.sort();
    out
}

async fn apply_sql(pool: &SqlitePool, sql: &str) {
    for stmt in parse_statements(sql) {
        sqlx::query(&stmt)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("{e}: {stmt}"));
    }
}

// -- renderer -----------------------------------------------------------------------------

#[test]
fn a_migration_has_the_header_the_statements_and_the_version_row() {
    let sha = render::spec_sha256(BASIC);
    let sql = render::render(&basic_seed(), &sha, EmitKind::Migration { number: 900 }).unwrap();
    assert!(sql.contains(&render::header_line("fixture-focus", &sha)));
    assert!(sql.contains("INSERT OR IGNORE INTO schema_version"));
    assert!(sql.contains("VALUES (900,"));
    assert!(!sql.contains("INSERT OR IGNORE INTO focuses"));
    for l in sql.lines().filter(|l| l.starts_with("--")) {
        assert!(!l.contains(';'), "a header comment holds a semicolon: {l}");
    }
    let base = render::render(&basic_seed(), &sha, EmitKind::Baseline).unwrap();
    assert!(!base.contains("INSERT OR IGNORE INTO schema_version"));
    assert_eq!(
        sql,
        render::render(&basic_seed(), &sha, EmitKind::Migration { number: 900 }).unwrap()
    );
}

#[test]
fn a_semicolon_in_any_text_is_refused_naming_the_row() {
    let sha = render::spec_sha256(BASIC);
    let mut s = basic_seed();
    s.compositions[1].customization =
        r#"{"routing_tier":1,"prompt_template":"one; two","output_var":"x"}"#.into();
    let e = render::render(&s, &sha, EmitKind::Baseline).unwrap_err();
    assert!(
        matches!(&e, FocusDevError::Render(m) if m.contains("polish") && m.contains("semicolon")),
        "{e}"
    );
    let mut s = basic_seed();
    s.questions[0].prompt = "Which; one?".into();
    assert!(render::render(&s, &sha, EmitKind::Baseline).is_err());
    let mut s = basic_seed();
    s.focus.display_name = "A;B".into();
    assert!(render::render(&s, &sha, EmitKind::Baseline).is_err());
    let mut s = basic_seed();
    s.compositions[0].customization = "{\"a\":\"x\ry\"}".into();
    assert!(render::render(&s, &sha, EmitKind::Baseline).is_err());
}

#[test]
fn the_next_number_needs_the_registry_and_the_directory_to_agree() {
    use render::next_shared_number as next;
    assert_eq!(next(&[1, 2, 3], &[3, 2, 1], None).unwrap(), 4);
    assert_eq!(next(&[1, 2, 3], &[1, 2, 3], Some(9)).unwrap(), 9);
    assert!(next(&[1, 2, 3], &[1, 2, 3], Some(3)).is_err());
    assert!(next(&[1, 2, 3], &[1, 2], None).is_err());
    assert!(next(&[1, 2], &[1, 2, 3], None).is_err());
}

#[test]
fn the_registry_and_the_schema_directory_agree_today() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("schema");
    let on_disk = schema_dir_versions(&dir).unwrap();
    let next = render::next_shared_number(&registered_shared_versions(), &on_disk, None).unwrap();
    assert!(next > 47, "{next}");
}

// -- the A versus B equivalence ----------------------------------------------------------

#[tokio::test]
async fn the_emitted_sql_makes_the_same_rows_as_the_one_writer() {
    for seed in [basic_seed(), tricky_seed()] {
        let sha = render::spec_sha256(BASIC);
        // A: the writer.
        let da = ScratchDir::new().unwrap();
        let (_, a) = scratch_db(da.path()).await;
        let mut conn = a.acquire().await.unwrap();
        store::write_focus(&mut conn, &seed).await.unwrap();
        drop(conn);
        // B: the emitted file, as a migration and as a baseline fragment.
        for kind in [EmitKind::Migration { number: 900 }, EmitKind::Baseline] {
            let db = ScratchDir::new().unwrap();
            let (_, b) = scratch_db(db.path()).await;
            let sql = render::render(&seed, &sha, kind).unwrap();
            apply_sql(&b, &sql).await;
            assert_eq!(dump(&a).await, dump(&b).await, "{kind:?}");
            // Running it again repairs rather than duplicates.
            apply_sql(&b, &sql).await;
            assert_eq!(dump(&a).await, dump(&b).await, "{kind:?} twice");
            if matches!(kind, EmitKind::Migration { .. }) {
                let n: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM schema_version WHERE version = 900")
                        .fetch_one(&b)
                        .await
                        .unwrap();
                assert_eq!(n, 1);
            }
            b.close().await;
        }
        a.close().await;
    }
}

#[tokio::test]
async fn an_emit_repairs_a_stale_row_that_insert_or_ignore_would_keep() {
    let sha = render::spec_sha256(BASIC);
    let d = ScratchDir::new().unwrap();
    let (_, pool) = scratch_db(d.path()).await;
    let mut old = basic_seed();
    old.focus.display_name = "Old name".into();
    let mut conn = pool.acquire().await.unwrap();
    store::write_focus(&mut conn, &old).await.unwrap();
    drop(conn);
    apply_sql(
        &pool,
        &render::render(&basic_seed(), &sha, EmitKind::Baseline).unwrap(),
    )
    .await;
    let name: String =
        sqlx::query_scalar("SELECT display_name FROM focuses WHERE focus_id = 'fixture-focus'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(name, "Fixture Focus");
    pool.close().await;
}

// -- dev-root guard ------------------------------------------------------------------------

fn layout() -> (tempfile::TempDir, PathBuf, Vec<PathBuf>) {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    let real = home.join(".local").join("share").join("ai.quietrabbit.app");
    std::fs::create_dir_all(&real).unwrap();
    let dev = t.path().join("dev");
    std::fs::create_dir_all(&dev).unwrap();
    let reals = devroot::real_data_dirs(Some(home.to_str().unwrap()), None);
    (t, dev, reals)
}

#[test]
fn the_real_directories_are_computed_from_home_and_xdg() {
    let d = devroot::real_data_dirs(Some("/h"), Some("/x"));
    assert!(d.contains(&PathBuf::from("/x/ai.quietrabbit.app")));
    assert!(d.contains(&PathBuf::from("/h/.local/share/ai.quietrabbit.app")));
    assert!(d.contains(&PathBuf::from("/h/.local/share/quietrabbit")));
    assert!(devroot::real_data_dirs(None, None).is_empty());
}

#[test]
fn the_guard_refuses_everything_it_cannot_show_is_a_development_copy() {
    let (t, dev, reals) = layout();
    // No marker.
    assert!(check_dev_root(&dev, &reals).is_err());
    // Nothing known about the real directories.
    assert!(mark_dev_root(&dev, &[]).is_err());
    // The real directory itself, inside it, and a directory that contains it.
    let real = reals[0].clone();
    std::fs::create_dir_all(real.join("instance")).unwrap();
    std::fs::write(real.join("instance").join("shared.db"), b"x").unwrap();
    assert!(mark_dev_root(&real, &reals).is_err());
    let inside = real.join("copy");
    std::fs::create_dir_all(&inside).unwrap();
    assert!(mark_dev_root(&inside, &reals).is_err());
    assert!(mark_dev_root(t.path().join("home").as_path(), &reals).is_err());
    // Not a directory, missing.
    assert!(mark_dev_root(&t.path().join("nope"), &reals).is_err());
    // A non-empty directory with no shared.db is refused even with a marker request.
    let junk = t.path().join("junk");
    std::fs::create_dir_all(&junk).unwrap();
    std::fs::write(junk.join("users"), b"x").unwrap();
    assert!(mark_dev_root(&junk, &reals).is_err());
    // The good case, and a marker is bound to the path.
    let ok = mark_dev_root(&dev, &reals).unwrap();
    assert!(check_dev_root(ok.root(), &reals).is_ok());
    let moved = t.path().join("moved");
    std::fs::rename(&dev, &moved).unwrap();
    let e = check_dev_root(&moved, &reals).unwrap_err().to_string();
    assert!(e.contains("copied or moved"), "{e}");
    // A garbled marker.
    std::fs::write(moved.join(devroot::MARKER_FILE), b"not json").unwrap();
    assert!(check_dev_root(&moved, &reals).is_err());
}

#[test]
fn a_dev_root_with_an_existing_database_and_users_is_accepted_once_marked() {
    let (_t, dev, reals) = layout();
    std::fs::create_dir_all(dev.join("instance")).unwrap();
    std::fs::write(dev.join("instance").join("shared.db"), b"x").unwrap();
    std::fs::create_dir_all(dev.join("users")).unwrap();
    assert!(check_dev_root(&dev, &reals).is_err());
    mark_dev_root(&dev, &reals).unwrap();
    assert!(check_dev_root(&dev, &reals).is_ok());
}

#[test]
fn a_symlinked_instance_directory_or_database_is_refused() {
    use std::os::unix::fs::symlink;
    // A symlinked instance/ directory, at marking and at check.
    let (t, dev, reals) = layout();
    let elsewhere = t.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("shared.db"), b"x").unwrap();
    symlink(&elsewhere, dev.join("instance")).unwrap();
    assert!(mark_dev_root(&dev, &reals).is_err());
    // A symlinked shared.db.
    let (t2, dev2, reals2) = layout();
    std::fs::create_dir_all(dev2.join("instance")).unwrap();
    let target = t2.path().join("real.db");
    std::fs::write(&target, b"x").unwrap();
    symlink(&target, dev2.join("instance").join("shared.db")).unwrap();
    assert!(mark_dev_root(&dev2, &reals2).is_err());
    // A symlink into the real data directory, even if the dev root looked fine when marked.
    let (_t3, dev3, reals3) = layout();
    let ok = mark_dev_root(&dev3, &reals3).unwrap();
    std::fs::create_dir_all(&reals3[0]).ok();
    std::fs::write(reals3[0].join("shared.db"), b"x").unwrap();
    std::fs::create_dir_all(dev3.join("instance")).unwrap();
    symlink(
        reals3[0].join("shared.db"),
        dev3.join("instance").join("shared.db"),
    )
    .unwrap();
    assert!(check_dev_root(&dev3, &reals3).is_err());
    assert!(
        ok.shared_db_path().is_err(),
        "the token re-verifies on every use"
    );
}

#[tokio::test]
async fn a_link_swapped_in_after_the_check_stops_reseed_before_the_database_is_opened() {
    use std::os::unix::fs::symlink;
    let (t, dev_dir, reals) = layout();
    let dev = marked(&reals, &dev_dir);
    // Swap instance/ for a link to a directory that holds a database.
    let elsewhere = t.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    symlink(&elsewhere, dev_dir.join("instance")).unwrap();
    for mode in [ReseedMode::DryRun, ReseedMode::Apply] {
        let e = reseed(&dev, &basic_seed(), mode).await.unwrap_err();
        assert!(matches!(e, FocusDevError::DevRoot(_)), "{e}");
    }
    assert!(
        !elsewhere.join("shared.db").exists(),
        "nothing was created through the link"
    );
}

#[test]
fn the_scratch_directory_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let d = ScratchDir::new().unwrap();
    let mode = std::fs::metadata(d.path()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    let p = d.path().to_path_buf();
    drop(d);
    assert!(!p.exists());
}

// -- validate and reseed ------------------------------------------------------------------

/// The basic spec plus one more unguarded step.
fn v2_seed() -> store::FocusSeed {
    let mut s = basic_seed();
    s.compositions.push(store::CompositionSeed::plain(
        "extra",
        None,
        r#"{"task_type":"general","routing_tier":1,"output_var":"extra_out","prompt_template":"Extra: {user_input}"}"#,
    ));
    s
}

fn marked(reals: &[PathBuf], dev: &Path) -> DevRoot {
    mark_dev_root(dev, reals).unwrap()
}

async fn actions_in(dev: &DevRoot, focus: &str) -> Vec<String> {
    let pool = open_pool(&dev.shared_db_path().unwrap()).await.unwrap();
    let r = current_actions(&pool, focus).await;
    pool.close().await;
    r
}

#[tokio::test]
async fn validate_runs_the_real_loader_and_reports_the_rows() {
    let r = validate_seed(&basic_seed()).await.unwrap();
    assert_eq!(r.focus_id, "fixture-focus");
    assert_eq!(r.rows.len(), 3);
    assert!(r.rows.iter().all(|(_, k)| k == "ordinary"));
    // A shape problem and a loader problem are both refused.
    let mut bad = basic_seed();
    bad.focus.status = "live".into();
    assert!(matches!(
        validate_seed(&bad).await,
        Err(FocusDevError::Shape(_))
    ));
    // A router with no fallback row is the loader's refusal (not repeated by shape).
    let mut no_fallback = basic_seed();
    no_fallback.compositions.remove(2);
    let e = validate_seed(&no_fallback).await.unwrap_err().to_string();
    assert!(e.contains("fallback"), "{e}");
    let mut bad_type = basic_seed();
    bad_type.compositions[0].customization =
        r#"{"routing_tier":1,"step_type":"generat","output_var":"intake_note"}"#.into();
    let e = validate_seed(&bad_type).await.unwrap_err().to_string();
    assert!(e.contains("unknown step_type"), "{e}");
}

#[tokio::test]
async fn a_dry_run_changes_nothing_and_apply_replaces_the_focus() {
    let (_t, dev_dir, reals) = layout();
    let dev = marked(&reals, &dev_dir);
    // Dry run on a missing database: reports it, creates nothing.
    let r = reseed(&dev, &basic_seed(), ReseedMode::DryRun)
        .await
        .unwrap();
    assert!(!r.database_exists && !r.mode_applied);
    assert!(!dev.shared_db_path().unwrap().exists());
    // Apply creates, migrates and writes.
    let r = reseed(&dev, &basic_seed(), ReseedMode::Apply)
        .await
        .unwrap();
    assert!(r.mode_applied && r.old_actions.is_empty());
    assert_eq!(
        actions_in(&dev, "fixture-focus").await,
        ["intake", "polish", "plain_reply"]
    );
    // A dry run of a changed spec reports old and new and changes nothing.
    let v2 = v2_seed();
    let r = reseed(&dev, &v2, ReseedMode::DryRun).await.unwrap();
    assert_eq!(r.old_actions, ["intake", "polish", "plain_reply"]);
    assert_eq!(r.new_actions, ["intake", "polish", "plain_reply", "extra"]);
    assert_eq!(actions_in(&dev, "fixture-focus").await.len(), 3);
    // Apply of the changed spec replaces the rows.
    reseed(&dev, &v2, ReseedMode::Apply).await.unwrap();
    assert_eq!(
        actions_in(&dev, "fixture-focus").await,
        ["intake", "polish", "plain_reply", "extra"]
    );
}

#[tokio::test]
async fn an_invalid_spec_never_touches_the_development_database() {
    let (_t, dev_dir, reals) = layout();
    let dev = marked(&reals, &dev_dir);
    reseed(&dev, &basic_seed(), ReseedMode::Apply)
        .await
        .unwrap();
    let before = {
        let p = open_pool(&dev.shared_db_path().unwrap()).await.unwrap();
        let d = dump(&p).await;
        p.close().await;
        d
    };
    let mut bad = basic_seed();
    bad.compositions[0].customization =
        r#"{"routing_tier":1,"task_type":"nonsense","output_var":"intake_note"}"#.into();
    assert!(reseed(&dev, &bad, ReseedMode::Apply).await.is_err());
    let mut bad_shape = basic_seed();
    bad_shape.focus.max_routing_tier = "open".into();
    assert!(reseed(&dev, &bad_shape, ReseedMode::Apply).await.is_err());
    let p = open_pool(&dev.shared_db_path().unwrap()).await.unwrap();
    assert_eq!(dump(&p).await, before);
    p.close().await;
}

#[tokio::test]
async fn a_failure_after_the_write_rolls_the_transaction_back() {
    let (_t, dev_dir, reals) = layout();
    let dev = marked(&reals, &dev_dir);
    reseed(&dev, &basic_seed(), ReseedMode::Apply)
        .await
        .unwrap();
    let before = {
        let p = open_pool(&dev.shared_db_path().unwrap()).await.unwrap();
        let d = dump(&p).await;
        p.close().await;
        d
    };
    let mut v2 = v2_seed();
    v2.focus.display_name = "Changed".into();
    let e = reseed_with(&dev, &v2, ReseedMode::Apply, Hook::FailAfterWrite).await;
    assert!(e.is_err());
    let p = open_pool(&dev.shared_db_path().unwrap()).await.unwrap();
    assert_eq!(dump(&p).await, before, "the previous rows are intact");
    p.close().await;
}

// -- drift ----------------------------------------------------------------------------------

#[test]
fn the_drift_check_passes_for_a_fresh_emit_and_fails_on_a_mismatch() {
    let spec = (
        "focus_specs/fixture-focus.json".to_owned(),
        BASIC.to_owned(),
    );
    let sql = render::render(
        &basic_seed(),
        &render::spec_sha256(BASIC),
        EmitKind::Baseline,
    )
    .unwrap();
    let files = vec![("fixture-focus.baseline.sql".to_owned(), sql)];
    assert!(drift::check(std::slice::from_ref(&spec), &files).is_empty());
    // The spec changes after the emit: a mismatch.
    let edited = BASIC.replace("Fixture Focus", "Fixture Focus 2");
    let p = drift::check(
        &[("focus_specs/fixture-focus.json".to_owned(), edited)],
        &files,
    );
    assert_eq!(p.len(), 1, "{p:?}");
    assert!(p[0].contains("emit-migration"));
    // An emit for another Focus does not satisfy it.
    let other = render::header_line("other-focus", &render::spec_sha256(BASIC));
    let p = drift::check(&[spec], &[("x.sql".to_owned(), other)]);
    assert_eq!(p.len(), 1);
    // No spec, nothing to check.
    assert!(drift::check(&[], &files).is_empty());
}

fn read_all(dir: &Path, suffix: &str) -> Vec<(String, String)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = rd
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(suffix))
        .map(|e| {
            (
                e.path().to_string_lossy().into_owned(),
                std::fs::read_to_string(e.path()).unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

#[test]
fn every_checked_in_focus_spec_has_a_current_emitted_file() {
    // Vacuous until the first spec lands in src-tauri/focus_specs/ (items.id=903, 915).
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let specs: Vec<(String, String)> = read_all(&root.join("focus_specs"), ".json");
    let mut sql = read_all(&root.join("schema"), ".sql");
    sql.extend(read_all(&root.join("focus_specs"), ".baseline.sql"));
    let problems = drift::check(&specs, &sql);
    assert!(problems.is_empty(), "{problems:#?}");
}

// -- scans: nothing in app code names focus_dev, and the gating -----------------------------

fn rust_sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, out: &mut Vec<(String, String)>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push((
                    p.to_string_lossy().into_owned(),
                    std::fs::read_to_string(&p).unwrap(),
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut out);
    out
}

/// True if `line` names the module focus_dev (not focus_device and the like).
fn names_focus_dev(line: &str) -> bool {
    line.match_indices("focus_dev").any(|(i, _)| {
        let after = line[i + "focus_dev".len()..].chars().next();
        let before = line[..i].chars().next_back();
        let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
        !after.is_some_and(ident) && !before.is_some_and(ident)
    })
}

#[test]
fn nothing_in_app_code_names_focus_dev() {
    assert!(names_focus_dev("use crate::conductor::focus_dev::spec;"));
    assert!(!names_focus_dev(
        "async fn a_case_on_another_focus_device_reads()"
    ));
    for (path, text) in rust_sources() {
        if path.contains("conductor/focus_dev/") {
            continue;
        }
        let app_surface = path.ends_with("src/main.rs")
            || path.ends_with("src/ipc.rs")
            || path.ends_with("src/lib.rs")
            || path.contains("/src/commands/");
        for (n, line) in text.lines().enumerate() {
            if !names_focus_dev(line) {
                continue;
            }
            let t = line.trim_start();
            assert!(
                !app_surface,
                "{path}:{} names focus_dev in the app surface (main, lib, ipc, commands)",
                n + 1
            );
            let comment = t.starts_with("//");
            let declaration = path.ends_with("conductor/mod.rs") && t == "pub mod focus_dev;";
            assert!(
                comment || declaration,
                "{path}:{} names focus_dev outside a comment: {t}",
                n + 1
            );
        }
    }
}

#[test]
fn the_database_half_and_every_writer_are_compiled_in_debug_and_test_builds_only() {
    let gate = "#[cfg(any(test, debug_assertions))]";
    let m = include_str!("mod.rs");
    for module in ["pub mod devroot;", "pub mod tool;"] {
        let at = m.find(module).unwrap();
        let before = &m[..at];
        assert!(before.trim_end().ends_with(gate), "{module} is not gated");
    }
    let store_src = include_str!("../../persistence/focus_composition_store.rs");
    for f in [
        "pub async fn write_focus(",
        "pub async fn write_launch(",
        "pub async fn delete_focus(",
    ] {
        let at = store_src.find(f).unwrap();
        assert!(
            store_src[..at].trim_end().ends_with(gate),
            "{f} is not gated"
        );
    }
    let mig = include_str!("../../persistence/migrations.rs");
    for f in [
        "pub(crate) async fn migrate_shared_db_at(",
        "pub(crate) fn shared_schema_versions(",
    ] {
        let at = mig.find(f).unwrap();
        assert!(mig[..at].trim_end().ends_with(gate), "{f} is not gated");
    }
    // The pure half names no database and writes no file.
    for (name, text) in [
        ("spec", include_str!("spec.rs")),
        ("preset", include_str!("preset.rs")),
        ("shape", include_str!("shape.rs")),
        ("render", include_str!("render.rs")),
        ("drift", include_str!("drift.rs")),
    ] {
        for banned in [
            "sqlx::",
            "std::fs",
            "File::",
            "tokio::fs",
            "reqwest",
            "TcpStream",
        ] {
            let code: String = text
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(!code.contains(banned), "{name}.rs names {banned}");
        }
    }
}
