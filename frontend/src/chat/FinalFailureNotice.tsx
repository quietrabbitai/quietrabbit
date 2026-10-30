// The final-failure notice (items.id=831, decisions.id=925): what a failed
// reply looks like once the last retry of a hosted call has also failed. The
// sentence is the reply's stored text (the backend names the provider from its
// row); this adds a quiet clock mark and a real Try again button. Neutral by
// design: a clock in Slate blue, not an Ember warning moment (rule 17).
import { useTranslation } from 'react-i18next'
import './FinalFailureNotice.css'

export interface FinalFailureNoticeProps {
  /** The reply's stored text: the whole message. */
  sentence: string
  /** Disabled while a run is working (this reply's or any other's). */
  busy: boolean
  onTryAgain: () => void
}

export function FinalFailureNotice({ sentence, busy, onTryAgain }: FinalFailureNoticeProps) {
  const { t } = useTranslation()
  return (
    <div className="final-failure">
      <div className="final-failure__message">
        <svg
          className="final-failure__icon"
          width="20"
          height="20"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          strokeLinejoin="round"
          aria-hidden="true"
        >
          <circle cx="12" cy="12" r="9" />
          <path d="M12 7v5l3 2" />
        </svg>
        <p className="final-failure__text">{sentence}</p>
      </div>
      <div className="final-failure__actions">
        <button
          type="button"
          className="final-failure__try-again"
          disabled={busy}
          onClick={onTryAgain}
        >
          {t('navShell.chat.finalFailure.tryAgain')}
        </button>
      </div>
    </div>
  )
}
