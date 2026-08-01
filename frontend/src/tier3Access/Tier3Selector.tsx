// Tier 2/Tier 3 access -- selector screen.
//
// Traces to: 03_ProjectDocs/Specifications/TIER3_ACCESS_MODEL.md
// (tracked_files.id=54), States section 3 ("Selector screen"),
// decisions.id=681 (two-box layout, cross-box multi-select, cap of 3)
// and decisions.id=683 (Escalate is Tier-3-only, bypasses the general
// two-box choice -- bottom box contents only).
//
// Scope: selector mechanics only (which providers, which box, the cap,
// confirm). Does NOT render the split screen this feeds into (States
// 4-5) -- that is separate, CEF-embedding-dependent scope, not built
// this pass. Does NOT render the outbound Privacy Guardian gate that
// must precede this screen existing at all (per the doc: "not disabled,
// simply absent" before the gate clears) -- that gate is existing
// locked infrastructure this component does not own or reimplement;
// the caller is responsible for only mounting this after the gate has
// cleared.
//
// VISUAL DESIGN NOTE: structural/behavioral only, no Chat-BRAND visual
// pass applied -- see tier3AccessConfig.ts header.

import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import './Tier3Selector.css'
import {
  MAX_SELECTED_PROVIDERS,
  type Provider,
  type ProviderLane,
} from './tier3AccessConfig'

export interface Tier3SelectorProps {
  /** Full candidate list; the component splits by lane internally. */
  providers: Provider[]
  /** decisions.id=683: Escalate skips the general Tier 2/Tier 3 choice
   *  entirely and goes straight to Tier 3 provider selection -- the
   *  bottom box's contents only, no Tier 2 box rendered at all. */
  escalateMode?: boolean
  /** Fires once the user has selected 1-3 providers and confirmed. */
  onConfirm: (selected: Provider[]) => void
}

const LANE_ORDER: ProviderLane[] = ['tier2', 'tier3']

export function Tier3Selector({
  providers,
  escalateMode = false,
  onConfirm,
}: Tier3SelectorProps) {
  const { t } = useTranslation()
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set())

  const lanes = escalateMode ? (['tier3'] as ProviderLane[]) : LANE_ORDER

  const atCap = selectedIds.size >= MAX_SELECTED_PROVIDERS

  const toggleProvider = (id: string) => {
    setSelectedIds((prev) => {
      const next = new Set(prev)
      if (next.has(id)) {
        next.delete(id)
        return next
      }
      // decisions.id=681: combined hard cap of 3 across both boxes --
      // prevent selecting a 4th rather than allow-then-truncate, so the
      // user's own selection order is never silently discarded.
      if (next.size >= MAX_SELECTED_PROVIDERS) {
        return prev
      }
      next.add(id)
      return next
    })
  }

  const handleConfirm = () => {
    const selected = providers.filter((p) => selectedIds.has(p.id))
    onConfirm(selected)
  }

  return (
    <div className="tier3-selector">
      {lanes.map((lane) => (
        <fieldset
          key={lane}
          className="tier3-selector__box"
          data-lane={lane}
        >
          <legend className="tier3-selector__box-label">
            {lane === 'tier2'
              ? t('tier3Selector.tier2BoxLabel')
              : t('tier3Selector.tier3BoxLabel')}
          </legend>
          <ul className="tier3-selector__list">
            {providers
              .filter((p) => p.lane === lane)
              .map((provider) => {
                const checked = selectedIds.has(provider.id)
                const disabled = !checked && atCap
                return (
                  <li key={provider.id} className="tier3-selector__item">
                    <label className="tier3-selector__item-label">
                      <input
                        type="checkbox"
                        checked={checked}
                        disabled={disabled}
                        onChange={() => toggleProvider(provider.id)}
                      />
                      {provider.name}
                    </label>
                  </li>
                )
              })}
          </ul>
        </fieldset>
      ))}

      <div className="tier3-selector__footer">
        <span aria-live="polite">
          {t('tier3Selector.selectionCount', {
            selected: selectedIds.size,
            max: MAX_SELECTED_PROVIDERS,
          })}
        </span>
        <button
          type="button"
          disabled={selectedIds.size === 0}
          onClick={handleConfirm}
        >
          {t('tier3Selector.continueButton')}
        </button>
      </div>
    </div>
  )
}
