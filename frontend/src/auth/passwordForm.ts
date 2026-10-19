// Pure check for the change-password form (items.id=769 stage 2). No React;
// covered by passwordForm.test.ts. The new-password rules (length, mismatch)
// live in passwordRules.ts since items.id=777; passwords are never trimmed.

/** The submit button is enabled once every field has something in it. */
export function canSubmitPasswordChange(current: string, next: string, confirm: string): boolean {
  return current.length > 0 && next.length > 0 && confirm.length > 0
}
