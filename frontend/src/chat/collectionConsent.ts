// Collection consent for a focus_level Focus (items.id=920 stage 3,
// decisions.id=944, decisions.id=509): the pure logic the ChatPane section uses.
// No React, no Tauri, so a plain node test covers it (collectionConsent.test.ts).
//
// WHO IS ASKED IS THE BACKEND'S DECISION. get_collection_consent_state returns
// `needs_decision` (not_asked, or granted or declined for another text version
// than the current one; a withdrawn user and a declined user at the current
// version are not asked). The frontend uses that field and NEVER recomputes it
// from the stored and current versions: a second copy of the rule would drift
// from the one the backend and its tests own. A test scans the frontend source
// for exactly that.
import type { CollectionConsentState } from '../bindings.ts'

/** The Focuses that ask for the one setup consent (posture focus_level). The
 *  backend owns the posture; the frontend only needs to know which Focus to ask
 *  for, so this is the one place that names it. */
export const COLLECTION_CONSENT_FOCUS_IDS: readonly string[] = ['tech-support']

export function usesCollectionConsent(focusId: string): boolean {
  return COLLECTION_CONSENT_FOCUS_IDS.includes(focusId)
}

/** Whether to show the confirm. Reads the backend's own field, nothing else. */
export function shouldAskForConsent(state: CollectionConsentState): boolean {
  return state.needs_decision
}

/** The text version the confirm DISPLAYS, and so the version a decision must
 *  carry: the version in force when the state was read. */
export function displayedTextVersion(state: CollectionConsentState): number {
  return state.current_text_version
}

/** The backend's short error codes (commands/collection_consent.rs), grouped by
 *  what the user should be told. The component picks the copy. */
export type ConsentErrorKind = 'stale' | 'unavailable' | 'other'

export function consentErrorKind(code: string): ConsentErrorKind {
  if (code === 'consent_text_stale') return 'stale'
  if (
    code === 'focus_settings_not_found' ||
    code === 'consent_text_unavailable' ||
    code === 'persona_not_found' ||
    code === 'invalid_focus_id'
  ) {
    return 'unavailable'
  }
  return 'other'
}
