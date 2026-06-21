// src-tauri/tests/golden_vectors.rs
//
// Golden-vector parity suite for Gate1-4.
// Loads src-tauri/tests/golden/gate{1,2,3,4}.json (Python oracle output)
// and asserts that the Rust port produces bit-identical results.
//
// Gate3 and Gate4: inputs are fully stored in fixture -- driven directly.
// Gate1 and Gate2: fixture stores outputs only -- inputs reconstructed from
//   label strings using the same logic as the Python extraction script.
// apply_abstraction: all inputs stored in fixture -- driven directly.

use std::path::PathBuf;

use serde_json::Value;

use quietrabbit_lib::conductor::privacy::{
    abstraction::apply_abstraction,
    gate1::gate1,
    gate2::gate2,
    gate3::gate3,
    gate4::gate4,
    logger::{FailLogger, TestLogger},
    types::{
        AbstractionPolicy, PersonalField, PersonalTrack, Sensitivity,
    },
};

// ---------------------------------------------------------------------------
// Fixture loading
// ---------------------------------------------------------------------------

fn golden_path(filename: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(filename)
}

fn load_json(filename: &str) -> Value {
    let path = golden_path(filename);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {}", path.display(), e));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("Failed to parse {}: {}", path.display(), e))
}

// ---------------------------------------------------------------------------
// Field / track builders (mirror Python extraction script exactly)
// ---------------------------------------------------------------------------

fn sens(s: &str) -> Sensitivity {
    match s {
        "general"   => Sensitivity::General,
        "personal"  => Sensitivity::Personal,
        "medical"   => Sensitivity::Medical,
        "financial" => Sensitivity::Financial,
        other       => panic!("Unknown sensitivity: {other}"),
    }
}

fn field(name: &str, value: &str, sensitivity: &str, t2: &str, t3: &str) -> PersonalField {
    let s = sens(sensitivity);
    let sev = s.severity();
    PersonalField {
        field_name:           name.to_string(),
        field_value:          value.to_string(),
        sensitivity:          s,
        sensitivity_severity: sev,
        source_id:            "personal-specialist".to_string(),
        abstraction_tier2:    AbstractionPolicy::from_str(t2),
        abstraction_tier3:    AbstractionPolicy::from_str(t3),
    }
}

fn track(fields: Vec<PersonalField>) -> PersonalTrack {
    let mut t = PersonalTrack::new();
    for f in fields { t.add_field(f).unwrap(); }
    t.seal();
    t
}

fn empty_track() -> PersonalTrack {
    let mut t = PersonalTrack::new();
    t.seal();
    t
}

// ---------------------------------------------------------------------------
// Gate1 input table (label -> inputs)
// Reconstructs (track, abstraction_tier, raw_abstraction, execution_tier, provider)
// Must match the Python extraction script exactly.
// ---------------------------------------------------------------------------

type G1Inputs = (PersonalTrack, u8, u8, u8, Option<String>);

fn gate1_inputs(label: &str) -> G1Inputs {
    let ol = || Some("ollama".to_string());
    let gr = || Some("groq".to_string());

    match label {
        "gate1::empty_track::tier1" => (empty_track(), 1, 1, 1, None),
        "gate1::empty_track::tier2" => (empty_track(), 2, 2, 2, ol()),

        "gate1::all_pass::tier1" => (track(vec![
            field("name", "Alice",  "personal", "pass", "pass"),
            field("city", "London", "general",  "pass", "pass"),
        ]), 1, 1, 1, None),
        "gate1::all_pass::tier2" => (track(vec![
            field("name", "Alice",  "personal", "pass", "pass"),
            field("city", "London", "general",  "pass", "pass"),
        ]), 2, 2, 2, ol()),
        "gate1::all_pass::tier3" => (track(vec![
            field("name", "Alice",  "personal", "pass", "pass"),
            field("city", "London", "general",  "pass", "pass"),
        ]), 3, 3, 3, ol()),

        "gate1::all_omit::tier2" => (track(vec![
            field("secret", "value", "personal", "omit", "omit"),
        ]), 2, 2, 2, ol()),

        "gate1::not_permitted::tier2" => (track(vec![
            field("ssn", "123-45-6789", "personal", "not_permitted", "not_permitted"),
        ]), 2, 2, 2, ol()),

        "gate1::summarize::general::tier2"  => (track(vec![field("x","SomeValue","general","summarize","summarize")]), 2,2,2, ol()),
        "gate1::summarize::personal::tier2" => (track(vec![field("x","SomeValue","personal","summarize","summarize")]),2,2,2, ol()),
        "gate1::summarize::medical::tier2"  => (track(vec![field("x","SomeValue","medical","summarize","summarize")]), 2,2,2, ol()),
        "gate1::summarize::financial::tier2"=> (track(vec![field("x","SomeValue","financial","summarize","summarize")]),2,2,2,ol()),

        "gate1::range_only::50000::tier2" => (track(vec![
            field("income", "50000", "financial", "range_only", "range_only"),
        ]), 2, 2, 2, ol()),

        "gate1::ordering_stress::tier2" => (track(vec![
            field("alpha",   "AAA",   "general",   "pass",          "pass"),
            field("beta",    "BBB",   "personal",  "omit",          "omit"),
            field("gamma",   "CCC",   "medical",   "not_permitted", "not_permitted"),
            field("delta",   "DDD",   "financial", "summarize",     "summarize"),
            field("epsilon", "50000", "financial", "range_only",    "range_only"),
            field("zeta",    "ZZZ",   "general",   "pass",          "pass"),
        ]), 2, 2, 2, gr()),

        "gate1::ordering_interleaved_omits::tier2" => (track(vec![
            field("a","AAA","general","pass","pass"),
            field("b","BBB","personal","omit","omit"),
            field("c","CCC","general","pass","pass"),
            field("d","DDD","personal","omit","omit"),
            field("e","EEE","general","pass","pass"),
        ]), 2, 2, 2, ol()),

        "gate1::fields_shared_accuracy::tier2" => (track(vec![
            field("passval","RawPassVal","general","pass","pass"),
            field("sumval","50000","financial","range_only","range_only"),
        ]), 2, 2, 2, ol()),

        l if l.starts_with("gate1::floor_clamp::") => {
            let parts: Vec<&str> = l.split("::").collect();
            let re_str  = parts[2];   // "raw1_eff2"
            let policy  = parts[3];
            let raw: u8 = re_str.split('_').next().unwrap()
                .trim_start_matches("raw").parse().unwrap();
            let eff: u8 = re_str.split('_').nth(1).unwrap()
                .trim_start_matches("eff").parse().unwrap();
            let value = if policy == "range_only" { "50000" } else { "TestVal" };
            let snss  = if policy == "range_only" { "financial" } else { "personal" };
            let prov  = if eff > 1 { ol() } else { None };
            (track(vec![field("fc_field", value, snss, policy, policy)]),
             eff, raw, eff, prov)
        }

        // Failure vectors handled separately in test_gate1_disclosure_failure
        l if l.contains("disclosure_log_failure") => {
            (track(vec![field("x","y","general","pass","pass")]),
             if l.contains("tier1") { 1 } else { 2 },
             if l.contains("tier1") { 1 } else { 2 },
             if l.contains("tier1") { 1 } else { 2 },
             if l.contains("tier2") { ol() } else { None })
        }

        other => panic!("No input constructor for gate1 label: {other}"),
    }
}

// ---------------------------------------------------------------------------
// Gate2 input table (label -> response + track + fields_shared)
// ---------------------------------------------------------------------------

fn nfc(s: &str) -> String { unicode_normalization::UnicodeNormalization::nfc(s).collect() }
fn nfd(s: &str) -> String { unicode_normalization::UnicodeNormalization::nfd(s).collect() }

type G2Inputs = (String, PersonalTrack, Option<Vec<String>>);

fn gate2_inputs(label: &str) -> G2Inputs {
    let alice_t  = || track(vec![field("name", "Alice", "personal", "pass", "pass")]);
    let german_t = || track(vec![field("street", "Stra\u{00df}e", "personal", "pass", "pass")]);

    match label {
        "gate2::no_match" =>
            ("The weather is nice today.".into(), alice_t(), None),
        "gate2::exact_match_ascii" =>
            ("Hello Alice, how are you?".into(), alice_t(), None),
        "gate2::case_insensitive_ascii" =>
            ("hello alice, how are you?".into(), alice_t(), None),
        "gate2::below_min_length_3" =>
            ("The code is ABC here.".into(),
             track(vec![field("code","ABC","personal","pass","pass")]), None),
        "gate2::at_min_length_match" =>
            ("The code is ABCD here.".into(),
             track(vec![field("code","ABCD","personal","pass","pass")]), None),
        "gate2::at_min_length_no_match" =>
            ("The code is EFGH here.".into(),
             track(vec![field("code","ABCD","personal","pass","pass")]), None),

        "gate2::fields_shared_exclusion" => (
            "Alice is here and Secret99 too.".into(),
            track(vec![
                field("shared_field","Alice","personal","pass","pass"),
                field("private_field","Secret99","personal","pass","pass"),
            ]),
            Some(vec!["shared_field".to_string()]),
        ),
        "gate2::fields_shared_none_excluded" => (
            "Alice is here and Secret99 too.".into(),
            track(vec![
                field("shared_field","Alice","personal","pass","pass"),
                field("private_field","Secret99","personal","pass","pass"),
            ]),
            Some(vec![]),
        ),
        "gate2::fields_shared_null_compat" => (
            "Alice is here and Secret99 too.".into(),
            track(vec![
                field("shared_field","Alice","personal","pass","pass"),
                field("private_field","Secret99","personal","pass","pass"),
            ]),
            None,
        ),

        // Unicode vectors
        "gate2::unicode_turkish_upper_I_exact" =>
            (format!("Visit \u{0130}stanbul today."),
             track(vec![field("city","\u{0130}stanbul","personal","pass","pass")]), None),
        "gate2::unicode_turkish_lower_i_folded" =>
            ("visit istanbul today.".into(),
             track(vec![field("city","\u{0130}stanbul","personal","pass","pass")]), None),
        "gate2::unicode_turkish_dotless_i" =>
            ("visit \u{0131}stanbul today.".into(),
             track(vec![field("city","\u{0130}stanbul","personal","pass","pass")]), None),
        "gate2::unicode_german_exact" =>
            ("Lives on Stra\u{00df}e now.".into(), german_t(), None),
        "gate2::unicode_german_lower" =>
            ("lives on stra\u{00df}e now.".into(), german_t(), None),
        "gate2::unicode_german_ss_upper" =>
            ("LIVES ON STRASSE NOW.".into(), german_t(), None),

        "gate2::unicode_nfc_field_nfc_response" => {
            let v = nfc("caf\u{00e9}");
            (format!("We visited {v}."),
             track(vec![field("place",&v,"personal","pass","pass")]), None)
        }
        "gate2::unicode_nfd_field_nfc_response" => {
            let vf = nfd("caf\u{00e9}");
            let vr = nfc("caf\u{00e9}");
            (format!("We visited {vr}."),
             track(vec![field("place",&vf,"personal","pass","pass")]), None)
        }
        "gate2::unicode_nfc_field_nfd_response" => {
            let vf = nfc("caf\u{00e9}");
            let vr = nfd("caf\u{00e9}");
            (format!("We visited {vr}."),
             track(vec![field("place",&vf,"personal","pass","pass")]), None)
        }
        "gate2::unicode_nfd_field_nfd_response" => {
            let v = nfd("caf\u{00e9}");
            (format!("We visited {v}."),
             track(vec![field("place",&v,"personal","pass","pass")]), None)
        }

        // Overlap / duplicate vectors
        "gate2::overlap_value_repeated" =>
            ("AliceAlice is the name.".into(), alice_t(), None),
        "gate2::overlap_substring_of_longer" => (
            "Alice Smith was here.".into(),
            track(vec![
                field("short","Alice","personal","pass","pass"),
                field("long","Alice Smith","personal","pass","pass"),
            ]), None),
        "gate2::overlap_only_short_present" => (
            "Alice was here.".into(),
            track(vec![
                field("short","Alice","personal","pass","pass"),
                field("long","Alice Smith","personal","pass","pass"),
            ]), None),
        "gate2::duplicate_values_match" => (
            "Bob is mentioned.".into(),
            track(vec![
                field("field_a","Bob","personal","pass","pass"),
                field("field_b","Bob","personal","pass","pass"),
            ]), None),
        "gate2::duplicate_values_no_match" => (
            "Nobody mentioned.".into(),
            track(vec![
                field("field_a","Bob","personal","pass","pass"),
                field("field_b","Bob","personal","pass","pass"),
            ]), None),

        // Multi-field
        "gate2::multi_field_partial_match" => (
            "Alice was here.".into(),
            track(vec![
                field("first","Alice","personal","pass","pass"),
                field("second","Bob","personal","pass","pass"),
                field("third","Charlie","personal","pass","pass"),
            ]), None),
        "gate2::multi_field_all_match" => (
            "Alice and Bob and Charlie.".into(),
            track(vec![
                field("first","Alice","personal","pass","pass"),
                field("second","Bob","personal","pass","pass"),
                field("third","Charlie","personal","pass","pass"),
            ]), None),
        "gate2::multi_field_no_match" => (
            "Nobody mentioned.".into(),
            track(vec![
                field("first","Alice","personal","pass","pass"),
                field("second","Bob","personal","pass","pass"),
                field("third","Charlie","personal","pass","pass"),
            ]), None),

        "gate2::disclosure_log_on_flag_tier2" =>
            ("Hello Alice.".into(), alice_t(), None),

        other => panic!("No input constructor for gate2 label: {other}"),
    }
}

// ---------------------------------------------------------------------------
// Test: apply_abstraction valid
// ---------------------------------------------------------------------------

#[test]
fn test_apply_abstraction_valid() {
    let root    = load_json("gate1.json");
    let vectors = root["apply_abstraction_valid"].as_array()
        .expect("apply_abstraction_valid must be array");

    for v in vectors {
        let label       = v["label"].as_str().unwrap();
        let field_name  = v["field_name"].as_str().unwrap();
        let field_value = v["field_value"].as_str().unwrap();
        let sensitivity = v["sensitivity"].as_str().unwrap();
        let t2          = v["policy_t2"].as_str().unwrap();
        let t3          = v["policy_t3"].as_str().unwrap();
        let tier        = v["tier"].as_u64().unwrap() as u8;
        let expected: Option<String> = match &v["output"] {
            Value::Null      => None,
            Value::String(s) => Some(s.clone()),
            other            => panic!("{label}: unexpected output type {other:?}"),
        };

        let f      = field(field_name, field_value, sensitivity, t2, t3);
        let result = apply_abstraction(&f, tier);

        assert_eq!(result, expected,
            "MISMATCH [{label}]\n  tier={tier} t2={t2} t3={t3} val={field_value:?}\n  \
             rust={result:?}\n  python={expected:?}");
    }
}

// ---------------------------------------------------------------------------
// Test: apply_abstraction invalid (Unknown policy fail-safe)
// ---------------------------------------------------------------------------

#[test]
fn test_apply_abstraction_invalid() {
    let root    = load_json("gate1.json");
    let vectors = root["apply_abstraction_invalid"].as_array()
        .expect("apply_abstraction_invalid must be array");

    for v in vectors {
        let label           = v["label"].as_str().unwrap();
        let field_value     = v["field_value"].as_str().unwrap();
        let policy_injected = v["policy_injected"].as_str().unwrap();
        let tier            = v["tier"].as_u64().unwrap() as u8;
        let expected: Option<String> = match &v["output"] {
            Value::Null      => None,
            Value::String(s) => Some(s.clone()),
            other            => panic!("{label}: unexpected output type {other:?}"),
        };

        let f = PersonalField {
            field_name:           "test_field".to_string(),
            field_value:          field_value.to_string(),
            sensitivity:          Sensitivity::General,
            sensitivity_severity: 1,
            source_id:            "personal-specialist".to_string(),
            abstraction_tier2:    AbstractionPolicy::Unknown(policy_injected.to_string()),
            abstraction_tier3:    AbstractionPolicy::Unknown(policy_injected.to_string()),
        };
        let result = apply_abstraction(&f, tier);

        assert_eq!(result, expected,
            "MISMATCH [{label}] tier={tier} policy={policy_injected:?}\n  \
             rust={result:?}  python={expected:?}");
    }
}

// ---------------------------------------------------------------------------
// Test: gate1 normal paths (all non-failure vectors)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_gate1_normal_paths() {
    let root    = load_json("gate1.json");
    let vectors = root["gate1"].as_array().expect("gate1 must be array");

    // Counter for deterministic step/run IDs (matches Python extraction script)
    let mut step = 0u32;
    let mut run  = 0u32;

    for v in vectors {
        let label = v["label"].as_str().unwrap();

        // Failure vectors are tested separately
        if label.contains("disclosure_log_failure") { step+=1; run+=1; continue; }

        step += 1; run += 1;
        let step_id = format!("step-{step:04}");
        let run_id  = format!("run-{run:04}");

        let (trk, abstraction_tier, raw_abstraction, execution_tier, provider) =
            gate1_inputs(label);

        let logger = TestLogger::new();
        let result = gate1(
            &logger, &step_id, &run_id,
            &trk, abstraction_tier, raw_abstraction, execution_tier, provider,
        ).await.unwrap_or_else(|e| panic!("{label}: gate1 returned Err: {e}"));

        // Assert approved_fields (ordered pairs)
        let expected_approved: Vec<(String, String)> = v["approved_fields"]
            .as_array().unwrap()
            .iter()
            .map(|pair| {
                let arr = pair.as_array().unwrap();
                (arr[0].as_str().unwrap().to_string(),
                 arr[1].as_str().unwrap().to_string())
            })
            .collect();
        let rust_approved: Vec<(String, String)> = result.approved_fields
            .iter()
            .map(|(k,v)| (k.clone(), v.clone()))
            .collect();
        assert_eq!(rust_approved, expected_approved,
            "approved_fields MISMATCH [{label}]\n  rust={rust_approved:?}\n  python={expected_approved:?}");

        // Assert withheld_fields
        let expected_withheld: Vec<String> = v["withheld_fields"]
            .as_array().unwrap()
            .iter().map(|x| x.as_str().unwrap().to_string()).collect();
        assert_eq!(result.withheld_fields, expected_withheld,
            "withheld_fields MISMATCH [{label}]");

        // Assert fields_shared
        let expected_shared: Vec<String> = v["fields_shared"]
            .as_array().unwrap()
            .iter().map(|x| x.as_str().unwrap().to_string()).collect();
        assert_eq!(result.fields_shared, expected_shared,
            "fields_shared MISMATCH [{label}]");

        // Assert floor_clamped_fields
        let expected_clamped: Vec<String> = v["floor_clamped_fields"]
            .as_array().unwrap()
            .iter().map(|x| x.as_str().unwrap().to_string()).collect();
        assert_eq!(result.floor_clamped_fields, expected_clamped,
            "floor_clamped_fields MISMATCH [{label}]");

        // Assert blocked
        assert_eq!(result.blocked, v["blocked"].as_bool().unwrap(),
            "blocked MISMATCH [{label}]");

        // Assert disclosure log written (TestLogger recorded exactly one entry)
        assert_eq!(logger.entry_count(), 1,
            "Expected 1 disclosure log write [{label}], got {}", logger.entry_count());

        // Verify entry contents (event_type, execution_tier)
        let entry = &logger.entries()[0];
        assert_eq!(entry.event_type, "gate1_pass", "event_type MISMATCH [{label}]");
        assert_eq!(entry.execution_tier, execution_tier,
            "disclosure entry execution_tier MISMATCH [{label}]");
    }
}

// ---------------------------------------------------------------------------
// Test: gate1 disclosure log failure paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_gate1_disclosure_failure_tier1_nonfatal() {
    let simple_track = track(vec![field("x","y","general","pass","pass")]);
    let logger       = FailLogger;

    // Tier 1: must NOT raise DisclosureLogWriteError -- returns Ok with empty log id
    let result = gate1(&logger, "step-fail-t1", "run-fail-t1",
        &simple_track, 1, 1, 1, None).await;

    assert!(result.is_ok(),
        "Tier 1 disclosure failure must be non-fatal but got Err: {:?}", result.err());
    assert_eq!(result.unwrap().disclosure_log_id, "",
        "Non-fatal tier 1 failure must return empty log id");
}

#[tokio::test]
async fn test_gate1_disclosure_failure_tier2_fatal() {
    let simple_track = track(vec![field("x","y","general","pass","pass")]);
    let logger       = FailLogger;

    // Tier 2: must raise DisclosureLogWriteError
    let result = gate1(&logger, "step-fail-t2", "run-fail-t2",
        &simple_track, 2, 2, 2, Some("ollama".into())).await;

    assert!(result.is_err(),
        "Tier 2 disclosure failure must be fatal but got Ok");
    let err = result.unwrap_err();
    assert!(!err.plain_language.is_empty(),
        "DisclosureLogWriteError must carry plain_language");
}

// ---------------------------------------------------------------------------
// Test: gate2
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_gate2() {
    let root    = load_json("gate2.json");
    let vectors = root.as_array().expect("gate2.json must be array");

    for v in vectors {
        let label          = v["label"].as_str().unwrap();
        let expected_flagged = v["flagged"].as_bool().unwrap();
        let expected_matched: Vec<String> = v["matched_field_names"]
            .as_array().unwrap()
            .iter().map(|x| x.as_str().unwrap().to_string()).collect();

        // execution_tier: 2 only for disclosure_log_on_flag vector
        let execution_tier: u8 = if label.contains("tier2") { 2 } else { 1 };

        let (response, trk, fields_shared) = gate2_inputs(label);
        let fields_shared_ref: Option<&[String]> =
            fields_shared.as_ref().map(|v| v.as_slice());

        let logger = TestLogger::new();
        let result = gate2(
            &logger, &format!("step-{label}"), &format!("run-{label}"),
            &response, &trk, execution_tier, None, fields_shared_ref,
        ).await.unwrap_or_else(|e| panic!("{label}: gate2 returned Err: {e}"));

        assert_eq!(result.flagged, expected_flagged,
            "flagged MISMATCH [{label}]  rust={} python={}", result.flagged, expected_flagged);
        assert_eq!(result.matched_field_names, expected_matched,
            "matched_field_names MISMATCH [{label}]\n  rust={:?}\n  python={:?}",
            result.matched_field_names, expected_matched);

        // Disclosure log written iff flagged
        if expected_flagged {
            assert_eq!(logger.entry_count(), 1,
                "Expected disclosure log write when flagged [{label}]");
            assert_eq!(logger.entries()[0].event_type, "gate2_contamination_detected",
                "event_type MISMATCH [{label}]");
        } else {
            assert_eq!(logger.entry_count(), 0,
                "Expected NO disclosure log write when not flagged [{label}]");
        }
    }
}

// ---------------------------------------------------------------------------
// Test: gate3 (inputs fully in fixture)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_gate3() {
    let root    = load_json("gate3.json");
    let vectors = root.as_array().expect("gate3.json must be array");

    for v in vectors {
        let label         = v["label"].as_str().unwrap();
        let severity      = v["content_sensitivity_severity"].as_u64().unwrap() as u8;
        let target_tier   = v["target_tier"].as_u64().unwrap() as u8;
        let space_max     = v["space_max_permitted_tier"].as_u64().unwrap() as u8;
        let execution_tier: u8 = 2;   // All gate3 vectors use tier 2

        let expected_approved = v["approved"].as_bool().unwrap();
        let expected_blocked  = v["blocked"].as_bool().unwrap();
        let expected_event    = v["event_type"].as_str().unwrap();

        let logger = TestLogger::new();
        let result = gate3(
            &logger, &format!("step-{label}"), &format!("run-{label}"),
            "test_content_key", severity, target_tier, space_max, execution_tier,
        ).await.unwrap_or_else(|e| panic!("{label}: gate3 returned Err: {e}"));

        assert_eq!(result.approved, expected_approved,
            "approved MISMATCH [{label}]");
        assert_eq!(result.blocked, expected_blocked,
            "blocked MISMATCH [{label}]");

        // Disclosure log always written for gate3
        assert_eq!(logger.entry_count(), 1,
            "Expected 1 disclosure log write [{label}]");

        // Event type must match oracle -- proves ceiling vs sensitivity ordering
        assert_eq!(logger.entries()[0].event_type, expected_event,
            "event_type MISMATCH [{label}]\n  rust={}\n  python={}",
            logger.entries()[0].event_type, expected_event);

        // Blocked results must have plain_language
        if expected_blocked {
            assert!(result.plain_language.is_some(),
                "Blocked gate3 must have plain_language [{label}]");
        } else {
            assert!(result.plain_language.is_none(),
                "Approved gate3 must not have plain_language [{label}]");
        }
    }
}

// ---------------------------------------------------------------------------
// Test: gate4 (inputs fully in fixture)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_gate4() {
    let root    = load_json("gate4.json");
    let vectors = root.as_array().expect("gate4.json must be array");

    for v in vectors {
        let label              = v["label"].as_str().unwrap();
        let severity           = v["content_sensitivity_severity"].as_u64().unwrap() as u8;
        let expected_approved  = v["content_approved"].as_bool().unwrap();
        let expected_clipboard = v["clipboard_blocked"].as_bool().unwrap();

        let logger = TestLogger::new();
        let result = gate4(
            &logger, &format!("step-{label}"), &format!("run-{label}"),
            severity, 3,
        ).await.unwrap_or_else(|e| panic!("{label}: gate4 returned Err: {e}"));

        assert_eq!(result.content_approved, expected_approved,
            "content_approved MISMATCH [{label}]");
        assert_eq!(result.clipboard_blocked, expected_clipboard,
            "clipboard_blocked MISMATCH [{label}]");

        // Always writes disclosure log
        assert_eq!(logger.entry_count(), 1,
            "Expected 1 disclosure log write [{label}]");
        assert_eq!(logger.entries()[0].event_type, "gate4_stub_validation",
            "event_type MISMATCH [{label}]");

        // plain_language present iff clipboard_blocked
        if expected_clipboard {
            assert!(result.plain_language.is_some(),
                "Clipboard-blocked gate4 must have plain_language [{label}]");
        } else {
            assert!(result.plain_language.is_none(),
                "Non-blocked gate4 must not have plain_language [{label}]");
        }
    }
}
