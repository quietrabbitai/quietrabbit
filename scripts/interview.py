#!/usr/bin/env python3
# scripts/interview.py
# CLI Personal Specialist install interview.
# Proves field storage end-to-end before the UI exists (Layer 5).
# Full conversational UI interview wired in Layer 8.
#
# Usage (inside container):
#   docker compose exec qr-conductor python scripts/interview.py
#
# Usage (dev, direct):
#   QR_DEV_KEY_HEX=<hex> python scripts/interview.py
#
# Writes to personal.db for dev-user / dev-life using QR_DEV_KEY_HEX.
# In production (Layer 8), this is replaced by the Flask UI interview flow
# with InMemoryKeyRegistry key access.
#
# Fields collected (personal_fields table):
#   general:   preferred_name, location_city, location_country, timezone
#   personal:  job_title, employer, industry
#   medical:   dietary_restrictions (optional, skip-able)
#   financial: income_range (optional, skip-able)
#
# Communication preferences (voice_profiles table, NOT personal_fields):
#   tone, formality, length_preference stored at precedence 3 (global).
#   These are voice-profile-only attributes -- they shape output style but
#   are not personal data fields. Gate1 does not process them.
#
# Abstraction defaults per personal-specialist.operator:
#   general:   tier2=pass,          tier3=summarize
#   personal:  tier2=summarize,     tier3=omit
#   medical:   tier2=not_permitted, tier3=not_permitted
#   financial: tier2=not_permitted, tier3=not_permitted
#
# Updated as part of Phase A codebase rename (D6-224, D6-225):
#   SPACE_ID / QR_INTERVIEW_SPACE_ID → PERSONA_ID / QR_INTERVIEW_PERSONA_ID
#   specialist_id → source_id in save_personal_field and save_voice_profile_entry
#   SPECIALIST_ID → SOURCE_ID constant
#   migrate_personal_db: space_id → life_id param
# Updated as part of Phase C Persona model migration (D6-298):
#   LIFE_ID / QR_INTERVIEW_LIFE_ID → PERSONA_ID / QR_INTERVIEW_PERSONA_ID
#   All call site parameters: life_id= → persona_id=
#   migrate_personal_db: life_id param → persona_id param

import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

from persistence.personal_store import (
    list_personal_fields,
    save_personal_field,
    save_voice_profile_entry,
)
from persistence.migrations import migrate_personal_db

# -- Dev mode constants -------------------------------------------------------

USER_ID = os.environ.get("QR_INTERVIEW_USER_ID", "dev-user")
PERSONA_ID = os.environ.get("QR_INTERVIEW_PERSONA_ID", "dev-life")

KEY_HEX = (os.environ.get("QR_DEV_KEY_HEX") or "").strip()

SOURCE_ID = "personal-specialist"

MIN_EXPECTED_GENERAL_FIELDS = 3


# -- Helpers ------------------------------------------------------------------

def prompt(label: str, hint: str = "", required: bool = True) -> str:
    suffix = f" ({hint})" if hint else ""
    if not required:
        suffix += " [optional -- press Enter to skip]"
    while True:
        raw = input(f"  {label}{suffix}: ").strip()
        if raw:
            return raw
        if not required:
            return ""
        print("    This field is required. Please enter a value.")


def section(title: str) -> None:
    print(f"\n{'─' * 52}")
    print(f"  {title}")
    print(f"{'─' * 52}")


def write_field(
    field_name: str,
    field_value: str,
    sensitivity: str,
    abstraction_tier2: str = "pass",
    abstraction_tier3: str = "pass",
) -> None:
    if not field_value:
        print(f"    o  Skipped {field_name}")
        return
    save_personal_field(
        user_id=USER_ID,
        persona_id=PERSONA_ID,
        key_hex=KEY_HEX,
        field_name=field_name,
        field_value=field_value,
        sensitivity=sensitivity,
        source_id=SOURCE_ID,
        abstraction_tier2=abstraction_tier2,
        abstraction_tier3=abstraction_tier3,
        source="interview",
    )
    print(f"    +  {field_name} saved ({sensitivity})")


# -- Interview ----------------------------------------------------------------

def run_interview() -> None:
    if not KEY_HEX:
        print(
            "\nERROR: QR_DEV_KEY_HEX not set.\n"
            "Add it to your .env file and docker-compose.yml environment: section.\n"
            "Example: QR_DEV_KEY_HEX="
            "0000000000000000000000000000000000000000000000000000000000000000"
        )
        sys.exit(1)

    print("\n" + "=" * 52)
    print("  Quiet Rabbit -- Personal Specialist Setup")
    print("  Layer 5 CLI Interview")
    print("=" * 52)
    print(f"\n  Writing to: {USER_ID} / {PERSONA_ID}")
    print("  Press Ctrl+C at any time to cancel.\n")

    print("  Initialising personal database...")
    try:
        migrate_personal_db(USER_ID, PERSONA_ID, KEY_HEX)
        print("  +  personal.db ready\n")
    except Exception as e:
        print(f"\nERROR: Could not initialise personal.db: {e}")
        sys.exit(1)

    try:
        # -- General fields ---------------------------------------------------
        section("About you -- general information")
        print("  General-sensitivity -- safe for Tier 2 (pass-through).\n")

        write_field(
            "preferred_name",
            prompt("Preferred name", hint="how you'd like to be addressed"),
            sensitivity="general",
            abstraction_tier2="pass",
            abstraction_tier3="summarize",
        )
        write_field(
            "location_city",
            prompt("City"),
            sensitivity="general",
            abstraction_tier2="pass",
            abstraction_tier3="summarize",
        )
        write_field(
            "location_country",
            prompt("Country"),
            sensitivity="general",
            abstraction_tier2="pass",
            abstraction_tier3="summarize",
        )
        write_field(
            "timezone",
            prompt(
                "Timezone",
                hint="e.g. Europe/London, America/New_York",
                required=False,
            ),
            sensitivity="general",
            abstraction_tier2="pass",
            abstraction_tier3="summarize",
        )

        # -- Personal fields --------------------------------------------------
        section("Work context -- personal information")
        print("  Personal-sensitivity -- summarised at Tier 2, omitted at Tier 3.\n")

        write_field(
            "job_title",
            prompt("Job title or role"),
            sensitivity="personal",
            abstraction_tier2="summarize",
            abstraction_tier3="omit",
        )
        write_field(
            "employer",
            prompt("Employer or organisation", required=False),
            sensitivity="personal",
            abstraction_tier2="summarize",
            abstraction_tier3="omit",
        )
        write_field(
            "industry",
            prompt(
                "Industry",
                hint="e.g. healthcare, software, finance",
                required=False,
            ),
            sensitivity="personal",
            abstraction_tier2="summarize",
            abstraction_tier3="omit",
        )

        # -- Communication preferences ----------------------------------------
        section("Communication preferences -- voice profile")
        print("  These shape how Quiet Rabbit writes for you.")
        print("  Stored in voice_profiles (NOT personal_fields) at precedence 3")
        print("  (global -- applies across all personas). Gate1 does not process these.\n")

        tone = (
            prompt(
                "Preferred tone",
                hint="direct / balanced / warm / formal",
                required=False,
            )
            or "balanced"
        )
        formality = (
            prompt(
                "Formality",
                hint="casual / moderate / formal",
                required=False,
            )
            or "moderate"
        )
        length_pref = (
            prompt(
                "Response length preference",
                hint="concise / detailed",
                required=False,
            )
            or "concise"
        )

        for attribute, value in [
            ("tone", tone),
            ("formality", formality),
            ("length_preference", length_pref),
        ]:
            save_voice_profile_entry(
                user_id=USER_ID,
                persona_id=PERSONA_ID,
                key_hex=KEY_HEX,
                attribute=attribute,
                value=value,
                precedence=3,
                source_id=SOURCE_ID,
            )
        print(
            f"    +  Voice profile saved: "
            f"tone={tone}, formality={formality}, length={length_pref}"
        )

        # -- Medical fields ---------------------------------------------------
        section("Health context -- medical information (optional)")
        print("  Medical-sensitivity -- blocked from Tier 2 and Tier 3.")
        print("  Used only by Tier 1 local models.\n")

        dietary = prompt(
            "Dietary restrictions or allergies",
            hint="e.g. vegan, nut allergy, gluten-free",
            required=False,
        )
        write_field(
            "dietary_restrictions",
            dietary,
            sensitivity="medical",
            abstraction_tier2="not_permitted",
            abstraction_tier3="not_permitted",
        )

        # -- Financial fields -------------------------------------------------
        section("Financial context -- financial information (optional)")
        print("  Financial-sensitivity -- blocked from Tier 2 and Tier 3.")
        print("  Used only by Tier 1 local models.\n")

        income = prompt(
            "Income range",
            hint="e.g. 40k-50k GBP, 80k-100k USD",
            required=False,
        )
        write_field(
            "income_range",
            income,
            sensitivity="financial",
            abstraction_tier2="not_permitted",
            abstraction_tier3="not_permitted",
        )

        # -- Summary ----------------------------------------------------------
        print("\n" + "=" * 52)
        print("  Interview complete.")
        print("=" * 52)

        fields = list_personal_fields(
            user_id=USER_ID,
            persona_id=PERSONA_ID,
            key_hex=KEY_HEX,
        )

        print(f"\n  Fields in personal.db: {len(fields)}\n")
        col_name  = "Field name"
        col_sens  = "Sensitivity"
        col_tier2 = "Tier2 policy"
        col_tier3 = "Tier3 policy"
        print(
            f"  {col_name:<30}  {col_sens:<12}  {col_tier2:<16}  {col_tier3}"
        )
        print(f"  {'─' * 30}  {'─' * 12}  {'─' * 16}  {'─' * 16}")
        for f in fields:
            print(
                f"  {f.field_name:<30}  {f.sensitivity:<12}  "
                f"{f.abstraction_tier2:<16}  {f.abstraction_tier3}"
            )

        general_fields = [f for f in fields if f.sensitivity == "general"]
        assert len(general_fields) >= MIN_EXPECTED_GENERAL_FIELDS, (
            f"\nSmoke test FAILED: expected >= {MIN_EXPECTED_GENERAL_FIELDS} "
            f"general fields, got {len(general_fields)}. "
            f"Check personal.db write path and QR_DEV_KEY_HEX."
        )
        print(
            f"\n  Smoke test passed: {len(general_fields)} general fields, "
            f"{len([f for f in fields if f.sensitivity == 'personal'])} personal, "
            f"{len([f for f in fields if f.sensitivity in ('medical', 'financial')])} "
            f"sensitive."
        )
        print()

    except KeyboardInterrupt:
        print("\n\n  Interview cancelled. Any fields already saved are retained.\n")
        sys.exit(0)


if __name__ == "__main__":
    run_interview()
