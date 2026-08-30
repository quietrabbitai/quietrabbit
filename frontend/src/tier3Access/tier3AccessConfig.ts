// Tier 2/Tier 3 access -- rail types and constants.
//
// Traces to: 03_ProjectDocs/Specifications/TIER3_ACCESS_MODEL.md
// (tracked_files.id=54), decisions.id=680-684 (2026-07-29, Session 2
// revision), decisions.id=699 (2026-07-31, embedding mechanism FINAL
// DISPOSITION -- Option 3b/CEF OSR), decisions.id=731-733 (2026-08-29,
// Session 3 rail+content-pane revision, items.id=359 implementation).
//
// SCOPE NOTE: this file covers the rail's provider data and per-provider
// display metadata -- which providers exist, which lane they're in, and
// (new, items.id=359) each provider's brand-tint color. It does NOT cover
// the pane-hosting mechanics themselves (PaneHitLayer.tsx, paneLayout.ts,
// Tier3AccessPane.tsx own that).
//
// VISUAL DESIGN NOTE: this is a structural/behavioral build only, same
// placeholder discipline as middleZone/ -- the whole app still runs the
// generic Vite/Tauri template palette (frontend/src/index.css), Palette
// v3 has not been applied live anywhere yet, so this file does not import
// it either. The one exception is PROVIDER_BRAND_COLORS below --
// decisions.id=732's explicit, narrow carve-out from Palette v3's lock
// applies regardless of whether the rest of the app has been skinned yet,
// since it's about *external* providers' own brand identity, not QR's.
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

/** decisions.id=733 (2026-08-29, implemented 2026-08-30, items.id=359):
 *  the combined cap of 3 (decisions.id=681) is superseded -- the rail
 *  lists every candidate provider, no cap. Its own stated rationale
 *  (screen real estate) no longer applies once only one provider is ever
 *  rendered at full size at a time. No replacement constant: the new
 *  click-per-row rail model has no "select up to N" concept at all. */

/** decisions.id=732: rail rows tinted in each external provider's own
 *  brand color when loaded/active -- an explicit, narrow carve-out from
 *  Palette v3's lock (rule 17), justified specifically because it
 *  reinforces that these rows are external to QR, not a QR-owned
 *  surface. Scope is limited to rail rows; see this file's own header.
 *
 *  Source: TIER3_PANE_RAIL_MOCKUP_20260829.html's own per-provider
 *  colors (the Session 3 design reference Jason approved), keyed by
 *  provider id. No `provider_store::Provider` schema field carries a
 *  color (decisions.id=684's provider-configuration-record work is
 *  separately tracked, not part of this item) -- this is a pragmatic,
 *  narrowly-scoped frontend stand-in, not a claim the color belongs in
 *  the schema. `DEFAULT_PROVIDER_BRAND_COLOR` covers any provider id not
 *  in the map (a muted neutral, not an unstyled/invisible tint) so a
 *  newly-added provider in `provider_store` never renders unstyled. */
export const PROVIDER_BRAND_COLORS: Record<string, string> = {
  duck: '#DE8D3A',
  brave: '#FB542B',
  claude: '#CC785C',
  chatgpt: '#10A37F',
  gemini: '#4285F4',
}

export const DEFAULT_PROVIDER_BRAND_COLOR = '#5A6870'

export function providerBrandColor(providerId: string): string {
  return PROVIDER_BRAND_COLORS[providerId] ?? DEFAULT_PROVIDER_BRAND_COLOR
}
