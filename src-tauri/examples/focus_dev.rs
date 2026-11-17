// src-tauri/examples/focus_dev.rs
//
// DEV-ONLY (items.id=898, decisions.id=951). Arguments and printing only: every rule lives in
// the library (quietrabbit_lib::conductor::focus_dev), where CI tests it. Never part of the
// app bundle, never built by `cargo tauri build`, run by hand on a developer machine, and
// built in a DEBUG build only (the library compiles the writers and the dev-root guard only
// under debug_assertions or test, so a release build of this example is a stub).
//
// Usage, from src-tauri/:
//   cargo run --example focus_dev -- validate <spec.json> [--presets <file>]
//   cargo run --example focus_dev -- reseed --mark-dev-root <dir>
//   cargo run --example focus_dev -- reseed <spec.json> --data-root <dir> [--apply] [--presets <file>]
//   cargo run --example focus_dev -- emit-migration <spec.json> [--number N] [--baseline]
//                                    [--out <file>] [--presets <file>]
//   cargo run --example focus_dev -- expand-preset <preset_id> --as <prefix>
//                                    [--param key=value]... [--presets <file>]
//
// reseed writes ONLY <dir>/instance/shared.db of a directory marked with --mark-dev-root, and
// dry-runs unless --apply is given. emit-migration prints to stdout unless --out names a
// file, and never edits migrations.rs: it prints the registration it expects.
// The schema directory is the source tree's (CARGO_MANIFEST_DIR at compile time): a dev-only
// path, like find_focus_file's (items.id=617).
// No network. No encrypted database. No user data.

#[cfg(not(debug_assertions))]
fn main() {
    eprintln!("focus_dev is built in debug builds only");
    std::process::exit(2);
}

#[cfg(debug_assertions)]
use std::path::{Path, PathBuf};

#[cfg(debug_assertions)]
use quietrabbit_lib::conductor::focus_dev::{devroot, preset, render, spec, tool, FocusDevError};

#[cfg(debug_assertions)]
struct Args {
    positional: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

#[cfg(debug_assertions)]
impl Args {
    fn parse(raw: &[String]) -> Args {
        let mut positional = Vec::new();
        let mut flags = Vec::new();
        let mut i = 0;
        while i < raw.len() {
            let a = &raw[i];
            if let Some(name) = a.strip_prefix("--") {
                let takes_value = !matches!(name, "apply" | "baseline");
                if takes_value && i + 1 < raw.len() {
                    flags.push((name.to_owned(), Some(raw[i + 1].clone())));
                    i += 2;
                    continue;
                }
                flags.push((name.to_owned(), None));
            } else {
                positional.push(a.clone());
            }
            i += 1;
        }
        Args { positional, flags }
    }
    fn flag(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| v.as_deref())
    }
    fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(n, _)| n == name)
    }
    fn all(&self, name: &str) -> Vec<&str> {
        self.flags
            .iter()
            .filter(|(n, _)| n == name)
            .filter_map(|(_, v)| v.as_deref())
            .collect()
    }
}

#[cfg(debug_assertions)]
fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("focus_dev: {msg}");
    std::process::exit(1);
}

#[cfg(debug_assertions)]
fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| die(format!("cannot read {path}: {e}")))
}

#[cfg(debug_assertions)]
fn presets(a: &Args) -> preset::PresetFile {
    match a.flag("presets") {
        Some(p) => preset::parse(&read(p)).unwrap_or_else(|e| die(e)),
        None => preset::shipped().unwrap_or_else(|e| die(e)).clone(),
    }
}

#[cfg(debug_assertions)]
fn load_spec(
    a: &Args,
) -> (
    String,
    spec::FocusSpec,
    quietrabbit_lib::persistence::focus_composition_store::FocusSeed,
) {
    let path = a
        .positional
        .get(1)
        .unwrap_or_else(|| die("a spec path is needed"));
    let text = read(path);
    let s = spec::FocusSpec::from_json(&text).unwrap_or_else(|e| die(e));
    let seed = s.to_seed(&presets(a)).unwrap_or_else(|e| die(e));
    (text, s, seed)
}

#[cfg(debug_assertions)]
fn print_rows(rows: &[(String, String)]) {
    for (id, kind) in rows {
        println!("  {id}  ({kind})");
    }
}

#[cfg(debug_assertions)]
#[tokio::main]
async fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let a = Args::parse(&raw);
    match a.positional.first().map(String::as_str) {
        Some("validate") => {
            let (_, _, seed) = load_spec(&a);
            match tool::validate_seed(&seed).await {
                Ok(r) => {
                    println!(
                        "{}: valid, {} rows through the real loader",
                        r.focus_id,
                        r.rows.len()
                    );
                    print_rows(&r.rows);
                }
                Err(e) => die(e),
            }
        }
        Some("reseed") => {
            let reals = devroot::real_data_dirs_from_env();
            if let Some(dir) = a.flag("mark-dev-root") {
                let d = devroot::mark_dev_root(Path::new(dir), &reals).unwrap_or_else(|e| die(e));
                println!("marked {} as a development root", d.root().display());
                return;
            }
            let dir = a
                .flag("data-root")
                .unwrap_or_else(|| die("--data-root <dir> is needed"));
            let dev = devroot::check_dev_root(Path::new(dir), &reals).unwrap_or_else(|e| die(e));
            let (_, _, seed) = load_spec(&a);
            let mode = if a.has("apply") {
                tool::ReseedMode::Apply
            } else {
                tool::ReseedMode::DryRun
            };
            let r = tool::reseed(&dev, &seed, mode)
                .await
                .unwrap_or_else(|e: FocusDevError| die(e));
            println!(
                "{}: {} ({})",
                r.focus_id,
                if r.mode_applied {
                    "APPLIED"
                } else {
                    "dry run, nothing written"
                },
                if r.database_exists {
                    "database exists"
                } else {
                    "no database yet"
                }
            );
            println!("  rows now: {:?}", r.old_actions);
            println!("  rows after: {:?}", r.new_actions);
            print_rows(&r.rows);
        }
        Some("emit-migration") => {
            let (text, _, seed) = load_spec(&a);
            let sha = render::spec_sha256(&text);
            let kind = if a.has("baseline") {
                render::EmitKind::Baseline
            } else {
                let schema: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("schema");
                let on_disk = tool::schema_dir_versions(&schema).unwrap_or_else(|e| die(e));
                let requested = a.flag("number").map(|n| {
                    n.parse::<u32>()
                        .unwrap_or_else(|_| die("--number must be a number"))
                });
                let n = render::next_shared_number(
                    &tool::registered_shared_versions(),
                    &on_disk,
                    requested,
                )
                .unwrap_or_else(|e| die(e));
                render::EmitKind::Migration { number: n }
            };
            let sql = render::render(&seed, &sha, kind).unwrap_or_else(|e| die(e));
            match a.flag("out") {
                Some(path) => {
                    std::fs::write(path, &sql)
                        .unwrap_or_else(|e| die(format!("cannot write {path}: {e}")));
                    eprintln!("wrote {path}");
                }
                None => print!("{sql}"),
            }
            if let render::EmitKind::Migration { number } = kind {
                eprintln!(
                    "register it by hand: SchemaFile {{ prefix: \"shared\", version: {number}, sql: include_str!(\"../../schema/shared_{number:03}.sql\") }} in migrations.rs SCHEMA_FILES, then re-pin the shared version tests"
                );
            }
        }
        Some("expand-preset") => {
            let id = a
                .positional
                .get(1)
                .unwrap_or_else(|| die("a preset id is needed"));
            let prefix = a
                .flag("as")
                .unwrap_or_else(|| die("--as <prefix> is needed"));
            let mut params = std::collections::BTreeMap::new();
            for kv in a.all("param") {
                let (k, v) = kv
                    .split_once('=')
                    .unwrap_or_else(|| die("--param needs key=value"));
                let value = v
                    .parse::<i64>()
                    .map(serde_json::Value::from)
                    .unwrap_or_else(|_| serde_json::Value::String(v.to_owned()));
                params.insert(k.to_owned(), value);
            }
            let u = spec::PresetUse {
                preset: id.clone(),
                prefix: prefix.to_owned(),
                params,
            };
            let rows = preset::expand(&presets(&a), &u).unwrap_or_else(|e| die(e));
            println!(
                "{}",
                serde_json::to_string_pretty(&rows).unwrap_or_else(|e| die(e))
            );
        }
        _ => die("commands: validate, reseed, emit-migration, expand-preset (see the file header)"),
    }
}
