// The one place the outbound review of an external step's prompt reaches the user
// (items.id=884 stage 2, decisions.id=943 point 6). Generic: keyed by run id, no
// per-Focus screens. Mounted once by NavShell, so it works whichever pane a run was
// started from (no GUI starts an external Focus run today; the first one is the
// hand check, which calls submit_focus_run from the console).
//
// WHAT IT SHOWS, ALL FROM THE BACKEND'S ANSWER (see stepReview.ts):
//   * the Privacy Guardian modal, with the flagged spans of the review a run is parked
//     on. The user's answers are recorded and the run resumed; the SERVER applies them
//     to the text it renders and sends. "Don't send" refuses: the run is cancelled.
//   * a small notice when the answers could not be recorded (Try again / Don't send),
//     or when the review could not be brought back up (Check again).
// A review the backend reports as needing a resume (the app restarted) is resumed
// automatically so it is asked again, and a modal whose run has no pending review any
// more is dismissed. Neither shows anything.
//
// THE NATIVE PANE. A Cloud Chat provider pane is composited by Rust above the webview,
// so while anything of this host is on screen it registers a blocking layer
// (auth/blockingOverlay), which makes CloudChatAccessPane push an empty pane layout, the
// same hide ChatPane's copy modal gets. Released when the host shows nothing.
//
// Copy is placeholder (Chat-BRAND sets the wording in items.id=900).
import { useEffect } from 'react'
import { useTranslation } from 'react-i18next'
import { acquireBlockingOverlay } from '../auth/blockingOverlay'
import { PrivacyGuardianModal } from '../cloudChatAccess/PrivacyGuardianModal'
import './StepReviewHost.css'
import { errorKeyOf, modalPayload } from './stepReview'
import { useStepReview } from './useStepReview'

export interface StepReviewHostProps {
  userId: string
  personaId: string
  /** Runs a surface already knows are parked; see useStepReview. */
  knownRunIds?: readonly string[]
}

export function StepReviewHost({ userId, personaId, knownRunIds }: StepReviewHostProps) {
  const { t } = useTranslation()
  const review = useStepReview({ userId, personaId, knownRunIds })
  const current = review.shown
  const visible = current !== null

  useEffect(() => {
    if (!visible) return
    return acquireBlockingOverlay()
  }, [visible])

  if (!current) return null

  if (current.kind === 'modal') {
    const { runId } = current
    return (
      <PrivacyGuardianModal
        key={current.review.review_id}
        open
        payload={modalPayload(current.review, t('stepReview.focusContext'))}
        onResolve={(decisions) => review.answer(runId, decisions)}
        // The scanning state this cancels never occurs here: the modal only opens once
        // the backend has the spans.
        onCancel={() => undefined}
        onDecline={current.sending ? undefined : () => review.decline(runId)}
        labels={{
          focusContext: t('stepReview.focusContext'),
          declineButton: t('stepReview.declineButton'),
          confirmAllKeptPrivate: t('stepReview.confirmAllKeptPrivate'),
        }}
      />
    )
  }

  if (current.kind === 'error') {
    const { runId } = current
    return (
      <div className="step-review-notice-overlay">
        <div className="step-review-notice" role="alertdialog" aria-modal="true">
          <h2 className="step-review-notice__title">{t('stepReview.notice.errorTitle')}</h2>
          <p className="step-review-notice__body">
            {errorText(t, current.message)} {t('stepReview.notice.errorNothingSent')}
          </p>
          <div className="step-review-notice__actions">
            <button type="button" onClick={() => review.decline(runId)}>
              {t('stepReview.notice.dontSend')}
            </button>
            {current.canRetry && (
              <button type="button" onClick={() => review.retrySend(runId)}>
                {t('stepReview.notice.tryAgain')}
              </button>
            )}
          </div>
        </div>
      </div>
    )
  }

  const { runId } = current
  return (
    <div className="step-review-notice-overlay">
      <div className="step-review-notice" role="alertdialog" aria-modal="true">
        <h2 className="step-review-notice__title">{t('stepReview.notice.stuckTitle')}</h2>
        <p className="step-review-notice__body">{t('stepReview.notice.stuckBody')}</p>
        <div className="step-review-notice__actions">
          <button type="button" onClick={() => review.dismiss(runId)}>
            {t('stepReview.notice.close')}
          </button>
          <button type="button" onClick={() => review.recheck(runId)}>
            {t('stepReview.notice.checkAgain')}
          </button>
        </div>
      </div>
    </div>
  )
}

/** The command's own error code as copy. Literal keys, so the consistency test sees
 *  each one used. */
function errorText(t: (key: string) => string, code: string): string {
  switch (errorKeyOf(code)) {
    case 'persona_not_found':
      return t('step_review.error.persona_not_found')
    case 'invalid_run_id':
      return t('step_review.error.invalid_run_id')
    case 'step_review_unavailable':
      return t('step_review.error.step_review_unavailable')
  }
}
