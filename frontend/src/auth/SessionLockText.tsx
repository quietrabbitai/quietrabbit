// The login-screen line shown after the backend ended the session
// (items.id=775). The reason-to-copy table lives in sessionLock.ts; the t()
// call sits here with an opaque `${message.suffix}` hole, the shape
// styleAndI18nConsistency.test.ts can resolve (same split as RotationErrorText).

import { useTranslation } from 'react-i18next'
import { lockNoticeSuffix } from './sessionLock'

export interface SessionLockTextProps {
  reason: string
}

export function SessionLockText({ reason }: SessionLockTextProps) {
  const { t } = useTranslation()
  const message = { suffix: lockNoticeSuffix(reason) }
  return <p role="alert">{t(`auth.sessionLocked.${message.suffix}`)}</p>
}
