// src-tauri/src/spend/price.rs
//
// Turns a model's `pricing` JSON (curated row or user override) into integer
// prices (items.id=710, scope item 7; review change C).
//
// EXACT PARSING WITHOUT A CRATE FEATURE: serde_json turns `0.07` into an f64
// and the decimal text is gone. Enabling its `arbitrary_precision` feature
// would change number handling crate-wide, so instead this module reads the
// raw JSON TEXT with a small tokenizer that keeps each number as its source
// lexeme, then converts the lexeme with `money::parse_decimal_micros`. The
// text is first validated by serde_json (structure), and every extracted
// lexeme is cross-checked against serde_json's own f64 reading of the same
// field as a guard against a tokenizer mistake.
//
// A price that cannot be read exactly (exponent form, more than six decimal
// places, a negative, a missing field) makes the whole price UNKNOWN. Unknown
// is never guessed and never passes the pre-flight (spend/gate.rs).
//
// The shape read (shared_028.sql):
//   {"input_per_mtok":0.07,"output_per_mtok":0.30,"currency":"USD",
//    "tiers":[{"prompt_tokens_up_to":100000,"input_per_mtok":..,"output_per_mtok":..},
//             {"prompt_tokens_over":100000,...}],
//    "as_of":"2026-10-11","source_url":"...","source":"curated","note":"..."}
// Cache prices are ignored (the estimate then over-counts, never under-counts).

use chrono::NaiveDate;
use serde_json::Value;

use crate::spend::money::parse_decimal_micros;

/// Curated prices older than this say so (decisions.id=875 point 6).
pub const PRICE_STALE_AFTER_DAYS: i64 = 90;

/// Where a price came from. The string values are the ledger's
/// `price_source` column values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceSource {
    ProviderApi,
    Curated,
    User,
}

impl PriceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            PriceSource::ProviderApi => "provider_api",
            PriceSource::Curated => "curated",
            PriceSource::User => "user",
        }
    }

    fn from_json(s: &str) -> Option<Self> {
        match s {
            "provider_api" => Some(PriceSource::ProviderApi),
            "curated" => Some(PriceSource::Curated),
            "user" => Some(PriceSource::User),
            _ => None,
        }
    }
}

/// One prompt-length tier. Exactly one of `up_to` / `over` is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceTier {
    /// Applies when the prompt has at most this many tokens.
    pub up_to: Option<u64>,
    /// Applies when the prompt has more than this many tokens.
    pub over: Option<u64>,
    pub input_micro_per_mtok: u64,
    pub output_micro_per_mtok: u64,
}

impl PriceTier {
    fn matches(&self, prompt_tokens: u64) -> bool {
        match (self.up_to, self.over) {
            (Some(n), None) => prompt_tokens <= n,
            (None, Some(n)) => prompt_tokens > n,
            _ => false,
        }
    }
}

/// A fully-read price, in integer micro-USD per million tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Price {
    pub input_micro_per_mtok: u64,
    pub output_micro_per_mtok: u64,
    pub tiers: Vec<PriceTier>,
    pub source: PriceSource,
    pub as_of: Option<NaiveDate>,
}

impl Price {
    /// The (input, output) rates that apply to a prompt of this size.
    pub fn rates_for(&self, prompt_tokens: u64) -> (u64, u64) {
        self.tiers
            .iter()
            .find(|t| t.matches(prompt_tokens))
            .map(|t| (t.input_micro_per_mtok, t.output_micro_per_mtok))
            .unwrap_or((self.input_micro_per_mtok, self.output_micro_per_mtok))
    }

    /// Known to cost exactly nothing: the base price and every tier are 0/0.
    pub fn is_free(&self) -> bool {
        self.input_micro_per_mtok == 0
            && self.output_micro_per_mtok == 0
            && self
                .tiers
                .iter()
                .all(|t| t.input_micro_per_mtok == 0 && t.output_micro_per_mtok == 0)
    }

    /// The most expensive (input, output) rates across the base price and all
    /// tiers. Used when the prompt size is not known well enough to pick a tier.
    pub fn max_rates(&self) -> (u64, u64) {
        let mut input = self.input_micro_per_mtok;
        let mut output = self.output_micro_per_mtok;
        for t in &self.tiers {
            input = input.max(t.input_micro_per_mtok);
            output = output.max(t.output_micro_per_mtok);
        }
        (input, output)
    }

    /// Whole days between `as_of` and `today`; `None` when there is no date.
    pub fn age_days(&self, today: NaiveDate) -> Option<i64> {
        self.as_of.map(|d| (today - d).num_days())
    }

    /// Older than the staleness window. A price with no date is stale.
    ///
    /// A price the user typed in themselves (`source` = `User`) is never
    /// flagged: the "estimate may be stale" note warns about QR's curated
    /// data going out of date (decisions.id=875 point 6, items.id=711), and
    /// the user set this one knowingly.
    pub fn is_stale(&self, today: NaiveDate) -> bool {
        if self.source == PriceSource::User {
            return false;
        }
        match self.age_days(today) {
            Some(days) => days > PRICE_STALE_AFTER_DAYS,
            None => true,
        }
    }
}

// ---------------------------------------------------------------------------
// Lexeme-preserving JSON reader
// ---------------------------------------------------------------------------

/// JSON where a number keeps its source text.
#[derive(Debug, Clone, PartialEq)]
enum Lex {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<Lex>),
    Obj(Vec<(String, Lex)>),
}

const MAX_DEPTH: usize = 12;

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            bytes: text.as_bytes(),
            pos: 0,
        }
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.bytes.get(self.pos) {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn eat(&mut self, b: u8) -> Option<()> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Some(())
        } else {
            None
        }
    }

    fn literal(&mut self, word: &str) -> Option<()> {
        let end = self.pos.checked_add(word.len())?;
        if self.bytes.get(self.pos..end)? == word.as_bytes() {
            self.pos = end;
            Some(())
        } else {
            None
        }
    }

    fn value(&mut self, depth: usize) -> Option<Lex> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.skip_ws();
        match self.peek()? {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => self.string().map(Lex::Str),
            b't' => self.literal("true").map(|_| Lex::Bool(true)),
            b'f' => self.literal("false").map(|_| Lex::Bool(false)),
            b'n' => self.literal("null").map(|_| Lex::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    fn number(&mut self) -> Option<Lex> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if matches!(b, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return None;
        }
        // The lexeme is ASCII by construction.
        let lexeme = std::str::from_utf8(&self.bytes[start..self.pos]).ok()?;
        Some(Lex::Num(lexeme.to_owned()))
    }

    fn string(&mut self) -> Option<String> {
        self.eat(b'"')?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let b = self.peek()?;
            self.pos += 1;
            match b {
                b'"' => return String::from_utf8(out).ok(),
                b'\\' => {
                    let esc = self.peek()?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            // Not needed for any key we read. Skip four hex
                            // digits and keep a placeholder.
                            for _ in 0..4 {
                                let h = self.peek()?;
                                if !h.is_ascii_hexdigit() {
                                    return None;
                                }
                                self.pos += 1;
                            }
                            out.push(b'?');
                        }
                        _ => return None,
                    }
                }
                other => out.push(other),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Option<Lex> {
        self.eat(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Some(Lex::Arr(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek()? {
                b',' => self.pos += 1,
                b']' => {
                    self.pos += 1;
                    return Some(Lex::Arr(items));
                }
                _ => return None,
            }
        }
    }

    fn object(&mut self, depth: usize) -> Option<Lex> {
        self.eat(b'{')?;
        let mut members: Vec<(String, Lex)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Some(Lex::Obj(members));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            // A duplicate key is ambiguous: refuse the whole document.
            if members.iter().any(|(k, _)| *k == key) {
                return None;
            }
            self.skip_ws();
            self.eat(b':')?;
            let v = self.value(depth + 1)?;
            members.push((key, v));
            self.skip_ws();
            match self.peek()? {
                b',' => self.pos += 1,
                b'}' => {
                    self.pos += 1;
                    return Some(Lex::Obj(members));
                }
                _ => return None,
            }
        }
    }
}

fn parse_lex(text: &str) -> Option<Lex> {
    let mut r = Reader::new(text);
    let v = r.value(0)?;
    r.skip_ws();
    if r.pos == r.bytes.len() {
        Some(v)
    } else {
        None
    }
}

fn field<'a>(obj: &'a [(String, Lex)], key: &str) -> Option<&'a Lex> {
    obj.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// A price field: its lexeme converted exactly, cross-checked against the
/// serde_json reading of the same field. `None` when missing, not a number,
/// not exactly representable, or inconsistent.
fn price_field(obj: &[(String, Lex)], reference: &Value, key: &str) -> Option<u64> {
    let Lex::Num(lexeme) = field(obj, key)? else {
        return None;
    };
    let micros = parse_decimal_micros(lexeme)?;
    // Guard: the independent reading of the same document must agree.
    let by_serde = reference.get(key)?.as_f64()?;
    let by_lexeme = lexeme.parse::<f64>().ok()?;
    if by_serde.to_bits() != by_lexeme.to_bits() {
        return None;
    }
    Some(micros)
}

/// A token-count field of a tier (a plain unsigned integer lexeme).
fn count_field(obj: &[(String, Lex)], key: &str) -> Option<Option<u64>> {
    match field(obj, key) {
        None | Some(Lex::Null) => Some(None),
        Some(Lex::Num(lexeme)) => {
            if lexeme.is_empty() || !lexeme.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some(Some(lexeme.parse().ok()?))
        }
        Some(_) => None,
    }
}

/// Read a `pricing` JSON text into integer prices. `forced_source` overrides
/// the document's own `source` (a user override is always `User`).
/// `None` means the price is unknown.
pub fn parse_pricing(raw: &str, forced_source: Option<PriceSource>) -> Option<Price> {
    let reference: Value = serde_json::from_str(raw).ok()?;
    let Lex::Obj(root) = parse_lex(raw)? else {
        return None;
    };

    let input = price_field(&root, &reference, "input_per_mtok")?;
    let output = price_field(&root, &reference, "output_per_mtok")?;

    let mut tiers = Vec::new();
    match field(&root, "tiers") {
        None | Some(Lex::Null) => {}
        Some(Lex::Arr(items)) => {
            let ref_tiers = reference.get("tiers")?.as_array()?;
            if ref_tiers.len() != items.len() {
                return None;
            }
            for (item, ref_item) in items.iter().zip(ref_tiers) {
                let Lex::Obj(t) = item else { return None };
                let up_to = count_field(t, "prompt_tokens_up_to")?;
                let over = count_field(t, "prompt_tokens_over")?;
                // Exactly one bound per tier.
                if up_to.is_some() == over.is_some() {
                    return None;
                }
                tiers.push(PriceTier {
                    up_to,
                    over,
                    input_micro_per_mtok: price_field(t, ref_item, "input_per_mtok")?,
                    output_micro_per_mtok: price_field(t, ref_item, "output_per_mtok")?,
                });
            }
        }
        Some(_) => return None,
    }

    let source = forced_source
        .or_else(|| match field(&root, "source") {
            Some(Lex::Str(s)) => PriceSource::from_json(s),
            _ => None,
        })
        // A document that does not say where it came from is a curated row.
        .unwrap_or(PriceSource::Curated);

    let as_of = match field(&root, "as_of") {
        Some(Lex::Str(s)) => NaiveDate::parse_from_str(s, "%Y-%m-%d").ok(),
        _ => None,
    };

    Some(Price {
        input_micro_per_mtok: input,
        output_micro_per_mtok: output,
        tiers,
        source,
        as_of,
    })
}

/// What the spend code knows about a model's price.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PriceQuote {
    Known(Price),
    Unknown,
}

/// Resolve a model's price: a user override wins over the curated row; no
/// readable price at all is unknown.
pub fn resolve_price(override_raw: Option<&str>, row_raw: Option<&str>) -> PriceQuote {
    if let Some(raw) = override_raw {
        // An override that cannot be read does not silently fall back to the
        // curated price: the user asked for something else, so it is unknown.
        return match parse_pricing(raw, Some(PriceSource::User)) {
            Some(p) => PriceQuote::Known(p),
            None => PriceQuote::Unknown,
        };
    }
    match row_raw.and_then(|raw| parse_pricing(raw, None)) {
        Some(p) => PriceQuote::Known(p),
        None => PriceQuote::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    const PLAIN: &str = r#"{"input_per_mtok":0.07,"output_per_mtok":0.30,"currency":"USD","cache":null,"tiers":null,"as_of":"2026-10-11","source_url":"https://example.test/p","source":"curated","note":"Cached input 0.035."}"#;

    #[test]
    fn plain_price_parses_exactly() {
        let p = parse_pricing(PLAIN, None).unwrap();
        assert_eq!(p.input_micro_per_mtok, 70_000);
        assert_eq!(p.output_micro_per_mtok, 300_000);
        assert_eq!(p.source, PriceSource::Curated);
        assert_eq!(p.as_of, Some(d(2026, 10, 11)));
        assert!(p.tiers.is_empty());
        assert!(!p.is_free());
    }

    #[test]
    fn tiers_parse_and_select_by_prompt_size() {
        let raw = r#"{"input_per_mtok":0.10,"output_per_mtok":0.50,"tiers":[
            {"prompt_tokens_up_to":100000,"input_per_mtok":0.10,"output_per_mtok":0.50},
            {"prompt_tokens_over":100000,"input_per_mtok":0.50,"output_per_mtok":2.50}],
            "as_of":"2026-10-11","source":"curated"}"#;
        let p = parse_pricing(raw, None).unwrap();
        assert_eq!(p.tiers.len(), 2);
        assert_eq!(p.rates_for(100_000), (100_000, 500_000));
        assert_eq!(p.rates_for(100_001), (500_000, 2_500_000));
        assert_eq!(p.rates_for(1), (100_000, 500_000));
        assert_eq!(p.max_rates(), (500_000, 2_500_000));
    }

    #[test]
    fn zero_zero_is_free_but_missing_is_unknown() {
        let free = r#"{"input_per_mtok":0,"output_per_mtok":0,"currency":"USD","as_of":"2026-10-11","source":"curated"}"#;
        assert!(parse_pricing(free, None).unwrap().is_free());
        let free_dec = r#"{"input_per_mtok":0.0,"output_per_mtok":0.00}"#;
        assert!(parse_pricing(free_dec, None).unwrap().is_free());
        assert_eq!(resolve_price(None, None), PriceQuote::Unknown);
        assert_eq!(resolve_price(None, Some("null")), PriceQuote::Unknown);
        assert_eq!(resolve_price(None, Some("{}")), PriceQuote::Unknown);
    }

    #[test]
    fn a_free_base_with_a_paid_tier_is_not_free() {
        let raw = r#"{"input_per_mtok":0,"output_per_mtok":0,"tiers":[
            {"prompt_tokens_over":1000,"input_per_mtok":1,"output_per_mtok":1}]}"#;
        assert!(!parse_pricing(raw, None).unwrap().is_free());
    }

    /// Digits, quotes, braces and price-looking keys inside a string must not
    /// confuse the reader.
    #[test]
    fn strings_cannot_smuggle_numbers() {
        let raw = r#"{"note":"\"input_per_mtok\":999 and {\"output_per_mtok\":888} 1e9","input_per_mtok":1.25,"output_per_mtok":2.5,"source":"curated"}"#;
        let p = parse_pricing(raw, None).unwrap();
        assert_eq!(p.input_micro_per_mtok, 1_250_000);
        assert_eq!(p.output_micro_per_mtok, 2_500_000);
    }

    #[test]
    fn unrepresentable_or_malformed_prices_are_unknown_not_rounded() {
        for bad in [
            // exponent form
            r#"{"input_per_mtok":1e-7,"output_per_mtok":1}"#,
            r#"{"input_per_mtok":1E1,"output_per_mtok":1}"#,
            // more than six decimals
            r#"{"input_per_mtok":0.1234567,"output_per_mtok":1}"#,
            // negative
            r#"{"input_per_mtok":-1,"output_per_mtok":1}"#,
            // missing output
            r#"{"input_per_mtok":1}"#,
            // wrong type
            r#"{"input_per_mtok":"1","output_per_mtok":1}"#,
            // duplicate key is ambiguous
            r#"{"input_per_mtok":1,"input_per_mtok":2,"output_per_mtok":1}"#,
            // tier with both bounds, and with none
            r#"{"input_per_mtok":1,"output_per_mtok":1,"tiers":[{"prompt_tokens_up_to":1,"prompt_tokens_over":1,"input_per_mtok":1,"output_per_mtok":1}]}"#,
            r#"{"input_per_mtok":1,"output_per_mtok":1,"tiers":[{"input_per_mtok":1,"output_per_mtok":1}]}"#,
            // tier with a fractional bound
            r#"{"input_per_mtok":1,"output_per_mtok":1,"tiers":[{"prompt_tokens_over":1.5,"input_per_mtok":1,"output_per_mtok":1}]}"#,
            // not an object / not JSON / trailing garbage
            "[1,2]",
            "not json",
            r#"{"input_per_mtok":1,"output_per_mtok":1} x"#,
            "",
        ] {
            assert!(
                parse_pricing(bad, None).is_none(),
                "{bad:?} must be unknown"
            );
        }
    }

    #[test]
    fn source_follows_the_document_unless_forced() {
        let raw = r#"{"input_per_mtok":1,"output_per_mtok":1,"source":"provider_api"}"#;
        assert_eq!(
            parse_pricing(raw, None).unwrap().source,
            PriceSource::ProviderApi
        );
        assert_eq!(
            parse_pricing(raw, Some(PriceSource::User)).unwrap().source,
            PriceSource::User
        );
        let no_source = r#"{"input_per_mtok":1,"output_per_mtok":1}"#;
        assert_eq!(
            parse_pricing(no_source, None).unwrap().source,
            PriceSource::Curated
        );
    }

    #[test]
    fn override_wins_and_a_broken_override_is_unknown() {
        let over = r#"{"input_per_mtok":9,"output_per_mtok":9,"source":"user"}"#;
        match resolve_price(Some(over), Some(PLAIN)) {
            PriceQuote::Known(p) => {
                assert_eq!(p.input_micro_per_mtok, 9_000_000);
                assert_eq!(p.source, PriceSource::User);
            }
            PriceQuote::Unknown => panic!("override must be read"),
        }
        assert_eq!(
            resolve_price(Some("{broken"), Some(PLAIN)),
            PriceQuote::Unknown
        );
    }

    #[test]
    fn staleness_uses_the_ninety_day_window() {
        let p = parse_pricing(PLAIN, None).unwrap();
        assert!(!p.is_stale(d(2026, 10, 11)));
        assert!(!p.is_stale(d(2027, 1, 9))); // exactly 90 days
        assert!(p.is_stale(d(2027, 1, 10))); // 91 days
        assert_eq!(p.age_days(d(2026, 10, 21)), Some(10));
        let undated = parse_pricing(r#"{"input_per_mtok":1,"output_per_mtok":1}"#, None).unwrap();
        assert!(undated.is_stale(d(2026, 10, 11)));
    }

    #[test]
    fn a_user_price_is_never_stale() {
        let raw = r#"{"input_per_mtok":1,"output_per_mtok":1,"as_of":"2026-01-01"}"#;
        let user = parse_pricing(raw, Some(PriceSource::User)).unwrap();
        assert_eq!(user.age_days(d(2026, 10, 11)), Some(283));
        assert!(!user.is_stale(d(2026, 10, 11)));
        // The same document as a curated price is stale.
        assert!(parse_pricing(raw, None).unwrap().is_stale(d(2026, 10, 11)));
        // A provider-reported price goes stale like a curated one.
        let api = parse_pricing(raw, Some(PriceSource::ProviderApi)).unwrap();
        assert!(api.is_stale(d(2026, 10, 11)));
    }

    /// Every curated price in the shipped seed data must read exactly, or be
    /// knowingly unknown. This is the check that no seed row silently became
    /// "unknown" because of a literal the parser refuses.
    #[test]
    fn seeded_pricing_literals_read_exactly() {
        let seed = include_str!("../../schema/shared_028.sql");
        let mut checked = 0;
        for line in seed.lines() {
            let t = line.trim();
            if t.starts_with("'{\"input_per_mtok\"") {
                let end = t.rfind('}').expect("closing brace");
                // Undo SQL string-literal quote doubling before reading it.
                let json = t[1..=end].replace("''", "'");
                let json = json.as_str();
                let p = parse_pricing(json, None)
                    .unwrap_or_else(|| panic!("seed pricing must read exactly: {json}"));
                assert!(p.as_of.is_some(), "seed price must carry as_of: {json}");
                checked += 1;
            }
        }
        assert!(
            checked >= 5,
            "expected several seeded prices, saw {checked}"
        );
    }
}
