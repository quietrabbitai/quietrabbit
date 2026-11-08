// src-tauri/tests/common/lex.rs
//
// A small Rust source lexer shared by the structural gate tests (diag_gate.rs,
// rig_gate.rs). It does not parse Rust: it separates comments and the contents of
// string and char literals from code, line by line, so a gate can look at code
// tokens and at string literals independently. Pulled in with
// `#[path = "common/lex.rs"] mod lex;` by the tests that use it, not through
// common/mod.rs (the SQLCipher helpers there are unrelated).

#![allow(dead_code)]

pub struct Lexed {
    /// One entry per source line, with comments and the contents of string and
    /// char literals replaced by spaces.
    pub code: Vec<String>,
    /// (1-based start line, content) of every string literal.
    pub literals: Vec<(usize, String)>,
}

pub fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

pub fn blank(c: char) -> char {
    if c == '\n' {
        '\n'
    } else {
        ' '
    }
}

/// Where a string literal starts at `i`: (index of the opening quote, raw,
/// number of `#`). Handles `"`, `b"`, `r"`, `r#"`, `br#"`.
pub fn string_start(chars: &[char], i: usize) -> Option<(usize, bool, usize)> {
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

pub fn lex(src: &str) -> Lexed {
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
pub fn has_word(s: &str, word: &str) -> bool {
    s.match_indices(word).any(|(pos, _)| {
        let before = s[..pos].chars().next_back();
        let after = s[pos + word.len()..].chars().next();
        !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
    })
}

/// The previous non-blank line of code (comments are already blanked).
pub fn previous_code_line(code: &[String], idx: usize) -> Option<&str> {
    code[..idx]
        .iter()
        .rev()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
}
