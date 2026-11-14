// The one-time collection consent a focus_level Focus (Tech Support) asks for
// (items.id=920 stage 3, decisions.id=944, decisions.id=509).
//
// No generic confirm dialog exists, so this is a small controlled component
// modelled on CrossPersonaConfirmModal.tsx: an inline alertdialog, no backend
// calls of its own. CollectionConsentSection owns the state and the commands.
//
// THE EXPLANATION IS FIXED LOCAL TEXT. Every sentence is an i18n string chosen
// by id (the promise ids come from the backend's text data, block_data/
// collection_consent_text.json). Nothing here is model-generated and nothing is a
// chat message. The copy is PLACEHOLDER, owned by Chat-BRAND (items.id=839).
//
// ONE confirm control, two answers: "Allow Tech Support to remember" and "Use
// without saving". Both send the version of the text DISPLAYED here
// (state.current_text_version) to submit_collection_consent_decision. Declining
// leaves Tech Support usable; it just saves nothing.
import { useTranslation } from 'react-i18next'
import type { CollectionConsentDecision, CollectionConsentState } from '../bindings'
import type { ConsentActionError } from './useCollectionConsent'
import './CollectionConsentConfirm.css'

export interface CollectionConsentConfirmProps {
  state: CollectionConsentState
  busy: boolean
  error: ConsentActionError | null
  onDecide: (decision: CollectionConsentDecision) => void
}

export function CollectionConsentConfirm({ state, busy, error, onDecide }: CollectionConsentConfirmProps) {
  const { t } = useTranslation()
  // Asked before (granted or declined for an older text): say the text changed.
  const askedBefore = state.consent !== 'not_asked'

  return (
    <div
      className="collection-consent-confirm"
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="collection-consent-confirm-title"
    >
      <p id="collection-consent-confirm-title" className="collection-consent-confirm__title">
        {t('navShell.chat.collectionConsent.title')}
      </p>
      {askedBefore && (
        <p className="collection-consent-confirm__changed">
          {t('navShell.chat.collectionConsent.changed')}
        </p>
      )}
      <p className="collection-consent-confirm__intro">{t('navShell.chat.collectionConsent.intro')}</p>
      <ul className="collection-consent-confirm__promises">
        {state.promises.map((id) => (
          <li key={id} className="collection-consent-confirm__promise">
            {t(`navShell.chat.collectionConsent.promise.${id}`)}
          </li>
        ))}
      </ul>
      <p className="collection-consent-confirm__decline-note">
        {t('navShell.chat.collectionConsent.declineNote')}
      </p>
      {error && (
        <p role="alert" className="collection-consent-confirm__error">
          {error.kind === 'stale' && t('navShell.chat.collectionConsent.error.stale')}
          {error.kind === 'unavailable' && t('navShell.chat.collectionConsent.error.unavailable')}
          {(error.kind === 'other' || error.kind === 'withdraw-pending') &&
            t('navShell.chat.collectionConsent.error.other', { message: error.message })}
        </p>
      )}
      <div className="collection-consent-confirm__footer">
        <button type="button" disabled={busy} onClick={() => onDecide('declined')}>
          {t('navShell.chat.collectionConsent.decline')}
        </button>
        <button
          type="button"
          className="collection-consent-confirm__allow"
          disabled={busy}
          onClick={() => onDecide('granted')}
        >
          {busy ? t('navShell.chat.collectionConsent.saving') : t('navShell.chat.collectionConsent.allow')}
        </button>
      </div>
    </div>
  )
}
