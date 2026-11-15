// The collection-consent section of ChatPane (items.id=920 stage 3,
// decisions.id=944). Rendered only for a Focus that asks (usesCollectionConsent):
//
//   * on opening it reads get_collection_consent_state and shows the confirm
//     ONLY when the backend says needs_decision (not_asked, or granted or
//     declined for an older text; never a declined user at the current text and
//     never a withdrawn user). The frontend does not recompute that.
//   * otherwise a small "Tech Support memory" button opens the memory screen,
//     where the user can stop remembering (with the plain statement that what
//     QuietRabbit already knows stays until they delete it) or allow it again.
//   * Tech Support stays usable whatever the answer: the confirm never blocks the
//     input, and not saving is what the backend already does without consent.
import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import { CollectionConsentConfirm } from './CollectionConsentConfirm'
import { CollectionConsentManage } from './CollectionConsentManage'
import { DeviceLibrary } from './DeviceLibrary'
import { shouldAskForConsent } from './collectionConsent'
import { useCollectionConsent } from './useCollectionConsent'
import './CollectionConsentSection.css'

export interface CollectionConsentSectionProps {
  personaId: string
  focusId: string
}

export function CollectionConsentSection({ personaId, focusId }: CollectionConsentSectionProps) {
  const { t } = useTranslation()
  const consent = useCollectionConsent(personaId, focusId)
  const [manageOpen, setManageOpen] = useState(false)
  // items.id=891: the device library, a second small entry beside the memory one.
  const [devicesOpen, setDevicesOpen] = useState(false)

  if (consent.status === 'idle' || consent.status === 'loading') return null

  if (consent.status === 'error' || !consent.state) {
    return (
      <p role="alert" className="collection-consent-section__check-error">
        {t('navShell.chat.collectionConsent.checkError', { message: consent.loadError ?? '' })}
      </p>
    )
  }

  const state = consent.state
  if (shouldAskForConsent(state)) {
    return (
      <CollectionConsentConfirm
        state={state}
        busy={consent.busy}
        error={consent.actionError}
        onDecide={(decision) => void consent.decide(decision)}
      />
    )
  }

  if (devicesOpen) {
    return (
      <DeviceLibrary
        personaId={personaId}
        focusId={focusId}
        onClose={() => setDevicesOpen(false)}
      />
    )
  }

  if (manageOpen) {
    return (
      <CollectionConsentManage
        personaId={personaId}
        focusId={focusId}
        state={state}
        busy={consent.busy}
        error={consent.actionError}
        onStop={() => void consent.withdraw()}
        onAllow={() => void consent.decide('granted')}
        onClose={() => setManageOpen(false)}
      />
    )
  }

  return (
    <div className="collection-consent-section__row">
      <button
        type="button"
        className="collection-consent-section__manage-button"
        onClick={() => setManageOpen(true)}
      >
        {t('navShell.chat.collectionConsent.manage.button')}
      </button>
      <button
        type="button"
        className="collection-consent-section__manage-button"
        onClick={() => setDevicesOpen(true)}
      >
        {t('navShell.chat.devices.button')}
      </button>
    </div>
  )
}
