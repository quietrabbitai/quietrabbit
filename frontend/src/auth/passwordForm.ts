// Pure checks for the "new password" half of the change-password form
// (items.id=769 stage 2). No React; covered by passwordForm.test.ts.
//
// Deliberately minimal: the backend rejects only an empty new password
// (KeyRotationError new_password_empty) and applies no strength or length
// rule, so none is invented here. Passwords are compared and sent exactly as
// typed -- never trimmed, because leading or trailing spaces are legitimate
// characters of a password. (An <input type="password"> already drops line
// breaks from pasted text, so a pasted CRLF cannot reach these functions.)

export type NewPasswordProblem = 'empty' | 'mismatch'

export function newPasswordProblem(next: string, confirm: string): NewPasswordProblem | null {
  if (next.length === 0) return 'empty'
  if (next !== confirm) return 'mismatch'
  return null
}

/** The submit button is enabled once every field has something in it. */
export function canSubmitPasswordChange(current: string, next: string, confirm: string): boolean {
  return current.length > 0 && next.length > 0 && confirm.length > 0
}
