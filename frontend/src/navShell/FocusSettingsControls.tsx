// items.id=321 / decisions.id=729 -- shared control for raising a Focus's
// privacy_tier / max_permitted_tier, used two ways: the full settings
// screen (FocusSettingsPane, mode="full") and the inline "raise it now"
// affordance Tier3AccessPane mounts in place of Gate3's dead
// "[Change Focus settings]" block text (mode="ceilingOnly", just the one
// field). One component, not two, so the friction-gate-confirm logic below
// only has to be written once.
//
// FrictionGateDetail is hand-declared here, not generated: update_focus_
// settings returns Result<FocusInfo, String>, and the gate trip is a
// JSON-serialized FrictionGateDetail *inside* that Err(String) -- not a
// distinct typed error -- so tauri-specta's collect_commands! never sees it
// even though the Rust struct derives specta::Type. Same convention
// PrivacyGuardianModal.tsx uses for ConsentRequestPayload (see that file's
// header comment). Keep in sync by hand with commands/persona.rs's
// FrictionGateDetail.

import { useCallback, useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { commands, type FocusInfo, type UpdateFocusSettingsRequest } from '../bindings'

export interface FrictionGateDetail {
  persona_id: string
  focus_id: string
  requested_privacy_tier: number | null
  requested_focus_profile: string | null
  requested_max_permitted_tier: number | null
  existing_privacy_tier: number
  existing_focus_profile: string
  existing_max_permitted_tier: number
  privacy_would_loosen: boolean
  moves_to_protected: boolean
  max_permitted_tier_would_loosen: boolean
}

function parseFrictionGateDetail(errorText: string): FrictionGateDetail | null {
  try {
    const parsed: unknown = JSON.parse(errorText)
    if (
      parsed !== null &&
      typeof parsed === 'object' &&
      'persona_id' in parsed &&
      'privacy_would_loosen' in parsed &&
      'max_permitted_tier_would_loosen' in parsed
    ) {
      return parsed as FrictionGateDetail
    }
  } catch {
    // Not JSON -- a plain error string (not_found, tier bounds check, ...).
  }
  return null
}

const TIER_VALUES = [1, 2, 3] as const

export type FocusSettingsControlsMode = 'full' | 'ceilingOnly'

export interface FocusSettingsControlsProps {
  userId: string
  personaId: string
  focusId: string
  /** "full": both privacy_tier and max_permitted_tier, one Save, both
   *  fields always sent together (decisions.id=729's batching requirement,
   *  so a combined loosen produces one friction-gate prompt, not two).
   *  "ceilingOnly": just max_permitted_tier, labeled "Raise" -- for the
   *  inline Gate3-blocked affordance, which has no reason to also expose
   *  privacy_tier. */
  mode: FocusSettingsControlsMode
  /** ceilingOnly only: prefills the select, e.g. from Gate3Result's own
   *  target_tier on the block that mounted this control. */
  suggestedMaxPermittedTier?: number
  /** Fires after a save applies cleanly, whether directly or via the
   *  friction-gate's "proceed" resolution -- callers use this to retry
   *  whatever action the old settings had blocked. */
  onSaved?: (info: FocusInfo) => void
}

export function FocusSettingsControls({
  userId,
  personaId,
  focusId,
  mode,
  suggestedMaxPermittedTier,
  onSaved,
}: FocusSettingsControlsProps) {
  const { t } = useTranslation()
  const [settings, setSettings] = useState<FocusInfo | null>(null)
  const [draftPrivacyTier, setDraftPrivacyTier] = useState(2)
  const [draftMaxPermittedTier, setDraftMaxPermittedTier] = useState(2)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [saveError, setSaveError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  const [pendingGate, setPendingGate] = useState<FrictionGateDetail | null>(null)
  const [pendingRequest, setPendingRequest] = useState<UpdateFocusSettingsRequest | null>(null)

  useEffect(() => {
    let cancelled = false
    setSettings(null)
    setLoadError(null)
    commands.getFocusSettings(userId, personaId, focusId).then((result) => {
      if (cancelled) return
      if (result.status === 'ok') {
        setSettings(result.data)
        setDraftPrivacyTier(result.data.privacy_tier)
        setDraftMaxPermittedTier(suggestedMaxPermittedTier ?? result.data.max_permitted_tier)
      } else {
        setLoadError(result.error)
      }
    })
    return () => {
      cancelled = true
    }
  }, [userId, personaId, focusId, suggestedMaxPermittedTier])

  const applyResult = useCallback(
    (info: FocusInfo) => {
      setSettings(info)
      setDraftPrivacyTier(info.privacy_tier)
      setDraftMaxPermittedTier(info.max_permitted_tier)
      setPendingGate(null)
      setPendingRequest(null)
      setSaveError(null)
      onSaved?.(info)
    },
    [onSaved],
  )

  const handleSave = () => {
    const request: UpdateFocusSettingsRequest = {
      persona_id: personaId,
      focus_id: focusId,
      context_flow: null,
      library_visibility: null,
      privacy_tier: mode === 'full' ? draftPrivacyTier : null,
      max_permitted_tier: draftMaxPermittedTier,
      focus_profile: null,
    }
    setSaving(true)
    setSaveError(null)
    commands.updateFocusSettings(userId, request).then((result) => {
      setSaving(false)
      if (result.status === 'ok') {
        applyResult(result.data)
        return
      }
      const detail = parseFrictionGateDetail(result.error)
      if (detail) {
        setPendingGate(detail)
        setPendingRequest(request)
      } else {
        setSaveError(result.error)
      }
    })
  }

  const handleGateDecision = (decision: 'proceed' | 'cancel') => {
    if (!pendingRequest) return
    setSaving(true)
    commands
      .submitFrictionGateDecision({ decision, original_request: pendingRequest })
      .then((result) => {
        setSaving(false)
        if (result.status !== 'ok') {
          setSaveError(result.error)
          return
        }
        if (result.data) {
          applyResult(result.data)
          return
        }
        // decision === 'cancel' -- backend still recorded it for audit;
        // revert the draft to the last-known-saved values.
        setPendingGate(null)
        setPendingRequest(null)
        if (settings) {
          setDraftPrivacyTier(settings.privacy_tier)
          setDraftMaxPermittedTier(settings.max_permitted_tier)
        }
      })
  }

  if (loadError) {
    return <p role="alert">{t('navShell.focusSettings.loadError', { message: loadError })}</p>
  }
  if (!settings) {
    return <p>{t('navShell.focusSettings.loading')}</p>
  }

  const privacyTierId = `focus-settings-privacy-tier-${personaId}-${focusId}`
  const maxPermittedTierId = `focus-settings-max-permitted-tier-${personaId}-${focusId}`

  return (
    <div className="focus-settings-controls">
      {mode === 'full' && (
        <div>
          <label htmlFor={privacyTierId}>{t('navShell.focusSettings.privacyTierLabel')}</label>
          <select
            id={privacyTierId}
            value={draftPrivacyTier}
            onChange={(event) => setDraftPrivacyTier(Number(event.target.value))}
          >
            {TIER_VALUES.map((tier) => (
              <option key={tier} value={tier}>
                {t(`navShell.focusSettings.tier${tier}Label`)}
              </option>
            ))}
          </select>
        </div>
      )}

      <div>
        <label htmlFor={maxPermittedTierId}>
          {t('navShell.focusSettings.maxPermittedTierLabel')}
        </label>
        <select
          id={maxPermittedTierId}
          value={draftMaxPermittedTier}
          onChange={(event) => setDraftMaxPermittedTier(Number(event.target.value))}
        >
          {TIER_VALUES.map((tier) => (
            <option key={tier} value={tier}>
              {t(`navShell.focusSettings.tier${tier}Label`)}
            </option>
          ))}
        </select>
      </div>

      <button type="button" onClick={handleSave} disabled={saving}>
        {t(mode === 'full' ? 'navShell.focusSettings.saveButton' : 'navShell.focusSettings.raiseButton')}
      </button>

      {saveError && (
        <p role="alert">{t('navShell.focusSettings.saveError', { message: saveError })}</p>
      )}

      {pendingGate && (
        <div role="alertdialog" className="focus-settings-controls__gate-confirm">
          <p>{t('navShell.focusSettings.gateConfirmHeading')}</p>
          <ul>
            {pendingGate.privacy_would_loosen && (
              <li>
                {t('navShell.focusSettings.gateConfirmPrivacyTier', {
                  from: pendingGate.existing_privacy_tier,
                  to: pendingGate.requested_privacy_tier,
                })}
              </li>
            )}
            {pendingGate.moves_to_protected && (
              <li>{t('navShell.focusSettings.gateConfirmProtected')}</li>
            )}
            {pendingGate.max_permitted_tier_would_loosen && (
              <li>
                {t('navShell.focusSettings.gateConfirmMaxPermittedTier', {
                  from: pendingGate.existing_max_permitted_tier,
                  to: pendingGate.requested_max_permitted_tier,
                })}
              </li>
            )}
          </ul>
          <button type="button" onClick={() => handleGateDecision('proceed')} disabled={saving}>
            {t('navShell.focusSettings.confirmProceed')}
          </button>
          <button type="button" onClick={() => handleGateDecision('cancel')} disabled={saving}>
            {t('navShell.focusSettings.confirmCancel')}
          </button>
        </div>
      )}
    </div>
  )
}
