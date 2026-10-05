// Cloud Chat -- provider rail.
//
// Traces to: 03_ProjectDocs/Specifications/TIER3_ACCESS_MODEL.md
// (tracked_files.id=54), States section 3 ("Rail + content-pane
// appears"), decisions.id=731 (QR collapse, driven by this component's
// activation callbacks, not owned here), decisions.id=732 (rail-row
// brand-color tint) and decisions.id=733 (no cap -- every candidate
// provider is listed, replacing the old two-box selector's
// decisions.id=681 cap-of-3 entirely, not just raising it).
//
// items.id=359: replaces the former two-box checkbox-selector UI in this
// same file (decisions.id=681, retired). Same component name/file per
// the item's own wording ("replace Tier3Selector.tsx's two-box UI"), new
// internals and props: no more "select up to N, then confirm" flow --
// every row is directly clickable, one click loads+activates an idle
// provider or switches the content pane to an already-loaded one.
//
// Scope: rail row rendering + activation/close callbacks only. Does NOT
// render the content pane itself, the QR collapse behavior, or own any
// pane-lifecycle IPC calls -- CloudChatAccessPane.tsx owns all three and
// passes this component only the read-only state (which providers exist,
// which are loaded, which is active) and callbacks.
//
// VISUAL DESIGN NOTE: structural/functional CSS only -- see
// cloudChatAccessConfig.ts's header for why (the whole app is still on
// the generic template palette). The one exception is per-row
// brand-color tinting (decisions.id=732), applied via
// cloudChatAccessConfig.ts's providerBrandColor() -- a narrow,
// decision-backed carve-out, not a broader visual pass.

import type { TFunction } from 'i18next'
import { useTranslation } from 'react-i18next'
import './CloudChatSelector.css'
import {
  DUCK_LOGO_URL,
  PROVIDER_LOGO_COMPONENTS,
  privacyChipLabel,
  privacyLevelColor,
  privacyLevelGlyphColor,
  privacyLevelRing,
  providerBrandColor,
  type Provider,
} from './cloudChatAccessConfig'

export type RailRowState = 'idle' | 'loaded' | 'active'

export interface CloudChatSelectorProps {
  /** Full candidate list; every provider gets a row, no cap
   *  (decisions.id=733). */
  providers: Provider[]
  /** Providers with an open (loaded) pane in Rust -- may or may not
   *  include activeProviderId. */
  openPaneIds: string[]
  /** The one provider currently shown in the content pane, or null. */
  activeProviderId: string | null
  /** decisions.id=683: Escalate skips the general cloud_anonymous/cloud_frontier choice
   *  entirely and filters the rail to cloud_frontier rows only -- unchanged
   *  intent from the retired two-box design's bottom-box-only filtering,
   *  new mechanism (a rail filter, not a hidden box). */
  escalateMode?: boolean
  /** Fires for a click on an idle OR loaded row -- the caller decides
   *  whether that means "open then activate" (idle) or "just switch the
   *  content pane" (loaded); this component doesn't know the difference
   *  beyond the row state it's already showing. */
  onActivate: (providerId: string) => void
  /** Fires from a row's hover-revealed close control -- ends that
   *  provider's session outright (returns it to idle). */
  onClose: (providerId: string) => void
}

function rowState(
  providerId: string,
  openPaneIds: string[],
  activeProviderId: string | null,
): RailRowState {
  if (providerId === activeProviderId) return 'active'
  if (openPaneIds.includes(providerId)) return 'loaded'
  return 'idle'
}

/** items.id=603: tooltip body for the rail chip -- the level, a login line,
 *  the curated user_privacy_summary text when present (the only retention
 *  information shown), hosting, and the default-posture caveat. */
function chipTooltip(provider: Provider, t: TFunction): string {
  const lines = [
    privacyChipLabel(provider.privacyGuardianDefaultLevel, t),
    provider.loginRequired
      ? t('cloudChatSelector.tooltip.loginRequired')
      : t('cloudChatSelector.tooltip.noLogin'),
  ]
  if (provider.userPrivacySummary) lines.push(provider.userPrivacySummary)
  lines.push(
    t('cloudChatSelector.tooltip.cloudHosted'),
    t('cloudChatSelector.badgeDefaultPostureNotice', { providerName: provider.name }),
  )
  return lines.join('\n')
}

export function CloudChatSelector({
  providers,
  openPaneIds,
  activeProviderId,
  escalateMode = false,
  onActivate,
  onClose,
}: CloudChatSelectorProps) {
  const { t } = useTranslation()

  const rows = escalateMode ? providers.filter((p) => p.lane === 'cloud_frontier') : providers

  return (
    <ul className="cloud-chat-rail" aria-label={t('cloudChatSelector.railLabel')}>
      {rows.map((provider) => {
        const state = rowState(provider.id, openPaneIds, activeProviderId)
        const LogoComponent = PROVIDER_LOGO_COMPONENTS[provider.id]
        const isDuck = provider.id === 'duckai'
        const hasRealLogo = Boolean(LogoComponent) || isDuck
        return (
          <li
            key={provider.id}
            className={`cloud-chat-rail__row cloud-chat-rail__row--${state}`}
            data-provider={provider.id}
            data-state={state}
            style={
              state !== 'idle'
                ? ({ '--row-brand-color': providerBrandColor(provider.id) } as React.CSSProperties)
                : undefined
            }
          >
            <button
              type="button"
              className="cloud-chat-rail__row-main"
              onClick={() => onActivate(provider.id)}
            >
              <span
                className="cloud-chat-rail__icon"
                aria-hidden="true"
                style={
                  !hasRealLogo && state !== 'idle'
                    ? { backgroundColor: providerBrandColor(provider.id) }
                    : undefined
                }
              >
                {LogoComponent ? (
                  <LogoComponent size={16} />
                ) : isDuck ? (
                  <img src={DUCK_LOGO_URL} width={16} height={16} alt="" />
                ) : (
                  provider.name.slice(0, 1)
                )}
              </span>
              <span className="cloud-chat-rail__meta">
                <span className="cloud-chat-rail__name">{provider.name}</span>
                <span className="cloud-chat-rail__state">
                  {t('cloudChatSelector.stateLine', {
                    state: t(`cloudChatSelector.rowState.${state}`),
                    access: provider.loginRequired
                      ? t('cloudChatSelector.loginRequired')
                      : t('cloudChatSelector.noLogin'),
                  })}
                </span>
              </span>
              <span
                className="cloud-chat-rail__chip"
                style={{
                  backgroundColor: privacyLevelColor(provider.privacyGuardianDefaultLevel),
                  color: privacyLevelGlyphColor(provider.privacyGuardianDefaultLevel),
                  boxShadow: privacyLevelRing(provider.privacyGuardianDefaultLevel),
                }}
                title={chipTooltip(provider, t)}
              >
                {privacyChipLabel(provider.privacyGuardianDefaultLevel, t)}
              </span>
            </button>
            {state !== 'idle' && (
              <button
                type="button"
                className="cloud-chat-rail__close"
                title={t('cloudChatSelector.closeRowTitle', { name: provider.name })}
                onClick={(e) => {
                  e.stopPropagation()
                  onClose(provider.id)
                }}
              >
                &times;
              </button>
            )}
          </li>
        )
      })}
    </ul>
  )
}
