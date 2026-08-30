// Tier 2/Tier 3 access -- provider rail.
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
// pane-lifecycle IPC calls -- Tier3AccessPane.tsx owns all three and
// passes this component only the read-only state (which providers exist,
// which are loaded, which is active) and callbacks.
//
// VISUAL DESIGN NOTE: structural/functional CSS only -- see
// tier3AccessConfig.ts's header for why (the whole app is still on the
// generic template palette). The one exception is per-row brand-color
// tinting (decisions.id=732), applied via tier3AccessConfig.ts's
// providerBrandColor() -- a narrow, decision-backed carve-out, not a
// broader visual pass.

import { useTranslation } from 'react-i18next'
import './Tier3Selector.css'
import { providerBrandColor, type Provider } from './tier3AccessConfig'

export type RailRowState = 'idle' | 'loaded' | 'active'

export interface Tier3SelectorProps {
  /** Full candidate list; every provider gets a row, no cap
   *  (decisions.id=733). */
  providers: Provider[]
  /** Providers with an open (loaded) pane in Rust -- may or may not
   *  include activeProviderId. */
  openPaneIds: string[]
  /** The one provider currently shown in the content pane, or null. */
  activeProviderId: string | null
  /** decisions.id=683: Escalate skips the general Tier 2/Tier 3 choice
   *  entirely and filters the rail to Tier 3 rows only -- unchanged
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

export function Tier3Selector({
  providers,
  openPaneIds,
  activeProviderId,
  escalateMode = false,
  onActivate,
  onClose,
}: Tier3SelectorProps) {
  const { t } = useTranslation()

  const rows = escalateMode ? providers.filter((p) => p.lane === 'tier3') : providers

  return (
    <ul className="tier3-rail" aria-label={t('tier3Selector.railLabel')}>
      {rows.map((provider) => {
        const state = rowState(provider.id, openPaneIds, activeProviderId)
        return (
          <li
            key={provider.id}
            className={`tier3-rail__row tier3-rail__row--${state}`}
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
              className="tier3-rail__row-main"
              onClick={() => onActivate(provider.id)}
            >
              <span
                className="tier3-rail__icon"
                aria-hidden="true"
                style={
                  state !== 'idle'
                    ? { backgroundColor: providerBrandColor(provider.id) }
                    : undefined
                }
              >
                {provider.name.slice(0, 1)}
              </span>
              <span className="tier3-rail__meta">
                <span className="tier3-rail__name">{provider.name}</span>
                <span className="tier3-rail__state">
                  {t(`tier3Selector.rowState.${state}`)}
                </span>
              </span>
              {state !== 'idle' && (
                <span
                  className="tier3-rail__hop"
                  title={t('tier3Selector.hopIndicatorTitle')}
                  aria-hidden="true"
                />
              )}
              <span
                className="tier3-rail__tierbadge"
                title={
                  provider.lane === 'tier2'
                    ? t('tier3Selector.tier2BadgeTitle')
                    : t('tier3Selector.tier3BadgeTitle')
                }
                aria-hidden="true"
              >
                {provider.lane === 'tier2' ? '⚡' : '☁'}
              </span>
            </button>
            {state !== 'idle' && (
              <button
                type="button"
                className="tier3-rail__close"
                title={t('tier3Selector.closeRowTitle', { name: provider.name })}
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
