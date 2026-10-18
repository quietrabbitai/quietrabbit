// Account security panel (items.id=769 stage 2): a blocking layer opened from
// the corner button in NavShell, drawn above the dock. It holds the blocking
// store while open, so the native Cloud Chat panes (which Rust composites above
// the webview) step aside the same way they do for the copy modal.
//
// It cannot be closed or signed out of while a change is running (`busy`); the
// key-rotation overlay sits above it for the duration.

import './AccountSecurityScreen.css'
import { useCallback, useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { commands } from '../bindings'
import { acquireBlockingOverlay } from './blockingOverlay'
import { ChangePasswordForm } from './ChangePasswordForm'
import { NewRecoveryPhraseFlow } from './NewRecoveryPhraseFlow'
import type { SignOutNotice } from './rotationErrors'
import { useDialogFocus } from './useDialogFocus'

export interface AccountSecurityScreenProps {
  onClose: () => void
  /** Sign the user out (optionally leaving a message on the login screen). */
  onSignOut: (notice?: SignOutNotice) => void
}

export function AccountSecurityScreen({ onClose, onSignOut }: AccountSecurityScreenProps) {
  const { t } = useTranslation()
  const panelRef = useRef<HTMLDialogElement>(null)
  const [busy, setBusy] = useState(false)
  const [displayName, setDisplayName] = useState('')
  const [newPhraseOpen, setNewPhraseOpen] = useState(false)

  useEffect(() => acquireBlockingOverlay(), [])

  useEffect(() => {
    let cancelled = false
    void commands.getSession().then((result) => {
      if (!cancelled && result.status === 'ok' && result.data !== null) {
        setDisplayName(result.data.display_name)
      }
    })
    return () => {
      cancelled = true
    }
  }, [])

  const handleEscape = useCallback(() => {
    if (!busy) onClose()
  }, [busy, onClose])
  // While busy the rotation overlay owns the keyboard.
  useDialogFocus(panelRef, !busy, handleEscape)

  return (
    <div className="account-security">
      {/* Non-modal <dialog open>, not showModal(): see useDialogFocus. */}
      <dialog
        ref={panelRef}
        open
        className="account-security__panel"
        aria-modal="true"
        aria-labelledby="account-security-title"
        tabIndex={-1}
      >
        <h2 id="account-security-title" className="account-security__title">
          {t('auth.accountSecurity.title')}
        </h2>

        <section className="account-security__section">
          <h3 className="account-security__heading">{t('auth.accountSecurity.changePassword.heading')}</h3>
          <ChangePasswordForm displayName={displayName} onBusyChange={setBusy} onSignOut={onSignOut} />
        </section>

        <section className="account-security__section">
          <h3 className="account-security__heading">{t('auth.accountSecurity.recoveryPhrase.heading')}</h3>
          {newPhraseOpen ? (
            <NewRecoveryPhraseFlow
              onClose={() => setNewPhraseOpen(false)}
              onBusyChange={setBusy}
              onSignOut={onSignOut}
            />
          ) : (
            <>
              <p className="account-security__text">{t('auth.accountSecurity.recoveryPhrase.intro')}</p>
              <button type="button" onClick={() => setNewPhraseOpen(true)} disabled={busy}>
                {t('auth.accountSecurity.recoveryPhrase.open')}
              </button>
            </>
          )}
        </section>

        <div className="account-security__footer">
          <button type="button" onClick={() => onSignOut()} disabled={busy}>
            {t('auth.accountSecurity.signOut')}
          </button>
          <button type="button" onClick={onClose} disabled={busy}>
            {t('auth.accountSecurity.close')}
          </button>
        </div>
      </dialog>
    </div>
  )
}
