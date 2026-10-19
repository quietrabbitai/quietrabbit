// The visible notice for a write that failed (items.id=778). Mounted once at
// the App root, beside every boot state, so it shows on any screen. The wording
// is in en.json under auth.writeFailed; the kind-to-copy table is writeFailed.ts.

import './WriteFailedNotice.css'
import { useTranslation } from 'react-i18next'
import { writeFailedSuffix } from './writeFailed'

export interface WriteFailedNoticeProps {
  kinds: string[]
  onDismiss: () => void
}

export function WriteFailedNotice({ kinds, onDismiss }: WriteFailedNoticeProps) {
  const { t } = useTranslation()
  if (kinds.length === 0) return null
  return (
    <div role="alert" className="write-failed-notice">
      {kinds.map((kind) => {
        const message = { suffix: writeFailedSuffix(kind) }
        return (
          <p key={kind} className="write-failed-notice__text">
            {t(`auth.writeFailed.${message.suffix}`)}
          </p>
        )
      })}
      <button type="button" onClick={onDismiss}>
        {t('auth.writeFailed.dismiss')}
      </button>
    </div>
  )
}
