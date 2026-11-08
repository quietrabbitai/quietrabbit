// src-tauri/tests/rig_gate.rs
//
// items.id=860: the debug-only rig hooks (src/rig_hooks.rs: open a Cloud Chat pane and close its
// popup from app arguments, for scratch runs) must not exist in a release build. This test does
// not parse Rust or cfg blocks. It lexes each source file with the same lexer as diag_gate.rs
// (comments and literal contents are separated from code) and checks three structural rules:
//
//   R1  A string literal containing `qr-rig` (the flag prefix) appears only in
//       `rig_hooks.rs`, so no flag text can be compiled in anywhere else.
//   R2  Every `mod rig_hooks` is preceded by `#[cfg(debug_assertions)]`, and there is exactly one.
//       With R1, no flag literal can be compiled into a release build.
//   R3  Every use of `rig_hooks::` outside rig_hooks.rs is preceded by
//       `#[cfg(debug_assertions)]` on the previous code line.
//
// The release result itself is checked by `release_has_no_rig_hooks` in lib.rs (the release test
// executable must not contain the flag text; run by name under --release). The scanner is a pure
// function, so the fixtures below prove it fails on each kind of regression and does not fire on
// comments or look-alikes.

use std::fs;
use std::path::{Path, PathBuf};

#[path = "common/lex.rs"]
mod lex;
use lex::{is_ident, lex, previous_code_line};

const GATE: &str = "#[cfg(debug_assertions)]";
const RIG_FILE: &str = "src/rig_hooks.rs";
const LIB_FILE: &str = "src/lib.rs";
const FLAG_PREFIX: &str = "qr-rig";

fn is_mod_rig(code_line: &str) -> bool {
    let mut t = code_line.trim();
    for vis in ["pub(crate) ", "pub(super) ", "pub "] {
        if let Some(rest) = t.strip_prefix(vis) {
            t = rest;
            break;
        }
    }
    t.strip_prefix("mod rig_hooks")
        .is_some_and(|rest| !rest.chars().next().is_some_and(is_ident))
}

fn mod_rig_count(src: &str) -> usize {
    lex(src).code.iter().filter(|l| is_mod_rig(l)).count()
}

/// Rule violations in one file. `path` uses `/` separators.
fn scan(path: &str, src: &str) -> Vec<String> {
    let is_rig_file = path.ends_with(RIG_FILE);
    let lexed = lex(src);
    let mut found = Vec::new();

    if !is_rig_file {
        for (line, lit) in &lexed.literals {
            if lit.contains(FLAG_PREFIX) {
                found.push(format!(
                    "{path}:{line}: R1 a string literal contains {FLAG_PREFIX} outside {RIG_FILE}"
                ));
            }
        }
    }

    for (idx, code) in lexed.code.iter().enumerate() {
        let line = idx + 1;
        if is_mod_rig(code) && previous_code_line(&lexed.code, idx) != Some(GATE) {
            found.push(format!(
                "{path}:{line}: R2 `mod rig_hooks` must be preceded by {GATE}"
            ));
        }
        if !is_rig_file {
            let calls = code
                .match_indices("rig_hooks::")
                .any(|(pos, _)| !code[..pos].chars().next_back().is_some_and(is_ident));
            if calls && previous_code_line(&lexed.code, idx) != Some(GATE) {
                found.push(format!(
                    "{path}:{line}: R3 a use of rig_hooks must sit on the line after {GATE}"
                ));
            }
        }
    }
    found
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn rel(path: &Path) -> String {
    path.strip_prefix(env!("CARGO_MANIFEST_DIR"))
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

// ---------------------------------------------------------------------------
// The real tree
// ---------------------------------------------------------------------------

#[test]
fn the_crate_has_no_ungated_rig_hooks() {
    let mut files = Vec::new();
    rust_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    // A scanner that silently saw nothing would pass for the wrong reason.
    assert!(files.len() > 50, "only {} source files found", files.len());
    assert!(files.iter().any(|f| rel(f) == RIG_FILE));
    assert!(files.iter().any(|f| rel(f) == LIB_FILE));

    let mut violations = Vec::new();
    let mut mod_rig = 0;
    let mut gated_uses = 0;
    for file in &files {
        let src = fs::read_to_string(file).unwrap();
        violations.extend(scan(&rel(file), &src));
        mod_rig += mod_rig_count(&src);
        if rel(file) != RIG_FILE {
            gated_uses += src.matches("rig_hooks::").count();
        }
    }
    assert_eq!(mod_rig, 1, "expected exactly one `mod rig_hooks`");
    // main.rs has the one call site; if it vanished the hooks would be dead code.
    assert!(gated_uses >= 1, "no call site of rig_hooks found");
    assert!(
        violations.is_empty(),
        "rig hook gate violated (items.id=860):\n{}",
        violations.join("\n")
    );
}

#[test]
fn the_real_rig_module_is_caught_if_it_is_misplaced() {
    let src = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(RIG_FILE)).unwrap();
    assert!(
        !scan("src/somewhere_else.rs", &src).is_empty(),
        "the flag literals in rig_hooks.rs were not detected"
    );
    assert!(scan(RIG_FILE, &src).is_empty());
}

// ---------------------------------------------------------------------------
// The scanner
// ---------------------------------------------------------------------------

fn rules(path: &str, src: &str) -> Vec<String> {
    scan(path, src)
        .into_iter()
        .map(|v| v.split(' ').nth(1).unwrap().to_owned())
        .collect()
}

const OTHER: &str = "src/main.rs";

#[test]
fn a_flag_literal_outside_the_module_is_flagged() {
    assert_eq!(
        rules(OTHER, "fn f() { let _ = \"--qr-rig-open-pane=x\"; }\n"),
        ["R1"]
    );
    assert_eq!(rules(OTHER, "const A: &str = r#\"qr-rig\"#;\n"), ["R1"]);
    assert!(scan(RIG_FILE, "const A: &str = \"--qr-rig-open-pane\";\n").is_empty());
}

#[test]
fn comments_and_look_alikes_are_not_flagged() {
    assert!(scan(OTHER, "// the qr-rig flags\nfn f() {}\n").is_empty());
    assert!(scan(
        OTHER,
        "/* qr-rig */\nfn f() { let _ = \"qr-rigid\".len() > 99; }\n"
    )
    .iter()
    .all(|v| v.contains("R1")));
    assert!(scan(OTHER, "fn f() { let _ = \"qr-ri\"; let _ = \"rig\"; }\n").is_empty());
}

#[test]
fn an_ungated_call_into_rig_hooks_is_flagged() {
    assert_eq!(
        rules(
            OTHER,
            "fn f() {\n    quietrabbit_lib::rig_hooks::spawn();\n}\n"
        ),
        ["R3"]
    );
    assert_eq!(rules(OTHER, "use crate::rig_hooks::parse;\n"), ["R3"]);
}

#[test]
fn a_gated_call_passes_even_across_lines_and_comments() {
    let src = "fn f() {\n    #[cfg(debug_assertions)]\n    // note\n\n    quietrabbit_lib::rig_hooks::spawn();\n}\n";
    assert!(scan(OTHER, src).is_empty());
}

#[test]
fn a_different_cfg_is_not_a_gate() {
    assert_eq!(
        rules(
            OTHER,
            "#[cfg(not(debug_assertions))]\nquietrabbit_lib::rig_hooks::spawn();\n"
        ),
        ["R3"]
    );
    assert_eq!(
        rules(
            OTHER,
            "#[cfg(any(debug_assertions, test))]\nquietrabbit_lib::rig_hooks::spawn();\n"
        ),
        ["R3"]
    );
}

#[test]
fn mod_rig_hooks_needs_the_gate() {
    assert_eq!(rules(LIB_FILE, "mod rig_hooks;\n"), ["R2"]);
    assert_eq!(rules(LIB_FILE, "pub mod rig_hooks;\n"), ["R2"]);
    assert_eq!(
        rules(
            LIB_FILE,
            "#[cfg(not(debug_assertions))]\npub mod rig_hooks;\n"
        ),
        ["R2"]
    );
    assert!(scan(LIB_FILE, "#[cfg(debug_assertions)]\npub mod rig_hooks;\n").is_empty());
    assert!(scan(LIB_FILE, "mod rig_hooks_other;\npub mod rig_hooked;\n").is_empty());
    assert_eq!(
        mod_rig_count("#[cfg(debug_assertions)]\npub mod rig_hooks;\n"),
        1
    );
    assert_eq!(mod_rig_count("// mod rig_hooks;\nmod rig_hooks_x;\n"), 0);
}

#[test]
fn words_that_only_contain_rig_hooks_are_not_calls() {
    assert!(scan(OTHER, "fn f() { my_rig_hooks::x(); }\n").is_empty());
}
