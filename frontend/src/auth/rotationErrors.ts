// Maps KeyRotationError (src-tauri/src/commands/key_rotation.rs) to user-facing
// copy, per screen (items.id=769 stage 1).
//
// This file returns i18n KEY SUFFIXES under `auth.rotationErrors.` plus params;
// RotationErrorText.tsx does the t() call. Two reasons for that split:
//   - styleAndI18nConsistency.test.ts resolves t() arguments statically, so the
//     call site must be a template literal with an opaque `${message.suffix}`
//     hole; a bare `t(variable)` fails that test.
//   - the suffixes are flat (one segment) for the same reason: a wildcard hole
//     is one dotted segment.
//
// EXHAUSTIVE ON PURPOSE: BASE is a Record over every KeyRotationError code, so
// adding a code to the Rust enum (and regenerating bindings.ts) makes tsc fail
// here until the code has a message. Per-screen differences are overrides.
// Codes carry no path, id or secret; nothing here ever receives a raw
// backend message.

import type { KeyRotationError } from '../bindings.ts'

export type RotationErrorCode = KeyRotationError['code']

/** Where the error is being shown. */
export type RotationErrorContext =
  | 'changePassword'
  | 'newPhrase'
  | 'restore'
  | 'login'
  | 'cancel'

export interface RotationErrorMessage {
  /** Leaf name under `auth.rotationErrors.` in en.json. */
  suffix: string
  /** Interpolation params for that string (counts and sizes only). */
  params?: Record<string, number>
}

const BASE: Record<RotationErrorCode, string> = {
  not_logged_in: 'notLoggedIn',
  invalid_credentials: 'invalidCredentials',
  account_locked: 'accountLocked',
  new_password_empty: 'newPasswordEmpty',
  phrase_invalid: 'phraseInvalid',
  rotation_in_progress: 'rotationInProgress',
  active_runs: 'activeRuns',
  busy: 'busy',
  insufficient_space: 'insufficientSpace',
  free_space_unknown: 'freeSpaceUnknown',
  unrecognised_files: 'unrecognisedFiles',
  database_unreadable: 'databaseUnreadable',
  epoch_changed: 'epochChanged',
  rotation_already_complete: 'rotationAlreadyComplete',
  rolled_back: 'rolledBack',
  needs_resume: 'needsResume',
  no_pending_phrase: 'noPendingPhrase',
  confirmation_failed: 'confirmationFailed',
  internal: 'internal',
}

const OVERRIDES: Record<RotationErrorContext, Partial<Record<RotationErrorCode, string>>> = {
  // The password typed is the account's current password.
  changePassword: { invalid_credentials: 'currentPasswordWrong' },
  newPhrase: {
    invalid_credentials: 'currentPasswordWrong',
    rolled_back: 'rolledBackNewPhrase',
    needs_resume: 'needsResumeNewPhrase',
  },
  restore: {
    invalid_credentials: 'restoreInvalidCredentials',
    rolled_back: 'rolledBackRestore',
    needs_resume: 'needsResumeRestore',
  },
  // Login reports plain strings, not KeyRotationError (see classifyLoginError,
  // added with the login screen); nothing differs from the base wording.
  login: {},
  cancel: { invalid_credentials: 'cancelInvalidCredentials' },
}

/** The i18n suffix and params for an error on a given screen. */
export function rotationErrorMessage(
  error: KeyRotationError,
  context: RotationErrorContext,
): RotationErrorMessage {
  const suffix = OVERRIDES[context][error.code] ?? BASE[error.code]
  switch (error.code) {
    case 'active_runs':
      return { suffix, params: { count: error.count } }
    case 'insufficient_space':
      return {
        suffix,
        params: { needed: error.needed_mb, available: error.available_mb },
      }
    default:
      return { suffix }
  }
}

/** Codes after which the in-memory session may no longer match the account's
 *  key (or there is no session), so the screen should sign the user out
 *  rather than leave them in an app whose stores will fail. needs_resume: the
 *  change is unfinished and resumes at the next sign-in. epoch_changed: another
 *  session rotated the key. */
export function requiresSignOut(code: RotationErrorCode): boolean {
  return code === 'needs_resume' || code === 'epoch_changed' || code === 'not_logged_in'
}

/** Every suffix this file can return, for the test that checks en.json has each. */
export function allRotationErrorSuffixes(): string[] {
  const out = new Set<string>(Object.values(BASE))
  for (const overrides of Object.values(OVERRIDES)) {
    for (const suffix of Object.values(overrides)) {
      if (suffix) out.add(suffix)
    }
  }
  return [...out].sort()
}
