// The password policy of decisions.id=858, as the frontend sees it
// (items.id=777). Pure: no React, no IPC, covered by passwordRules.test.ts.
// items.id=637's Create account screen reuses this module; do not copy it.
//
// ADVISORY ONLY. The server (src-tauri/src/auth/password_policy.rs) is the
// authority and enforces the same rules again, plus the common/breached
// password list, which is deliberately NOT duplicated here: that list lives in
// the Rust binary, and no password is sent over IPC while it is being typed. A
// blocked password is reported by the server on submit (new_password_blocked).
//
// Rules (mirrored in Rust; keep them in step):
//   - at least MIN_PASSWORD_LENGTH characters, no upper cap, spaces allowed,
//     no composition rules;
//   - characters are Unicode code points of the NFKC form (decisions.id=900:
//     the key derivation normalises to NFKC, so the rules judge the same
//     text), never bytes and never UTF-16 units (`.length`, which counts an
//     emoji as 2);
//   - nothing is trimmed: leading and trailing spaces belong to the password.

/** decisions.id=858. Mirrors MIN_PASSWORD_LENGTH in password_policy.rs. */
export const MIN_PASSWORD_LENGTH = 15

/** Length of a password in Unicode code points of its NFKC form. */
export function passwordLength(password: string): number {
  let count = 0
  for (const _ of password.normalize('NFKC')) count += 1
  return count
}

export type NewPasswordProblem = 'empty' | 'too_short' | 'mismatch'

/** The live count shown while a new password is typed. */
export interface PasswordCount {
  count: number
  min: number
  /** At least the minimum: the count turns to OK. */
  ok: boolean
}

export function passwordCount(password: string): PasswordCount {
  const count = passwordLength(password)
  return { count, min: MIN_PASSWORD_LENGTH, ok: count >= MIN_PASSWORD_LENGTH }
}

/**
 * The first thing wrong with a new password and its confirmation, or null.
 * Order: empty, too short, mismatch. The confirmation is compared in NFKC form:
 * two typings that derive the same key are the same password.
 */
export function validateNewPassword(next: string, confirm: string): NewPasswordProblem | null {
  if (next.length === 0) return 'empty'
  if (passwordLength(next) < MIN_PASSWORD_LENGTH) return 'too_short'
  if (next.normalize('NFKC') !== confirm.normalize('NFKC')) return 'mismatch'
  return null
}
