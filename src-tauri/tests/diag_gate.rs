// src-tauri/tests/diag_gate.rs
//
// items.id=649: release builds must carry no `DIAG` logging and no per-GL-call
// global lock. This test does not parse Rust or cfg blocks. It lexes each
// source file (comments and literal contents are separated from code) and
// checks four structural rules:
//
//   R1  A string literal containing the word `DIAG` appears only in
//       `cloud_chat_gpu_pane/diag.rs`. Every log macro form carries a literal,
//       so this covers `log::x!`, bare `info!`, multi-line and `target:` calls.
//       (`QR_CEF_GPU_DIAG` does not match: `DIAG` must be a whole word.)
//   R2  `Diag312GlGuard` and `DIAG_312_GL_ACTIVE` never appear in code.
//   R3  Every `mod diag` is preceded by `#[cfg(debug_assertions)]`, and there is
//       exactly one. With R1, no DIAG literal can be compiled in a release.
//   R4  Every use of `diag::` outside diag.rs is preceded by
//       `#[cfg(debug_assertions)]` on the previous code line.
//
// The scanner is a pure function, so the fixtures below prove it fails on each
// kind of regression and does not fire on comments or look-alikes.

use std::fs;
use std::path::{Path, PathBuf};

const GATE: &str = "#[cfg(debug_assertions)]";
const DIAG_FILE: &str = "cloud_chat_gpu_pane/diag.rs";
const MOD_FILE: &str = "cloud_chat_gpu_pane/mod.rs";

struct Lexed {
    /// One entry per source line, with comments and the contents of string and
    /// char literals replaced by spaces.
    code: Vec<String>,
    /// (1-based start line, content) of every string literal.
    literals: Vec<(usize, String)>,
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn blank(c: char) -> char {
    if c == '\n' {
        '\n'
    } else {
        ' '
    }
}

/// Where a string literal starts at `i`: (index of the opening quote, raw,
/// number of `#`). Handles `"`, `b"`, `r"`, `r#"`, `br#"`.
fn string_start(chars: &[char], i: usize) -> Option<(usize, bool, usize)> {
    let c = chars[i];
    if c == '"' {
        return Some((i, false, 0));
    }
    if (c != 'r' && c != 'b') || (i > 0 && is_ident(chars[i - 1])) {
        return None;
    }
    let mut j = i;
    if chars[j] == 'b' {
        j += 1;
    }
    let raw = chars.get(j) == Some(&'r');
    let mut hashes = 0;
    if raw {
        j += 1;
        while chars.get(j) == Some(&'#') {
            hashes += 1;
            j += 1;
        }
    }
    if chars.get(j) == Some(&'"') && (raw || chars[i] == 'b') {
        Some((j, raw, hashes))
    } else {
        None
    }
}

fn lex(src: &str) -> Lexed {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(src.len());
    let mut literals = Vec::new();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            while i < n && chars[i] != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        if c == '/' && next == Some('*') {
            let mut depth = 0;
            while i < n {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    out.push_str("  ");
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    out.push_str("  ");
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    out.push(blank(chars[i]));
                    i += 1;
                }
            }
            continue;
        }
        if let Some((q, raw, hashes)) = string_start(&chars, i) {
            let start_line = out.matches('\n').count() + 1;
            for &p in &chars[i..q] {
                out.push(p);
            }
            out.push('"');
            let mut k = q + 1;
            let mut content = String::new();
            while k < n {
                let ch = chars[k];
                if raw {
                    if ch == '"' && (0..hashes).all(|h| chars.get(k + 1 + h) == Some(&'#')) {
                        k += 1 + hashes;
                        break;
                    }
                } else if ch == '\\' {
                    content.push(ch);
                    out.push(' ');
                    k += 1;
                    if k < n {
                        content.push(chars[k]);
                        out.push(blank(chars[k]));
                        k += 1;
                    }
                    continue;
                } else if ch == '"' {
                    k += 1;
                    break;
                }
                content.push(ch);
                out.push(blank(ch));
                k += 1;
            }
            out.push('"');
            literals.push((start_line, content));
            i = k;
            continue;
        }
        if c == '\'' {
            if next == Some('\\') {
                // Escaped char literal: skip the backslash and the escaped
                // character, then everything up to the closing quote.
                let mut k = (i + 3).min(n);
                while k < n && chars[k] != '\'' {
                    k += 1;
                }
                let end = (k + 1).min(n);
                out.push('\'');
                for _ in i + 1..end.saturating_sub(1) {
                    out.push(' ');
                }
                out.push('\'');
                i = end;
                continue;
            }
            if chars.get(i + 2) == Some(&'\'') {
                out.push_str("' '");
                i += 3;
                continue;
            }
            // A lifetime: left as code.
        }
        out.push(c);
        i += 1;
    }
    Lexed {
        code: out.split('\n').map(str::to_owned).collect(),
        literals,
    }
}

/// `word` as a whole word (not preceded or followed by an identifier char).
fn has_word(s: &str, word: &str) -> bool {
    s.match_indices(word).any(|(pos, _)| {
        let before = s[..pos].chars().next_back();
        let after = s[pos + word.len()..].chars().next();
        !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
    })
}

fn is_mod_diag(code_line: &str) -> bool {
    let mut t = code_line.trim();
    for vis in ["pub(crate) ", "pub(super) ", "pub "] {
        if let Some(rest) = t.strip_prefix(vis) {
            t = rest;
            break;
        }
    }
    t.strip_prefix("mod diag")
        .is_some_and(|rest| !rest.chars().next().is_some_and(is_ident))
}

/// The previous non-blank line of code (comments are already blanked).
fn previous_code_line(code: &[String], idx: usize) -> Option<&str> {
    code[..idx]
        .iter()
        .rev()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
}

fn mod_diag_count(src: &str) -> usize {
    lex(src).code.iter().filter(|l| is_mod_diag(l)).count()
}

/// Rule violations in one file. `path` uses `/` separators.
fn scan(path: &str, src: &str) -> Vec<String> {
    let is_diag_file = path.ends_with(DIAG_FILE);
    let lexed = lex(src);
    let mut found = Vec::new();

    if !is_diag_file {
        for (line, lit) in &lexed.literals {
            if has_word(lit, "DIAG") {
                found.push(format!(
                    "{path}:{line}: R1 a string literal contains DIAG outside {DIAG_FILE}; \
                     put the log in diag.rs (debug-only, opt-in) or remove it"
                ));
            }
        }
    }

    for (idx, code) in lexed.code.iter().enumerate() {
        let line = idx + 1;
        for token in ["Diag312GlGuard", "DIAG_312_GL_ACTIVE"] {
            if code.contains(token) {
                found.push(format!(
                    "{path}:{line}: R2 `{token}` (the per-GL-call lock) must not come back"
                ));
            }
        }
        if is_mod_diag(code) && previous_code_line(&lexed.code, idx) != Some(GATE) {
            found.push(format!(
                "{path}:{line}: R3 `mod diag` must be preceded by {GATE}"
            ));
        }
        if !is_diag_file {
            let calls_diag = code
                .match_indices("diag::")
                .any(|(pos, _)| !code[..pos].chars().next_back().is_some_and(is_ident));
            if calls_diag && previous_code_line(&lexed.code, idx) != Some(GATE) {
                found.push(format!(
                    "{path}:{line}: R4 a call into diag must sit on the line after {GATE}"
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

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
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
fn the_crate_has_no_ungated_diag_logging() {
    let mut files = Vec::new();
    rust_files(&src_dir(), &mut files);
    // A scanner that silently saw nothing would pass for the wrong reason.
    assert!(files.len() > 50, "only {} source files found", files.len());
    assert!(files.iter().any(|f| rel(f).ends_with(MOD_FILE)));
    assert!(files.iter().any(|f| rel(f).ends_with(DIAG_FILE)));

    let mut violations = Vec::new();
    let mut mod_diag = 0;
    for file in &files {
        let src = fs::read_to_string(file).unwrap();
        violations.extend(scan(&rel(file), &src));
        mod_diag += mod_diag_count(&src);
    }
    assert_eq!(mod_diag, 1, "expected exactly one `mod diag`");
    assert!(
        violations.is_empty(),
        "DIAG gate violated (items.id=649):\n{}",
        violations.join("\n")
    );
}

#[test]
fn the_real_diag_module_is_caught_if_it_is_misplaced() {
    let src = fs::read_to_string(src_dir().join(DIAG_FILE)).unwrap();
    assert!(
        !scan("src/somewhere_else.rs", &src).is_empty(),
        "the DIAG strings in diag.rs were not detected"
    );
    assert!(scan(&format!("src/{DIAG_FILE}"), &src).is_empty());
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

const OTHER: &str = "src/cloud_chat_gpu_pane/pane_host.rs";

#[test]
fn an_ungated_diag_log_is_flagged() {
    let src = r#"fn f() { log::warn!("DIAG items.id=1: x"); }"#;
    assert_eq!(rules(OTHER, src), ["R1"]);
}

#[test]
fn a_bare_macro_name_and_a_prefixed_message_are_flagged() {
    let src = r#"fn f() { info!("cloud_chat_gpu_pane::pane_host: DIAG items.id=539: x"); }"#;
    assert_eq!(rules(OTHER, src), ["R1"]);
}

#[test]
fn a_multi_line_log_is_flagged_at_its_literal() {
    let src = "fn f() {\n    log::info!(\n        \"a \\\n b DIAG items.id=2\"\n    );\n}\n";
    let found = scan(OTHER, src);
    assert_eq!(found.len(), 1);
    assert!(found[0].contains(":3:"), "{found:?}");
}

#[test]
fn escaped_quotes_and_backslashes_do_not_hide_a_later_diag_literal() {
    let src = r#"fn f() { let s = "a\\"; log::warn!("say \"hi\" DIAG items.id=3"); }"#;
    assert_eq!(rules(OTHER, src), ["R1"]);
}

#[test]
fn raw_strings_chars_and_lifetimes_do_not_confuse_the_lexer() {
    let src = r##"
fn f<'a>(x: &'a str) {
    let r = r#"a // b "quoted" c"#;
    let q = '"';
    let e = '\'';
    log::warn!("DIAG items.id=4");
}
"##;
    assert_eq!(rules(OTHER, src), ["R1"]);
}

#[test]
fn diag_in_comments_and_look_alikes_is_not_flagged() {
    let src = r#"
// DIAG items.id=1: a comment
/* DIAG /* nested DIAG */ DIAG */
/// DIAG in a doc comment
const GPU_DIAG_ENV: &str = "QR_CEF_GPU_DIAG";
const OTHER_DIAGNOSTIC: &str = "diagnostic";
fn f() { log::info!("no tag here"); }
"#;
    assert!(scan(OTHER, src).is_empty(), "{:?}", scan(OTHER, src));
}

#[test]
fn diag_literals_are_allowed_in_the_diag_module_only() {
    let src = r#"fn f() { log::warn!("DIAG items.id=365: x"); }"#;
    assert!(scan("src/cloud_chat_gpu_pane/diag.rs", src).is_empty());
    assert_eq!(rules("src/cloud_chat_gpu_pane/render.rs", src), ["R1"]);
}

#[test]
fn the_gl_guard_is_flagged_in_code_but_not_in_comments() {
    assert_eq!(rules(OTHER, "struct Diag312GlGuard;"), ["R2"]);
    assert_eq!(rules(OTHER, "static DIAG_312_GL_ACTIVE: u8 = 0;"), ["R2"]);
    assert_eq!(
        rules("src/cloud_chat_gpu_pane/diag.rs", "struct Diag312GlGuard;"),
        ["R2"]
    );
    assert!(scan(OTHER, "// Diag312GlGuard was removed\n").is_empty());
}

#[test]
fn an_ungated_call_into_diag_is_flagged() {
    let src = "fn f() {\n    diag::sync_apply(k, a, b);\n}\n";
    assert_eq!(rules(OTHER, src), ["R4"]);
    let src = "fn f() {\n    crate::cloud_chat_gpu_pane::diag::tick();\n}\n";
    assert_eq!(rules(OTHER, src), ["R4"]);
}

#[test]
fn a_gated_call_into_diag_passes_even_across_lines_and_comments() {
    let src = "fn f() {\n    #[cfg(debug_assertions)]\n    diag::sync_apply(k, a, b);\n}\n";
    assert!(scan(OTHER, src).is_empty());
    let src = "fn f() {\n    #[cfg(debug_assertions)]\n    // why\n\n    diag::input_dropped(\n        \"k\",\n    );\n}\n";
    assert!(scan(OTHER, src).is_empty());
}

#[test]
fn a_different_cfg_is_not_a_gate() {
    for gate in [
        "#[cfg(not(debug_assertions))]",
        "#[cfg(any(debug_assertions, test))]",
        "#[cfg(test)]",
        "#[inline]",
    ] {
        let src = format!("fn f() {{\n    {gate}\n    diag::tick();\n}}\n");
        assert_eq!(rules(OTHER, &src), ["R4"], "{gate}");
    }
}

#[test]
fn words_that_only_contain_diag_are_not_calls() {
    let src = "fn f() { gpu_diag::x(); mydiag::y(); gpu_diag_requested(v); }\n";
    assert!(scan(OTHER, src).is_empty());
}

#[test]
fn mod_diag_needs_the_gate() {
    assert_eq!(rules("src/x/mod.rs", "mod diag;\n"), ["R3"]);
    assert_eq!(rules("src/x/mod.rs", "pub mod diag;\n"), ["R3"]);
    assert_eq!(
        rules("src/x/mod.rs", "#[cfg(not(debug_assertions))]\nmod diag;\n"),
        ["R3"]
    );
    assert!(scan("src/x/mod.rs", "#[cfg(debug_assertions)]\nmod diag;\n").is_empty());
    assert!(scan("src/x/mod.rs", "mod diagnostics;\nmod diag_other;\n").is_empty());
    assert_eq!(mod_diag_count("#[cfg(debug_assertions)]\nmod diag;\n"), 1);
    assert_eq!(mod_diag_count("// mod diag;\nmod diagnostics;\n"), 0);
}
