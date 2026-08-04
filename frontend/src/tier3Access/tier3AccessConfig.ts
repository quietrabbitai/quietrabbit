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
//
// PROVIDER DATA (items.id=202 piece 1, 2026-08-04): fetchActiveProviders()
// replaces the prior PLACEHOLDER_PROVIDERS stand-in array -- real data now
// comes from commands.listActiveProviders(), backed by
// provider_store::list_active_providers() (commit 4e5147f). The IPC
// response's `lane` field is already "tier2"/"tier3" (mirrors
// provider_store::ProviderTier's own serde rendering exactly), so no
// tier-to-lane transformation happens here -- just a field rename
// (display_name -> name) to match this file's own Provider shape.

import { commands } from '../bindings'

/** The two lanes a provider can belong to (decisions.id=680/681).
 *  Tier is a routing designation only, never surfaced to the user by
 *  number -- the doc's own selector boxes are labeled by defining
 *  property ("No login required" / "Account required, data retained"),
 *  not by tier name. This type exists for internal routing only. */
export type ProviderLane = 'tier2' | 'tier3'

/** One selectable destination in the selector screen. Backed by the real
 *  provider catalog (provider_store::tier3_providers, decisions.id=684/710)
 *  via commands.listActiveProviders() -- see fetchActiveProviders() below.
 *  Card-ready fields beyond id/name/lane (retention posture, documentation
 *  gate) are not surfaced here; this screen only needs enough to render
 *  and select. */
export interface Provider {
  id: string
  name: string
  lane: ProviderLane
}

/** Fetches the selector screen's real provider list. Ordered tier-then-name
 *  by the backing query (provider_store::list_active_providers) -- the
 *  frontend does not re-sort. */
export async function fetchActiveProviders(): Promise<Provider[]> {
  const result = await commands.listActiveProviders()
  if (result.status !== 'ok') {
    throw new Error(result.error)
  }
  return result.data.map((p) => ({
    id: p.id,
    name: p.display_name,
    lane: p.lane as ProviderLane,
  }))
}

/** Combined hard cap across BOTH boxes (decisions.id=681):
 *  screen-real-estate-driven, not provider-count-driven. This is a
 *  settled value from the doc itself, not a Chat-DEV placeholder --
 *  unlike middleZoneConfig.ts's numeric constants. */
export const MAX_SELECTED_PROVIDERS = 3
