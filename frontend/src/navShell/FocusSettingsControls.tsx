// items.id=321 / decisions.id=729 -- shared control for raising a Focus's
// privacy_tier / max_permitted_tier, used two ways: the full settings
// screen (FocusSettingsPane, mode="full") and the inline "raise it now"
// affordance CloudChatAccessPane mounts in place of Gate3's dead
// "[Change Focus settings]" block text (mode="ceilingOnly", just the one
// field). One component, not two, so the friction-gate-confirm logic below
// only has to be written once.
//
// items.id=533: FrictionGateDetail and its parser moved to
// ./frictionGateDetail.ts (a non-JSX module, importable by a plain Node
// test) -- see that file's own header for why it's hand-declared rather
// than generated, and for the sync-with-persona.rs convention.
//
// items.id=448: max_permitted_tier fields (here and throughout this file)
// retyped from a 1/2/3 ordinal to the 4-value ExternalAccess enum
// (local_only/anonymous_required/anonymous_preferred/unrestricted),
// matching commands/persona.rs's live retype.
// items.id=533: privacy_tier fields retyped the same way, to the 3-value
// PrivacyPreference enum (red/yellow/green).

import { useCallback, useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import {
  commands,
  type ExternalAccess,
  type FocusInfo,
  type PrivacyPreference,
  type UpdateFocusSettingsRequest,
} from '../bindings'
import { parseFrictionGateDetail, type FrictionGateDetail } from './frictionGateDetail.ts'

const PRIVACY_TIER_VALUES: PrivacyPreference[] = ['red', 'yellow', 'green']

// items.id=533: labels re-keyed from the old navShell.focusSettings.tier${n}Label
// dynamic-key form to one key per variant (tierRedLabel/tierYellowLabel/
// tierGreenLabel) -- English text carried over unchanged, including the
// retired "Tier 1.5/2" wording in tierYellowLabel; that copy fix is a
// separate, deliberately untouched decision (see en.json).
function privacyTierLabelKey(value: PrivacyPreference): string {
  switch (value) {
    case 'red':
      return 'navShell.focusSettings.tierRedLabel'
    case 'yellow':
      return 'navShell.focusSettings.tierYellowLabel'
    case 'green':
      return 'navShell.focusSettings.tierGreenLabel'
  }
}

// items.id=448: max_permitted_tier's 4 ExternalAccess values, replacing the
// old shared TIER_VALUES=[1,2,3] this select used to reuse from the
// privacy-tier list. anonymous_preferred is a genuinely new option with no
// legacy 1/2/3 slot (PROVIDER_REGISTRY_AND_TIER_MODEL_SPEC.md Part 6b).
const MAX_PERMITTED_TIER_VALUES: ExternalAccess[] = [
  'local_only',
  'anonymous_required',
  'anonymous_preferred',
  'unrestricted',
]

// items.id=448: plain-language labels, not "anonymous"/"external" (backend
// vocabulary) -- mirrors the plain-language pattern already shipped on
// cloudChatSelector's own badges ("No login required" / "Account required,
// data retained"). Decided live with Jason this session; see this draft's
// own "Vocabulary decisions" section for the full reasoning trail.
function maxPermittedTierLabelKey(value: ExternalAccess): string {
  switch (value) {
    case 'local_only':
      return 'navShell.focusSettings.maxPermittedTierLocalOnlyLabel'
    case 'anonymous_required':
      return 'navShell.focusSettings.maxPermittedTierAnonymousRequiredLabel'
    case 'anonymous_preferred':
      return 'navShell.focusSettings.maxPermittedTierAnonymousPreferredLabel'
    case 'unrestricted':
      return 'navShell.focusSettings.maxPermittedTierUnrestrictedLabel'
  }
}

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
   *  target_tier on the block that mounted this control, converted to its
   *  ExternalAccess equivalent by the caller (items.id=448 -- target_tier
   *  itself stays a plain number; see CloudChatAccessPane.tsx's own
   *  externalAccessFromLegacyTier helper). */
  suggestedMaxPermittedTier?: ExternalAccess
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
  // items.id=533: default 'yellow' -- the practical equivalent of the old
  // numeric default (2). Overwritten immediately once real settings load
  // (see the effect below), same as the old numeric default was.
  const [draftPrivacyTier, setDraftPrivacyTier] = useState<PrivacyPreference>('yellow')
  // items.id=448: default 'anonymous_required' -- the practical equivalent
  // of the old numeric default (2), via ExternalAccess::from_legacy_tier's
  // own 2->AnonymousRequired mapping. Overwritten immediately once real
  // settings load (see the effect below), same as the old numeric default
  // was.
  const [draftMaxPermittedTier, setDraftMaxPermittedTier] =
    useState<ExternalAccess>('anonymous_required')
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
            onChange={(event) => setDraftPrivacyTier(event.target.value as PrivacyPreference)}
          >
            {PRIVACY_TIER_VALUES.map((tier) => (
              <option key={tier} value={tier}>
                {t(privacyTierLabelKey(tier))}
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
          onChange={(event) => setDraftMaxPermittedTier(event.target.value as ExternalAccess)}
        >
          {MAX_PERMITTED_TIER_VALUES.map((tier) => (
            <option key={tier} value={tier}>
              {t(maxPermittedTierLabelKey(tier))}
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
                  from: t(privacyTierLabelKey(pendingGate.existing_privacy_tier)),
                  to: t(
                    privacyTierLabelKey(
                      pendingGate.requested_privacy_tier ?? pendingGate.existing_privacy_tier,
                    ),
                  ),
                })}
              </li>
            )}
            {pendingGate.moves_to_protected && (
              <li>{t('navShell.focusSettings.gateConfirmProtected')}</li>
            )}
            {pendingGate.max_permitted_tier_would_loosen && (
              <li>
                {t('navShell.focusSettings.gateConfirmMaxPermittedTier', {
                  from: t(maxPermittedTierLabelKey(pendingGate.existing_max_permitted_tier)),
                  to: t(
                    maxPermittedTierLabelKey(
                      pendingGate.requested_max_permitted_tier ??
                        pendingGate.existing_max_permitted_tier,
                    ),
                  ),
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
