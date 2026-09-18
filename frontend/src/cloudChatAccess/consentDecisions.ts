// Shared pure logic for classifying a resolved Gate3 consent decision set.
// Extracted out of PrivacyGuardianModal.tsx's handleSend and
// Tier3AccessPane.tsx's handleModalResolve (items.id=377): both had their
// own inline `decisions.length === keptPrivate` / `decisions.every(...)`
// check, and `Array.prototype.every` on an empty array is vacuously true --
// with zero spans to review (gate3's zero-spans-forced-High branch,
// D6-362/decisions.id=405, or any other empty-spans payload) both call
// sites read that as "everything was kept private" when nothing was
// reviewed at all. One shared function, guarded on length, fixes both.
//
// ElementDecisionKind/ElementDecision live here (a plain .ts module, no
// JSX) rather than in PrivacyGuardianModal.tsx so this file's own test can
// run directly on Node's type-stripping without needing a JSX transform --
// PrivacyGuardianModal.tsx re-exports both for its existing callers.
export type ElementDecisionKind = 'generalize' | 'keep_private' | 'release_original'

export interface ElementDecision {
  span_id: string
  decision: ElementDecisionKind
  suggestion_text: string | null
  user_modified_text: string | null
  /** items.id=406: echoed back unchanged from the matching ConsentSpanItem.category. */
  category: string
  /** items.id=406: echoed back unchanged from the matching ConsentSpanItem.fact_key --
   *  null when gate3 couldn't resolve a stable identity for this span. */
  fact_key: string | null
  /** items.id=406 (decisions.id=756): "remember this for [Persona]" opt-in,
   *  off by default. Ignored server-side when fact_key is null. */
  save_for_persona: boolean
}

export function isAllKeptPrivate(decisions: ElementDecision[]): boolean {
  return decisions.length > 0 && decisions.every((d) => d.decision === 'keep_private')
}
