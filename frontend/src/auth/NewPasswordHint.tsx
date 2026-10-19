// The rule line and live character count under a NEW-password field
// (items.id=777, decisions.id=858): one plain sentence and a count that turns to
// OK at the minimum. There is no per-rule checklist because the policy has only
// one rule the person can act on. Shown on Change password and Restore access
// only; the Interrupted-change and sign-in fields type a password that already
// exists and have no rules to show.
//
// The count is advisory (the server decides). It is plain text that changes as
// the person types; a screen reader hears the OK state once, through a hidden
// status that changes only when the OK state flips, never per keystroke.

import './NewPasswordHint.css'
import { useTranslation } from 'react-i18next'
import { passwordCount } from './passwordRules'

export interface NewPasswordHintProps {
  /** Id the field's aria-describedby points at. */
  id: string
  password: string
}

export function NewPasswordHint({ id, password }: NewPasswordHintProps) {
  const { t } = useTranslation()
  const { count, min, ok } = passwordCount(password)
  return (
    <div className="new-password-hint" id={id}>
      <span className="new-password-hint__rule">{t('auth.passwordRules.rule', { min })}</span>
      <span
        className={ok ? 'new-password-hint__count new-password-hint__count--ok' : 'new-password-hint__count'}
      >
        {ok ? t('auth.passwordRules.countOk', { count }) : t('auth.passwordRules.count', { count, min })}
      </span>
      <output className="new-password-hint__status">
        {ok ? t('auth.passwordRules.reachedMinimum', { min }) : ''}
      </output>
    </div>
  )
}
