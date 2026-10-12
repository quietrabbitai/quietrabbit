// src-tauri/src/provider_credentials/key_pattern.rs
//
// Clipboard assist and key syntax (items.id=714; decisions.id=875 point 7,
// spec Part 8g). A provider row may hold `credential_acquisition.key_pattern`,
// a regular expression the clipboard text must match in full before QR offers
// it. No pattern means no offer: the pattern is row data and is never guessed.
//
// Nothing here logs, formats or returns key text except the candidate handed
// back in a Zeroizing buffer.

use regex::RegexBuilder;
use zeroize::Zeroizing;

/// Longest key text accepted, in bytes. Checked before any pattern runs.
pub const MAX_KEY_BYTES: usize = 512;

/// Compiled-program size limit for a row pattern, in bytes.
const PATTERN_SIZE_LIMIT: usize = 64 * 1024;

/// A row pattern that cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternError {
    /// Empty, too long, or not a valid regular expression.
    Unusable,
}

/// The key text inside `raw`: surrounding ASCII whitespace removed once (a
/// copy often carries a trailing newline), then one line only. None when it is
/// empty, longer than `MAX_KEY_BYTES`, or holds any whitespace or control
/// character inside.
pub fn normalise_key_text(raw: &str) -> Option<&str> {
    if raw.len() > MAX_KEY_BYTES + 16 {
        return None;
    }
    let text = raw.trim_matches(|c: char| c.is_ascii_whitespace());
    if text.is_empty() || text.len() > MAX_KEY_BYTES {
        return None;
    }
    if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    Some(text)
}

/// Whether `pattern` is usable as a row pattern: compiles under the size limit.
pub fn pattern_is_usable(pattern: &str) -> bool {
    compile(pattern).is_ok()
}

fn compile(pattern: &str) -> Result<regex::Regex, PatternError> {
    if pattern.is_empty() || pattern.len() > 256 {
        return Err(PatternError::Unusable);
    }
    // Anchored at both ends whatever the row says, so a partial match can
    // never offer text that merely contains something key-shaped.
    RegexBuilder::new(&format!(r"\A(?:{pattern})\z"))
        .size_limit(PATTERN_SIZE_LIMIT)
        .build()
        .map_err(|_| PatternError::Unusable)
}

/// `Ok(Some(key))` when `clipboard_text` is one line, within the length bound,
/// and matches `pattern` in full; `Ok(None)` when it does not;
/// `Err` when the row pattern is unusable. The returned key is the normalised
/// text.
pub fn match_clipboard(
    pattern: &str,
    clipboard_text: &str,
) -> Result<Option<Zeroizing<String>>, PatternError> {
    let re = compile(pattern)?;
    let Some(text) = normalise_key_text(clipboard_text) else {
        return Ok(None);
    };
    Ok(re.is_match(text).then(|| Zeroizing::new(text.to_owned())))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATTERN: &str = r"fx_[A-Za-z0-9]{8,64}";

    #[test]
    fn a_full_match_is_offered_and_trailing_whitespace_is_dropped() {
        for ok in ["fx_abcdefgh", "fx_abcdefgh\n", "  fx_abcdefgh\r\n"] {
            let got = match_clipboard(PATTERN, ok).unwrap();
            assert_eq!(
                got.as_deref().map(String::as_str),
                Some("fx_abcdefgh"),
                "{ok:?}"
            );
        }
    }

    #[test]
    fn whitespace_inside_multiple_lines_and_wrong_text_never_match() {
        let no = [
            "fx_abc defgh",
            "fx_abcdefgh\nfx_abcdefgh",
            "fx_abcdefgh\tx",
            "fx_abcdefgh\u{a0}",
            "fx_abcdefgh\u{0}",
            "key: fx_abcdefgh",
            "prefix fx_abcdefgh",
            "fx_abcdefgh suffix",
            "fx_short",
            "FX_abcdefgh",
            "https://example.test/fx_abcdefgh",
            "",
            "   ",
            "\n",
        ];
        for text in no {
            assert!(
                match_clipboard(PATTERN, text).unwrap().is_none(),
                "{text:?}"
            );
        }
    }

    #[test]
    fn the_pattern_is_anchored_even_when_the_row_forgot_to() {
        assert!(match_clipboard("fx_", "fx_abcdefgh").unwrap().is_none());
        assert!(match_clipboard("abcdefgh", "fx_abcdefgh")
            .unwrap()
            .is_none());
        // An alternation cannot escape the anchors.
        assert!(match_clipboard("aaa|bbb", "xaaa").unwrap().is_none());
        assert!(match_clipboard("aaa|bbb", "bbb").unwrap().is_some());
    }

    #[test]
    fn an_overlong_clipboard_is_refused_before_matching() {
        let long = format!("fx_{}", "a".repeat(MAX_KEY_BYTES));
        assert!(match_clipboard(r"fx_[a-z]+", &long).unwrap().is_none());
        let huge = "a".repeat(10 * 1024 * 1024);
        assert!(match_clipboard(r"[a-z]+", &huge).unwrap().is_none());
    }

    #[test]
    fn an_unusable_row_pattern_fails_closed() {
        for bad in [
            "",
            "(",
            "[a-",
            "*abc",
            &"a".repeat(300),
            "(?:a{1000}){1000}{1000}",
        ] {
            assert!(
                matches!(match_clipboard(bad, "abc"), Err(PatternError::Unusable)),
                "{bad:?}"
            );
            assert!(!pattern_is_usable(bad));
        }
        assert!(pattern_is_usable(PATTERN));
    }

    #[test]
    fn typed_key_text_rules() {
        assert_eq!(normalise_key_text(" sk-123 \n"), Some("sk-123"));
        for bad in [
            "",
            "  ",
            "a b",
            "a\nb",
            "a\u{7f}",
            &"k".repeat(MAX_KEY_BYTES + 1),
        ] {
            assert_eq!(normalise_key_text(bad), None, "{bad:?}");
        }
        assert!(normalise_key_text(&"k".repeat(MAX_KEY_BYTES)).is_some());
    }
}
