// The one password input (items.id=777): a masked field with an eye button that
// toggles it to plain text. Used for every password input in the app, so none
// can forget the toggle or the reset.
//
// The field is controlled by its parent; this component keeps only whether the
// text is currently shown. That flag is never persisted, logged or sent
// anywhere. It returns to masked when the form is submitted, when the field is
// disabled (a submit in flight, shown masked) and, because the state dies with the component,
// when it unmounts. The label stays with the parent (`htmlFor` = `id`), which
// owns the layout; the eye button has its own accessible name and pressed state.

import './PasswordField.css'
import { useEffect, useRef, useState, type InputHTMLAttributes } from 'react'
import { useTranslation } from 'react-i18next'

export type PasswordFieldProps = Omit<InputHTMLAttributes<HTMLInputElement>, 'type'> & {
  id: string
}

function EyeIcon({ crossed }: { crossed: boolean }) {
  return (
    <svg
      width="16"
      height="16"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      <path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12z" />
      <circle cx="12" cy="12" r="3" />
      {crossed && <path d="M4 4l16 16" />}
    </svg>
  )
}

export function PasswordField({ className, disabled, ...inputProps }: PasswordFieldProps) {
  const { t } = useTranslation()
  const [shown, setShown] = useState(false)
  const inputRef = useRef<HTMLInputElement>(null)

  // Back to masked as soon as the surrounding form is submitted.
  useEffect(() => {
    const form = inputRef.current?.form
    if (!form) return
    const mask = () => setShown(false)
    form.addEventListener('submit', mask)
    return () => form.removeEventListener('submit', mask)
  }, [])

  // A disabled field (a submit in flight) always shows masked.
  const revealed = shown && !disabled

  return (
    <span className="password-field">
      <input
        {...inputProps}
        ref={inputRef}
        type={revealed ? 'text' : 'password'}
        className={className}
        disabled={disabled}
        autoCapitalize="none"
        autoCorrect="off"
        spellCheck={false}
      />
      <button
        type="button"
        className="password-field__toggle"
        aria-label={t('auth.passwordField.show')}
        aria-pressed={revealed}
        aria-controls={inputProps.id}
        disabled={disabled}
        onClick={() => setShown((value) => !value)}
      >
        <EyeIcon crossed={revealed} />
      </button>
    </span>
  )
}
