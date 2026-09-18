// Cloud Chat -- rail types and constants.
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
// response's `lane` field is computed by commands/tier3_pane.rs's
// lane_str() from provider_type (providers has no tier column of its own
// -- items.id=427) and already renders "cloud_anonymous"/"cloud_frontier"
// literally, so no tier-to-lane transformation happens here -- just a
// field rename (display_name -> name) to match this file's own Provider
// shape.

// Deep component imports, not the top-level `@lobehub/icons` barrel: each
// icon's barrel `index.js` unconditionally builds a "compounded" object
// (Avatar/Color/Combine/Text all attached to the default export), and the
// Combine feature alone statically imports `react-layout-kit` -- a real,
// unavoidable runtime dependency once any icon is imported through the
// barrel, regardless of whether this app ever touches `.Combine`. The leaf
// component files below have no such dependency (confirmed: only `react`
// and a local, import-free `style` module) -- same real, MIT-licensed SVGs,
// none of the barrel's transitive weight.
import ClaudeColor from '@lobehub/icons/es/Claude/components/Color'
import GeminiColor from '@lobehub/icons/es/Gemini/components/Color'
import GroqMono from '@lobehub/icons/es/Groq/components/Mono'
import MistralColor from '@lobehub/icons/es/Mistral/components/Color'
import OpenAIMono from '@lobehub/icons/es/OpenAI/components/Mono'
import type { IconType } from '@lobehub/icons'
import { commands } from '../bindings'
import type { PrivacyGuardianDefaultLevel } from '../bindings'
import duckLogoUrl from './assets/duckduckgo-dax-solo.svg'

/** The two lanes a provider can belong to (decisions.id=680/681).
 *  Tier is a routing designation only, never surfaced to the user by
 *  number -- the doc's own selector boxes are labeled by defining
 *  property ("No login required" / "Account required, data retained"),
 *  not by tier name. This type exists for internal routing only. */
export type ProviderLane = 'cloud_anonymous' | 'cloud_frontier'

/** One selectable destination in the selector screen. Backed by the real
 *  provider catalog (provider_store::tier3_providers, decisions.id=684/710)
 *  via commands.listActiveProviders() -- see fetchActiveProviders() below.
 *  loginRequired/isAnonymous/privacyGuardianDefaultLevel (items.id=418) are
 *  the provider indicator badges' real, existing data source -- no other
 *  card-ready fields (documentation gate, etc.) are surfaced here. */
export interface Provider {
  id: string
  name: string
  lane: ProviderLane
  loginRequired: boolean
  isAnonymous: boolean
  privacyGuardianDefaultLevel: PrivacyGuardianDefaultLevel | null
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
    loginRequired: p.login_required,
    isAnonymous: p.is_anonymous,
    privacyGuardianDefaultLevel: p.privacy_guardian_default_level,
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
  duckai: '#DE8D3A',
  claude: '#CC785C',
  chatgpt: '#10A37F',
  gemini: '#4285F4',
}

export const DEFAULT_PROVIDER_BRAND_COLOR = '#5A6870'

export function providerBrandColor(providerId: string): string {
  return PROVIDER_BRAND_COLORS[providerId] ?? DEFAULT_PROVIDER_BRAND_COLOR
}

/** items.id=418 (decisions.id=800): rule 17 carve-out for the identity
 *  badge's fill color, same precedent and same narrow scope as
 *  PROVIDER_BRAND_COLORS above -- mapped directly to the real
 *  privacy_guardian_default_level field, no invented data. */
export const PRIVACY_LEVEL_COLORS: Record<PrivacyGuardianDefaultLevel, string> = {
  low: '#4c7a5e',
  medium: '#c19e42',
  high: '#a15a4c',
}

/** Neutral fallback for a provider not yet curated (level is null) --
 *  not a claim that its actual risk is low, medium, or high. */
export const DEFAULT_PRIVACY_LEVEL_COLOR = '#5A6870'

export function privacyLevelColor(level: PrivacyGuardianDefaultLevel | null): string {
  return level ? PRIVACY_LEVEL_COLORS[level] : DEFAULT_PRIVACY_LEVEL_COLOR
}

/** items.id=418: real provider logos via @lobehub/icons (MIT), replacing
 *  the letter-square placeholder for the providers it covers. Named
 *  product-branded marks (Claude, OpenAI), not the parent-company ones
 *  (Anthropic, Google) -- matches this app's existing id/name conventions.
 *  Claude/Gemini/Mistral use their multi-color mark; Groq and OpenAI's own
 *  marks are single-color by design (neither ships a `.Color` variant at
 *  all), so their Mono component *is* their real logo, not a fallback. No
 *  lobehub export exists for Duck.ai/DuckDuckGo; see DUCK_LOGO_URL. */
export const PROVIDER_LOGO_COMPONENTS: Record<string, IconType> = {
  claude: ClaudeColor,
  chatgpt: OpenAIMono,
  gemini: GeminiColor,
  groq: GroqMono,
  mistral: MistralColor,
}

/** items.id=418: DuckDuckGo has no @lobehub/icons entry. Jason's direction
 *  (2026-09-07): use the actual DuckDuckGo mark, sourced unmodified from
 *  duckduckgo.com/press's official brand assets, rather than an
 *  approximation or a differently-styled icon-pack version -- see
 *  assets/duckduckgo-dax-solo.svg's own header for source/license detail.
 *  This means the badge won't visually distinguish Duck.ai from the base
 *  DuckDuckGo brand; accepted tradeoff vs. altering a third party's mark. */
export const DUCK_LOGO_URL = duckLogoUrl
