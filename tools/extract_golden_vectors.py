#!/usr/bin/env python3
# tools/extract_golden_vectors.py
#
# Golden-vector extraction for Gate1-4 (privacy.py).
# Invokes the REAL PrivacyGateway methods against a live temporary SQLCipher
# personal.db that auto-migrates via the real open_personal_db / migrate_personal_db.
#
# Output: src-tauri/tests/golden/gate{1,2,3,4}.json
#
# Usage (from repo root):
#   QR_DATA_ROOT=/tmp/qr_gv python tools/extract_golden_vectors.py
#
# The script is destructive to /tmp/qr_gv -- safe to re-run.

from __future__ import annotations

import json
import os
import shutil
import sys
import unicodedata
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO))

TMP_ROOT   = Path("/tmp/qr_gv")
KEY_HEX    = "00" * 32   # 32-byte zero key -- test-only, good gateway
BAD_KEY    = "ff" * 32   # wrong key on a zero-keyed DB -> SQLCipher error
USER_ID    = "gv-user"
PERSONA_ID = "gv-persona"

os.environ["QR_DATA_ROOT"]       = str(TMP_ROOT)
os.environ["QR_NETWORK_STORAGE"] = "false"

from conductor.context import PersonalField, PersonalTrack
from conductor.privacy import PrivacyGateway, _apply_abstraction
from providers.errors import DisclosureLogWriteError
from providers.utils import open_personal_db
from sqlcipher3 import dbapi2 as _sq3

OUT_DIR = REPO / "src-tauri" / "tests" / "golden"

# -- Deterministic ID counters ------------------------------------------------
_step_counter = 0
_run_counter  = 0

def sid() -> str:
    global _step_counter
    _step_counter += 1
    return f"step-{_step_counter:04d}"

def rid() -> str:
    global _run_counter
    _run_counter += 1
    return f"run-{_run_counter:04d}"

# -- Good gateway (real DB, correct key) --------------------------------------
def gw() -> PrivacyGateway:
    return PrivacyGateway(user_id=USER_ID, persona_id=PERSONA_ID, key_hex=KEY_HEX)

# -- Bad gateway (real DB path, WRONG key -> SQLCipher rejects every write) ---
def broken_gw() -> PrivacyGateway:
    """
    Same DB path as gw() -- DB was initialised with KEY_HEX.
    BAD_KEY causes SQLCipher to reject the connection, so _write_disclosure_log
    always throws. This is deterministic: wrong-key on an existing encrypted DB
    cannot silently succeed.
    """
    return PrivacyGateway(user_id=USER_ID, persona_id=PERSONA_ID, key_hex=BAD_KEY)

def _ensure_good_db_exists() -> None:
    """
    Force the real personal.db to be created with KEY_HEX before any broken_gw()
    call. open_personal_db auto-migrates on first open.
    """
    with open_personal_db(USER_ID, PERSONA_ID, KEY_HEX):
        pass

# -- Disclosure-log DB verification -------------------------------------------
def _db_path() -> Path:
    return TMP_ROOT / "users" / USER_ID / "personas" / PERSONA_ID / "personal.db"

def _log_row_count() -> int:
    """Return total rows in disclosure_log for the test DB."""
    conn = _sq3.connect(str(_db_path()))
    conn.execute(f"PRAGMA key = \"x'{KEY_HEX}'\"")
    row = conn.execute("SELECT COUNT(*) FROM disclosure_log").fetchone()
    conn.close()
    return row[0]

def _last_event_type() -> str:
    """Return the event_type of the most recently written disclosure_log row."""
    conn = _sq3.connect(str(_db_path()))
    conn.execute(f"PRAGMA key = \"x'{KEY_HEX}'\"")
    row = conn.execute(
        "SELECT extra_metadata FROM disclosure_log ORDER BY created_at DESC LIMIT 1"
    ).fetchone()
    conn.close()
    if row is None:
        return "none"
    return json.loads(row[0]).get("event_type", "unknown")

# -- Field / track builders ---------------------------------------------------
SEVERITY = {"general": 1, "personal": 2, "medical": 3, "financial": 4}

def make_field(name, value, sensitivity="general", t2="pass", t3="pass") -> PersonalField:
    return PersonalField(
        field_name=name,
        field_value=value,
        sensitivity=sensitivity,
        sensitivity_severity=SEVERITY[sensitivity],
        source_id="personal-specialist",
        abstraction_tier2=t2,
        abstraction_tier3=t3,
    )

def make_track(*fields: PersonalField) -> PersonalTrack:
    t = PersonalTrack()
    for f in fields:
        t.add_field(f)
    t.seal()
    return t

# -- Result serialisers -------------------------------------------------------
def g1_dict(r, db_rows_before: int) -> dict:
    return {
        "approved_fields":            list(r.approved_fields.items()),
        "withheld_fields":            r.withheld_fields,
        "fields_shared":              r.fields_shared,
        "floor_clamped_fields":       r.floor_clamped_fields,
        "blocked":                    r.blocked,
        "disclosure_log_id_set":      bool(r.disclosure_log_id),
        "disclosure_log_row_written": _log_row_count() > db_rows_before,
    }

def g2_dict(r) -> dict:
    return {
        "flagged":             r.flagged,
        "matched_field_names": r.matched_field_names,
    }

def g3_dict(r, event_type: str) -> dict:
    return {
        "approved":      r.approved,
        "blocked":       r.blocked,
        "plain_language": r.plain_language,
        "event_type":    event_type,
    }

def g4_dict(r) -> dict:
    return {
        "content_approved":  r.content_approved,
        "clipboard_blocked": r.clipboard_blocked,
        "plain_language":    r.plain_language,
    }


# =============================================================================
# SECTION 1 -- _apply_abstraction (pure function)
# =============================================================================

def extract_apply_abstraction_valid() -> list[dict]:
    """Valid production policy x tier x sensitivity combinations."""
    vectors = []
    POLICIES      = ["pass", "omit", "not_permitted", "summarize", "range_only"]
    SENSITIVITIES = ["general", "personal", "medical", "financial"]

    for policy in POLICIES:
        for sensitivity in SENSITIVITIES:
            for tier in [1, 2, 3]:
                f = make_field("test_field", "TestValue123",
                               sensitivity=sensitivity, t2=policy, t3=policy)
                vectors.append({
                    "label":       f"valid::{policy}::{sensitivity}::tier{tier}",
                    "field_name":  "test_field",
                    "field_value": "TestValue123",
                    "sensitivity": sensitivity,
                    "policy_t2":   policy,
                    "policy_t3":   policy,
                    "tier":        tier,
                    "output":      _apply_abstraction(f, tier),
                })

    # summarize -- sensitivity label governs output text; record all four
    for sensitivity in SENSITIVITIES:
        f = make_field("my_field", "anything", sensitivity=sensitivity,
                       t2="summarize", t3="summarize")
        for tier in [2, 3]:
            vectors.append({
                "label":       f"valid::summarize_label::{sensitivity}::tier{tier}",
                "field_name":  "my_field",
                "field_value": "anything",
                "sensitivity": sensitivity,
                "policy_t2":   "summarize",
                "policy_t3":   "summarize",
                "tier":        tier,
                "output":      _apply_abstraction(f, tier),
            })

    # range_only -- let Python produce the answer; no expectations stated
    range_cases = [
        ("integer_5000",       "5000",        "financial"),
        ("integer_4999",       "4999",        "financial"),
        ("integer_5001",       "5001",        "financial"),
        ("integer_999",        "999",         "financial"),
        ("integer_1000",       "1000",        "financial"),
        ("integer_1",          "1",           "financial"),
        ("integer_0",          "0",           "financial"),
        ("negative",           "-1000",       "financial"),
        ("decimal_4999_9",     "4999.9",      "financial"),
        ("decimal_5000_1",     "5000.1",      "financial"),
        ("decimal_0_5",        "0.5",         "financial"),
        ("large_100000",       "100000",      "financial"),
        ("large_99999",        "99999",       "financial"),
        ("commas",             "50,000",      "financial"),
        ("currency_usd",       "$50000",      "financial"),
        ("currency_gbp",       "\u00a350000", "financial"),
        ("currency_comma_usd", "$50,000",     "financial"),
        ("whitespace",         "  5000  ",    "financial"),
        ("non_numeric",        "unknown",     "financial"),
        ("empty_string",       "",            "financial"),
        ("boundary_6250",      "6250",        "financial"),
        ("boundary_6251",      "6251",        "financial"),
        ("boundary_4999_5",    "4999.5",      "financial"),
        ("boundary_5000_5",    "5000.5",      "financial"),
    ]
    for label, value, sensitivity in range_cases:
        for tier in [2, 3]:
            f = make_field("income", value, sensitivity=sensitivity,
                           t2="range_only", t3="range_only")
            vectors.append({
                "label":       f"valid::range_only::{label}::tier{tier}",
                "field_name":  "income",
                "field_value": value,
                "sensitivity": sensitivity,
                "policy_t2":   "range_only",
                "policy_t3":   "range_only",
                "tier":        tier,
                "output":      _apply_abstraction(f, tier),
            })

    return vectors


def extract_apply_abstraction_invalid() -> list[dict]:
    """
    Invariant-violation vectors: unknown policy strings injected via
    object.__setattr__ to bypass the Literal type hint (unenforced at runtime).
    These are NOT valid production states. They document the fail-safe
    (unknown -> None / omit) for Rust defensive porting.
    Kept separate from valid vectors -- do not mix.
    """
    vectors = []
    for tier in [1, 2, 3]:
        f = make_field("test_field", "TestValue123", "general")
        object.__setattr__(f, "abstraction_tier2", "unknown_policy")
        object.__setattr__(f, "abstraction_tier3", "unknown_policy")
        vectors.append({
            "label":            f"invalid::unknown_policy::tier{tier}",
            "note":             "invariant-violation: policy string not in allowed set",
            "field_name":       "test_field",
            "field_value":      "TestValue123",
            "policy_injected":  "unknown_policy",
            "tier":             tier,
            "output":           _apply_abstraction(f, tier),
        })
    return vectors


# =============================================================================
# SECTION 2 -- Gate1
# =============================================================================

def extract_gate1() -> list[dict]:
    vectors = []
    gateway = gw()

    def run(label, track, abstraction_tier, raw_abstraction, execution_tier,
            provider=None) -> dict:
        before = _log_row_count()
        r = gateway.gate1(
            step_id=sid(), focus_run_id=rid(),
            personal_track=track,
            abstraction_tier=abstraction_tier,
            raw_abstraction=raw_abstraction,
            execution_tier=execution_tier,
            provider=provider,
        )
        d = g1_dict(r, before)
        d["label"]            = label
        d["abstraction_tier"] = abstraction_tier
        d["raw_abstraction"]  = raw_abstraction
        d["execution_tier"]   = execution_tier
        return d

    # D5-073: empty PersonalTrack still writes disclosure log
    vectors.append(run("gate1::empty_track::tier1",
        make_track(), 1, 1, 1))
    vectors.append(run("gate1::empty_track::tier2",
        make_track(), 2, 2, 2, provider="ollama"))

    # All-pass at each tier
    all_pass = make_track(
        make_field("name", "Alice",  "personal", t2="pass", t3="pass"),
        make_field("city", "London", "general",  t2="pass", t3="pass"),
    )
    for tier in [1, 2, 3]:
        vectors.append(run(f"gate1::all_pass::tier{tier}",
            all_pass, tier, tier, tier,
            provider="ollama" if tier > 1 else None))

    # All-omit
    vectors.append(run("gate1::all_omit::tier2",
        make_track(make_field("secret", "value", "personal", t2="omit", t3="omit")),
        2, 2, 2, provider="ollama"))

    # not_permitted -- withheld, not a hard block
    vectors.append(run("gate1::not_permitted::tier2",
        make_track(make_field("ssn", "123-45-6789", "personal",
                              t2="not_permitted", t3="not_permitted")),
        2, 2, 2, provider="ollama"))

    # summarize x all sensitivities
    for sensitivity in ["general", "personal", "medical", "financial"]:
        vectors.append(run(f"gate1::summarize::{sensitivity}::tier2",
            make_track(make_field("x", "SomeValue", sensitivity,
                                  t2="summarize", t3="summarize")),
            2, 2, 2, provider="ollama"))

    # range_only
    vectors.append(run("gate1::range_only::50000::tier2",
        make_track(make_field("income", "50000", "financial",
                              t2="range_only", t3="range_only")),
        2, 2, 2, provider="ollama"))

    # Ordering stress: 6 fields, all policy types, verify approved_fields order
    vectors.append(run("gate1::ordering_stress::tier2",
        make_track(
            make_field("alpha",   "AAA",   "general",   t2="pass",          t3="pass"),
            make_field("beta",    "BBB",   "personal",  t2="omit",          t3="omit"),
            make_field("gamma",   "CCC",   "medical",   t2="not_permitted", t3="not_permitted"),
            make_field("delta",   "DDD",   "financial", t2="summarize",     t3="summarize"),
            make_field("epsilon", "50000", "financial", t2="range_only",    t3="range_only"),
            make_field("zeta",    "ZZZ",   "general",   t2="pass",          t3="pass"),
        ),
        2, 2, 2, provider="groq"))

    # Ordering: omitted fields must not shift positions of survivors
    vectors.append(run("gate1::ordering_interleaved_omits::tier2",
        make_track(
            make_field("a", "AAA", "general",  t2="pass", t3="pass"),
            make_field("b", "BBB", "personal", t2="omit", t3="omit"),
            make_field("c", "CCC", "general",  t2="pass", t3="pass"),
            make_field("d", "DDD", "personal", t2="omit", t3="omit"),
            make_field("e", "EEE", "general",  t2="pass", t3="pass"),
        ),
        2, 2, 2, provider="ollama"))

    # fields_shared accuracy: pass-through raw value vs transformed value
    vectors.append(run("gate1::fields_shared_accuracy::tier2",
        make_track(
            make_field("passval", "RawPassVal", "general",   t2="pass",       t3="pass"),
            make_field("sumval",  "50000",      "financial", t2="range_only", t3="range_only"),
        ),
        2, 2, 2, provider="ollama"))

    # Floor-clamp matrix: raw x effective x policy
    for raw, eff in [(1, 2), (1, 3), (2, 3)]:
        for policy in ["pass", "omit", "not_permitted", "summarize", "range_only"]:
            value       = "50000"    if policy == "range_only" else "TestVal"
            sensitivity = "financial" if policy == "range_only" else "personal"
            vectors.append(run(
                f"gate1::floor_clamp::raw{raw}_eff{eff}::{policy}",
                make_track(make_field("fc_field", value, sensitivity,
                                      t2=policy, t3=policy)),
                abstraction_tier=eff, raw_abstraction=raw, execution_tier=eff,
                provider="ollama" if eff > 1 else None,
            ))

    # Disclosure log failure -- Tier1 non-fatal (wrong key, should not raise)
    bgw = broken_gw()
    try:
        bgw.gate1(
            step_id=sid(), focus_run_id=rid(),
            personal_track=make_track(make_field("x", "y", "general")),
            abstraction_tier=1, raw_abstraction=1, execution_tier=1,
        )
        vectors.append({
            "label":          "gate1::disclosure_log_failure::tier1_nonfatal",
            "execution_tier": 1,
            "outcome":        "nonfatal_continue",
        })
    except DisclosureLogWriteError:
        vectors.append({
            "label":          "gate1::disclosure_log_failure::tier1_nonfatal",
            "execution_tier": 1,
            "outcome":        "ERROR_unexpected_fatal",
        })

    # Disclosure log failure -- Tier2 FATAL (wrong key, must raise)
    bgw2 = broken_gw()
    try:
        bgw2.gate1(
            step_id=sid(), focus_run_id=rid(),
            personal_track=make_track(make_field("x", "y", "general")),
            abstraction_tier=2, raw_abstraction=2, execution_tier=2,
            provider="ollama",
        )
        vectors.append({
            "label":          "gate1::disclosure_log_failure::tier2_fatal",
            "execution_tier": 2,
            "outcome":        "ERROR_should_have_raised",
        })
    except DisclosureLogWriteError as e:
        vectors.append({
            "label":                 "gate1::disclosure_log_failure::tier2_fatal",
            "execution_tier":        2,
            "outcome":               "raised_DisclosureLogWriteError",
            "plain_language_present": bool(e.plain_language),
        })

    return vectors


# =============================================================================
# SECTION 3 -- Gate2
# =============================================================================

def extract_gate2() -> list[dict]:
    vectors = []
    gateway = gw()

    def run(label, response, track, execution_tier=1, fields_shared=None) -> dict:
        r = gateway.gate2(
            step_id=sid(), focus_run_id=rid(),
            response_content=response,
            personal_track=track,
            execution_tier=execution_tier,
            provider="ollama" if execution_tier > 1 else None,
            fields_shared=fields_shared,
        )
        d = g2_dict(r)
        d["label"]            = label
        d["response_snippet"] = response[:80]
        return d

    # No match
    vectors.append(run("gate2::no_match",
        "The weather is nice today.",
        make_track(make_field("name", "Alice", "personal"))))

    # Exact match ASCII
    vectors.append(run("gate2::exact_match_ascii",
        "Hello Alice, how are you?",
        make_track(make_field("name", "Alice", "personal"))))

    # Case-insensitive ASCII
    vectors.append(run("gate2::case_insensitive_ascii",
        "hello alice, how are you?",
        make_track(make_field("name", "Alice", "personal"))))

    # Below MIN_MATCH_LENGTH=4 -- observe whether Python scans it
    vectors.append(run("gate2::below_min_length_3",
        "The code is ABC here.",
        make_track(make_field("code", "ABC", "personal"))))

    # At MIN_MATCH_LENGTH=4
    vectors.append(run("gate2::at_min_length_match",
        "The code is ABCD here.",
        make_track(make_field("code", "ABCD", "personal"))))
    vectors.append(run("gate2::at_min_length_no_match",
        "The code is EFGH here.",
        make_track(make_field("code", "ABCD", "personal"))))

    # fields_shared exclusion
    shared_t = make_track(
        make_field("shared_field",  "Alice",    "personal"),
        make_field("private_field", "Secret99", "personal"),
    )
    vectors.append(run("gate2::fields_shared_exclusion",
        "Alice is here and Secret99 too.", shared_t,
        fields_shared=["shared_field"]))
    vectors.append(run("gate2::fields_shared_none_excluded",
        "Alice is here and Secret99 too.", shared_t,
        fields_shared=[]))
    vectors.append(run("gate2::fields_shared_null_compat",
        "Alice is here and Secret99 too.", shared_t,
        fields_shared=None))

    # Unicode -- record observed Python behavior; no expectations stated
    # Turkish I variants
    turkish_t = make_track(make_field("city", "\u0130stanbul", "personal"))
    vectors.append(run("gate2::unicode_turkish_upper_I_exact",
        "Visit \u0130stanbul today.", turkish_t))
    vectors.append(run("gate2::unicode_turkish_lower_i_folded",
        "visit istanbul today.", turkish_t))
    vectors.append(run("gate2::unicode_turkish_dotless_i",
        "visit \u0131stanbul today.", turkish_t))

    # German eszett
    german_t = make_track(make_field("street", "Stra\u00dfe", "personal"))
    vectors.append(run("gate2::unicode_german_exact",
        "Lives on Stra\u00dfe now.", german_t))
    vectors.append(run("gate2::unicode_german_lower",
        "lives on stra\u00dfe now.", german_t))
    vectors.append(run("gate2::unicode_german_ss_upper",
        "LIVES ON STRASSE NOW.", german_t))

    # NFC vs NFD -- observe parity
    nfc_val = unicodedata.normalize("NFC", "caf\u00e9")
    nfd_val = unicodedata.normalize("NFD", "caf\u00e9")
    nfc_t = make_track(make_field("place", nfc_val, "personal"))
    nfd_t = make_track(make_field("place", nfd_val, "personal"))
    vectors.append(run("gate2::unicode_nfc_field_nfc_response",
        f"We visited {nfc_val}.", nfc_t))
    vectors.append(run("gate2::unicode_nfd_field_nfc_response",
        f"We visited {nfc_val}.", nfd_t))
    vectors.append(run("gate2::unicode_nfc_field_nfd_response",
        f"We visited {nfd_val}.", nfc_t))
    vectors.append(run("gate2::unicode_nfd_field_nfd_response",
        f"We visited {nfd_val}.", nfd_t))

    # Substring overlap: value appears twice in response
    vectors.append(run("gate2::overlap_value_repeated",
        "AliceAlice is the name.",
        make_track(make_field("name", "Alice", "personal"))))

    # Substring overlap: shorter value is substring of longer field value
    sub_t = make_track(
        make_field("short", "Alice",       "personal"),
        make_field("long",  "Alice Smith", "personal"),
    )
    vectors.append(run("gate2::overlap_substring_of_longer",
        "Alice Smith was here.", sub_t))
    vectors.append(run("gate2::overlap_only_short_present",
        "Alice was here.", sub_t))

    # Duplicate values across fields
    dup_t = make_track(
        make_field("field_a", "Bob", "personal"),
        make_field("field_b", "Bob", "personal"),
    )
    vectors.append(run("gate2::duplicate_values_match",
        "Bob is mentioned.", dup_t))
    vectors.append(run("gate2::duplicate_values_no_match",
        "Nobody mentioned.", dup_t))

    # Multi-field partial / full / no match
    multi_t = make_track(
        make_field("first",  "Alice",   "personal"),
        make_field("second", "Bob",     "personal"),
        make_field("third",  "Charlie", "personal"),
    )
    vectors.append(run("gate2::multi_field_partial_match",
        "Alice was here.", multi_t))
    vectors.append(run("gate2::multi_field_all_match",
        "Alice and Bob and Charlie.", multi_t))
    vectors.append(run("gate2::multi_field_no_match",
        "Nobody mentioned.", multi_t))

    # Disclosure log written on flagged result at tier2
    vectors.append(run("gate2::disclosure_log_on_flag_tier2",
        "Hello Alice.",
        make_track(make_field("name", "Alice", "personal")),
        execution_tier=2))

    return vectors


# =============================================================================
# SECTION 4 -- Gate3
# =============================================================================

def extract_gate3() -> list[dict]:
    vectors = []
    gateway = gw()

    def run(label, content_sensitivity_severity, target_tier,
            space_max_permitted_tier, execution_tier=2) -> dict:
        r = gateway.gate3(
            step_id=sid(), focus_run_id=rid(),
            content_key="test_content_key",
            content="Some content string for testing.",
            content_sensitivity_severity=content_sensitivity_severity,
            target_tier=target_tier,
            space_max_permitted_tier=space_max_permitted_tier,
            execution_tier=execution_tier,
        )
        event_type = _last_event_type()
        d = g3_dict(r, event_type)
        d["label"]                        = label
        d["content_sensitivity_severity"] = content_sensitivity_severity
        d["target_tier"]                  = target_tier
        d["space_max_permitted_tier"]     = space_max_permitted_tier
        return d

    # Tier ceiling block: target > max
    vectors.append(run("gate3::ceiling_block::t2_max1",    1, target_tier=2, space_max_permitted_tier=1))
    vectors.append(run("gate3::ceiling_block::t3_max2",    1, target_tier=3, space_max_permitted_tier=2))
    vectors.append(run("gate3::ceiling_block::t3_max1",    1, target_tier=3, space_max_permitted_tier=1))

    # Sensitivity block: severity>=3 AND target>=2
    vectors.append(run("gate3::sensitivity_block::sev3_t2", 3, target_tier=2, space_max_permitted_tier=3))
    vectors.append(run("gate3::sensitivity_block::sev4_t2", 4, target_tier=2, space_max_permitted_tier=3))
    vectors.append(run("gate3::sensitivity_block::sev3_t3", 3, target_tier=3, space_max_permitted_tier=3))
    vectors.append(run("gate3::sensitivity_block::sev4_t3", 4, target_tier=3, space_max_permitted_tier=3))

    # Approved paths
    vectors.append(run("gate3::approved::sev2_t2",         2, target_tier=2, space_max_permitted_tier=3))
    vectors.append(run("gate3::approved::sev3_t1",         3, target_tier=1, space_max_permitted_tier=3))
    vectors.append(run("gate3::approved::sev1_t2",         1, target_tier=2, space_max_permitted_tier=3))
    vectors.append(run("gate3::approved::sev2_t3",         2, target_tier=3, space_max_permitted_tier=3))

    # Ceiling check fires first: target > max AND severity >= 3
    # event_type reveals which check actually fired
    vectors.append(run("gate3::ceiling_beats_sensitivity",  3, target_tier=3, space_max_permitted_tier=2))

    # Exact boundary: target == max -> not a ceiling block
    vectors.append(run("gate3::at_ceiling_not_blocked::sev1", 1, target_tier=2, space_max_permitted_tier=2))
    vectors.append(run("gate3::at_ceiling_not_blocked::sev2", 2, target_tier=2, space_max_permitted_tier=2))

    return vectors


# =============================================================================
# SECTION 5 -- Gate4
# =============================================================================

def extract_gate4() -> list[dict]:
    vectors = []
    gateway = gw()

    def run(label, content_sensitivity_severity, execution_tier=3) -> dict:
        r = gateway.gate4(
            step_id=sid(), focus_run_id=rid(),
            content="Content for tier3 validation.",
            content_sensitivity_severity=content_sensitivity_severity,
            execution_tier=execution_tier,
        )
        d = g4_dict(r)
        d["label"]                        = label
        d["content_sensitivity_severity"] = content_sensitivity_severity
        return d

    # CLIPBOARD_MAX_SENSITIVITY_SEVERITY = 2
    vectors.append(run("gate4::not_blocked::sev1",            1))
    vectors.append(run("gate4::not_blocked::sev2",            2))
    vectors.append(run("gate4::blocked::sev3",                3))
    vectors.append(run("gate4::blocked::sev4",                4))
    vectors.append(run("gate4::boundary_exact_max",           2))   # == max, not blocked
    vectors.append(run("gate4::boundary_one_over_max",        3))   # max+1, blocked
    vectors.append(run("gate4::content_always_approved_sev3", 3))   # content_approved always True
    vectors.append(run("gate4::content_always_approved_sev4", 4))

    return vectors


# =============================================================================
# MAIN
# =============================================================================

def main():
    if TMP_ROOT.exists():
        shutil.rmtree(TMP_ROOT)
    TMP_ROOT.mkdir(parents=True)
    OUT_DIR.mkdir(parents=True, exist_ok=True)

    try:
        print("Initialising test personal.db...")
        _ensure_good_db_exists()

        print("Extracting _apply_abstraction valid vectors...")
        aa_valid = extract_apply_abstraction_valid()

        print("Extracting _apply_abstraction invalid (invariant-violation) vectors...")
        aa_invalid = extract_apply_abstraction_invalid()

        print("Extracting Gate1 vectors...")
        g1 = extract_gate1()

        print("Extracting Gate2 vectors...")
        g2 = extract_gate2()

        print("Extracting Gate3 vectors...")
        g3 = extract_gate3()

        print("Extracting Gate4 vectors...")
        g4 = extract_gate4()

        fixtures = {
            "gate1.json": {
                "apply_abstraction_valid":   aa_valid,
                "apply_abstraction_invalid": aa_invalid,
                "gate1":                     g1,
            },
            "gate2.json": g2,
            "gate3.json": g3,
            "gate4.json": g4,
        }

        for filename, data in fixtures.items():
            path = OUT_DIR / filename
            path.write_text(
                json.dumps(data, indent=2, ensure_ascii=False),
                encoding="utf-8",
            )

        print("\nDone.")
        print(f"  apply_abstraction valid:   {len(aa_valid)} vectors")
        print(f"  apply_abstraction invalid: {len(aa_invalid)} vectors")
        print(f"  gate1:                     {len(g1)} vectors")
        print(f"  gate2:                     {len(g2)} vectors")
        print(f"  gate3:                     {len(g3)} vectors")
        print(f"  gate4:                     {len(g4)} vectors")
        total = len(aa_valid)+len(aa_invalid)+len(g1)+len(g2)+len(g3)+len(g4)
        print(f"  Total:                     {total} vectors")
        print(f"\nOutput: {OUT_DIR}")

    finally:
        if TMP_ROOT.exists():
            shutil.rmtree(TMP_ROOT)
        print("Temp data cleaned up.")


if __name__ == "__main__":
    main()
