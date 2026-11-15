// The Tech Support memory screen (items.id=920 stage 3): where the user stops
// Tech Support remembering, or allows it again. Controlled for the consent
// answers: CollectionConsentSection owns those calls. items.id=891 stage 7 adds
// the separate DeleteEverythingControl below the status, which owns its own call
// (stopping keeps what is already known, deleting is its own step). Placeholder
// copy, Chat-BRAND.
//
// THE WITHDRAWAL SAYS PLAINLY that stopping does not delete anything: what
// QuietRabbit already knows stays until the user deletes it. That sentence is
// shown in every state of this screen, so it is read before the user presses Stop.
//
// A withdrawal whose record could not be written (collection is already off) keeps
// the Stop button visible with an explanation, so pressing it again finishes it.
import { useTranslation } from 'react-i18next'
import type { CollectionConsentState } from '../bindings'
import type { ConsentActionError } from './useCollectionConsent'
import { DeleteEverythingControl } from './DeleteEverythingControl'
import './CollectionConsentManage.css'

export interface CollectionConsentManageProps {
  personaId: string
  focusId: string
  state: CollectionConsentState
  busy: boolean
  error: ConsentActionError | null
  onStop: () => void
  onAllow: () => void
  onClose: () => void
}

export function CollectionConsentManage({
  personaId,
  focusId,
  state,
  busy,
  error,
  onStop,
  onAllow,
  onClose,
}: CollectionConsentManageProps) {
  const { t } = useTranslation()
  const granted = state.consent === 'granted'
  const finishingWithdrawal = error?.kind === 'withdraw-pending'

  return (
    <section className="collection-consent-manage" aria-labelledby="collection-consent-manage-heading">
      <h3 id="collection-consent-manage-heading" className="collection-consent-manage__heading">
        {t('navShell.chat.collectionConsent.manage.heading')}
      </h3>
      <p className="collection-consent-manage__status">
        {granted && t('navShell.chat.collectionConsent.manage.statusGranted')}
        {state.consent === 'declined' && t('navShell.chat.collectionConsent.manage.statusDeclined')}
        {state.consent === 'withdrawn' && t('navShell.chat.collectionConsent.manage.statusWithdrawn')}
        {state.consent === 'not_asked' && t('navShell.chat.collectionConsent.manage.statusNotAsked')}
      </p>
      <p className="collection-consent-manage__stays">
        {t('navShell.chat.collectionConsent.manage.staysUntilDeleted')}
      </p>
      {error && (
        <p role="alert" className="collection-consent-manage__error">
          {error.kind === 'withdraw-pending'
            ? t('navShell.chat.collectionConsent.manage.withdrawPending', { message: error.message })
            : t('navShell.chat.collectionConsent.error.other', { message: error.message })}
        </p>
      )}
      <DeleteEverythingControl personaId={personaId} focusId={focusId} />
      <div className="collection-consent-manage__footer">
        <button type="button" onClick={onClose}>
          {t('navShell.chat.collectionConsent.manage.close')}
        </button>
        {(granted || finishingWithdrawal) && (
          <button type="button" disabled={busy} onClick={onStop}>
            {busy
              ? t('navShell.chat.collectionConsent.manage.stopping')
              : t('navShell.chat.collectionConsent.manage.stop')}
          </button>
        )}
        {!granted && !finishingWithdrawal && (
          <button type="button" disabled={busy} onClick={onAllow}>
            {t('navShell.chat.collectionConsent.allow')}
          </button>
        )}
      </div>
    </section>
  )
}
