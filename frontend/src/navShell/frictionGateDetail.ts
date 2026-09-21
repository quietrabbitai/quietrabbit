// items.id=533: FrictionGateDetail and its parser, extracted out of
// FocusSettingsControls.tsx into their own non-JSX module -- this is what
// makes a React-free Node test possible at all (see
// frictionGateDetail.test.ts), following the same precedent as
// cloudChatAccess/consentDecisions.ts and reviewSections.ts.
//
// FrictionGateDetail is hand-declared here, not generated: update_focus_
// settings returns Result<FocusInfo, String>, and the gate trip is a
// JSON-serialized FrictionGateDetail *inside* that Err(String) -- not a
// distinct typed error -- so tauri-specta's collect_commands! never sees it
// even though the Rust struct derives specta::Type. Same convention
// PrivacyGuardianModal.tsx uses for ConsentRequestPayload (see that file's
// header comment). Keep in sync by hand with commands/persona.rs's
// FrictionGateDetail.
//
// items.id=533: privacy_tier fields retyped from a plain 1/2/3 ordinal to
// the 3-value PrivacyPreference enum (red/yellow/green), mirroring
// items.id=448's earlier max_permitted_tier -> ExternalAccess retype.

// '../bindings.ts' with the extension: the extensionless form fails tsc -b
// under tsconfig.node.json with TS2835 (reproduced by Chat-PM).
import type { ExternalAccess, PrivacyPreference } from '../bindings.ts'

export interface FrictionGateDetail {
  persona_id: string
  focus_id: string
  requested_privacy_tier: PrivacyPreference | null
  requested_focus_profile: string | null
  requested_max_permitted_tier: ExternalAccess | null
  existing_privacy_tier: PrivacyPreference
  existing_focus_profile: string
  existing_max_permitted_tier: ExternalAccess
  privacy_would_loosen: boolean
  moves_to_protected: boolean
  max_permitted_tier_would_loosen: boolean
}

// Record<Union, true> rather than a plain array: this makes both lists
// exhaustive over their bindings.ts union at the type level -- if Rust ever
// grows a fourth PrivacyPreference or ExternalAccess variant, tsc fails
// here (missing key) instead of the new variant silently passing validation
// as an unrecognized-but-untyped string.
const PRIVACY_PREFERENCE_VALUES: Record<PrivacyPreference, true> = {
  red: true,
  yellow: true,
  green: true,
}

const EXTERNAL_ACCESS_VALUES: Record<ExternalAccess, true> = {
  local_only: true,
  anonymous_required: true,
  anonymous_preferred: true,
  unrestricted: true,
}

function isPrivacyPreference(value: unknown): value is PrivacyPreference {
  return typeof value === 'string' && Object.hasOwn(PRIVACY_PREFERENCE_VALUES, value)
}

function isExternalAccess(value: unknown): value is ExternalAccess {
  return typeof value === 'string' && Object.hasOwn(EXTERNAL_ACCESS_VALUES, value)
}

export function parseFrictionGateDetail(errorText: string): FrictionGateDetail | null {
  try {
    const parsed: unknown = JSON.parse(errorText)
    if (
      parsed === null ||
      typeof parsed !== 'object' ||
      !('persona_id' in parsed) ||
      !('privacy_would_loosen' in parsed) ||
      !('max_permitted_tier_would_loosen' in parsed)
    ) {
      return null
    }

    // items.id=533: validate every enum-typed field, not just the newly
    // retyped privacy_tier ones -- max_permitted_tier's fields have gone
    // through this same unchecked `as` cast since items.id=448 with no
    // validation of their own; this function is already being opened for
    // the privacy_tier change, and validating one enum field but not its
    // sibling three lines away would be an inconsistent half-fix. A field
    // that fails validation makes this function return null, which
    // handleSave already treats as "not a friction-gate error" and falls
    // through to surfacing the raw JSON string via setSaveError -- exactly
    // the "wire drift fails visibly" behavior this item exists to add.
    const candidate = parsed as Record<string, unknown>
    if (
      (candidate.requested_privacy_tier !== null &&
        !isPrivacyPreference(candidate.requested_privacy_tier)) ||
      !isPrivacyPreference(candidate.existing_privacy_tier) ||
      (candidate.requested_max_permitted_tier !== null &&
        !isExternalAccess(candidate.requested_max_permitted_tier)) ||
      !isExternalAccess(candidate.existing_max_permitted_tier)
    ) {
      return null
    }

    return parsed as FrictionGateDetail
  } catch {
    // Not JSON -- a plain error string (not_found, tier bounds check, ...).
  }
  return null
}
