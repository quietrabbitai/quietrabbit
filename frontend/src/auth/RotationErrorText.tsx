// The one place a KeyRotationError becomes text (items.id=769 stage 1). Every
// credential-change form renders its failures through this, so the wording
// for each code lives in en.json under auth.rotationErrors and in the table in
// rotationErrors.ts, never in a form. Renders nothing for a null error.

import './RotationErrorText.css'
import { useTranslation } from 'react-i18next'
import type { KeyRotationError } from '../bindings'
import { rotationErrorMessage, type RotationErrorContext } from './rotationErrors'

export interface RotationErrorTextProps {
  error: KeyRotationError | null
  context: RotationErrorContext
}

export function RotationErrorText({ error, context }: RotationErrorTextProps) {
  const { t } = useTranslation()
  if (error === null) return null
  const message = rotationErrorMessage(error, context)
  return (
    <p role="alert" className="rotation-error-text">
      {t(`auth.rotationErrors.${message.suffix}`, message.params)}
    </p>
  )
}
