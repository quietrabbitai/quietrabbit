// Tier 2/Tier 3 access -- selector-screen types and constants.
//
// Traces to: 03_ProjectDocs/Specifications/TIER3_ACCESS_MODEL.md
// (tracked_files.id=54), decisions.id=680-684 (2026-07-29, Session 2
// revision), decisions.id=699 (2026-07-31, embedding mechanism FINAL
// DISPOSITION -- Option 3b/CEF OSR). Both verified active/current this
// session (2026-07-31) against the full decisions.id=659-699 range --
// the document's own body contains stale "not yet logged"/"pending
// re-approval" language left over from before Chat-PM's adjudication
// pass; the design itself is current. Flagged to Chat-PM for a doc
// cleanup pass, not a build blocker.
//
// SCOPE NOTE: this file covers the selector-screen mechanics only
// (decisions.id=681) -- which providers exist, which box they belong
// in, and the combined cap-of-3 selection rule. It does NOT cover the
// split-screen/pane-compaction piece (States 4-5 in the doc), which is
// separate, larger, CEF-embedding-dependent scope, not built this pass.
//
// VISUAL DESIGN NOTE: this is a structural/behavioral build only, same
// placeholder discipline as middleZone/. No QR branding, palette, or
// visual grammar is applied here -- that is Chat-BRAND's domain
// (INFORMATION_ARCHITECTURE_SPEC.md / TIER3_ACCESS_MODEL.md are both
// owner_chat=Chat-BRAND). Class names and structure are written to be
// easy to reskin, not styled as a design decision.

/** The two lanes a provider can belong to (decisions.id=680/681).
 *  Tier is a routing designation only, never surfaced to the user by
 *  number -- the doc's own selector boxes are labeled by defining
 *  property ("No login required" / "Account required, data retained"),
 *  not by tier name. This type exists for internal routing only. */
export type ProviderLane = 'tier2' | 'tier3'

/** One selectable destination in the selector screen.
 *  PLACEHOLDER DATA SET -- the doc (Infrastructure Dependencies, Schema
 *  requirements) requires a real "provider configuration record" with
 *  citably-sourced fields (decisions.id=684), explicitly flagged as new
 *  build + research work, not yet built. The five named providers below
 *  (Duck.ai, Brave Leo / Claude, ChatGPT, Gemini) are the ones the doc
 *  itself names as of Session 2 -- listed here as a structural stand-in
 *  so the selector screen has something real to render and select
 *  against, NOT as the sourced, card-ready data the real feature needs.
 *  Mistral is deliberately excluded (decisions.id=665: disposition
 *  unresolved, not guessed at here, per the doc's own instruction). */
export interface Provider {
  id: string
  name: string
  lane: ProviderLane
}

export const PLACEHOLDER_PROVIDERS: Provider[] = [
  { id: 'duckai', name: 'Duck.ai', lane: 'tier2' },
  { id: 'brave-leo', name: 'Brave Leo', lane: 'tier2' },
  { id: 'claude', name: 'Claude', lane: 'tier3' },
  { id: 'chatgpt', name: 'ChatGPT', lane: 'tier3' },
  { id: 'gemini', name: 'Gemini', lane: 'tier3' },
]

/** Combined hard cap across BOTH boxes (decisions.id=681):
 *  screen-real-estate-driven, not provider-count-driven. This is a
 *  settled value from the doc itself, not a Chat-DEV placeholder --
 *  unlike middleZoneConfig.ts's numeric constants. */
export const MAX_SELECTED_PROVIDERS = 3
