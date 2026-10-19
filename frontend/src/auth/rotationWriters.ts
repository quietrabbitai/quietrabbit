// The finish-or-stop prompt of a credential change (items.id=778,
// decisions.id=896), as plain functions so it runs under
// `node --experimental-strip-types` (rotationWriters.test.ts).
//
// Before a data-key rotation starts, the backend drains the account's writers
// (a chat reply, a Focus run). The three commands (changePassword,
// restoreWithRecoveryPhrase, confirmNewRecoveryPhrase) take a WritersChoice:
//   ask          first call; nothing is changed, and if work is running the
//                call answers `writers_active` with the counts;
//   finish_first wait for it, then rotate (the default choice);
//   stop_and_save stop it cooperatively, keep what it wrote, then rotate.
// A wait ends with `writers_still_running` after the ceiling (10 minutes), or
// `writer_not_stopping` 60 s after a stop that did not take. The user can
// cancel a wait at any time (cancelWriterWait), which answers `drain_cancelled`.
//
// The state below is what the prompt shows. While it is anything but idle the
// form still holds what the user typed (in a closure, never in a field), so
// the answer can be sent without asking for the password again; it is dropped
// on cancel, on any error and on completion (useWritersFlow).

import type { KeyRotationError } from '../bindings.ts'

export type WaitingChoice = 'finish_first' | 'stop_and_save'

export type WriterState =
  | { kind: 'idle' }
  /** Work is running; ask the user. */
  | { kind: 'ask'; replies: number; focusRuns: number }
  /** A call is waiting for the work to finish or stop. */
  | { kind: 'waiting'; choice: WaitingChoice; replies: number; focusRuns: number }
  /** The 10-minute wait ended. */
  | { kind: 'ceiling'; replies: number; focusRuns: number }
  /** 'Stop and save' did not take within 60 s. */
  | { kind: 'not_stopping' }

export const IDLE: WriterState = { kind: 'idle' }

/** The prompt state an error leads to, or null when the error is an ordinary
 *  failure (shown by the form) or a cancelled wait (silent). */
export function stateAfterError(error: KeyRotationError): WriterState | null {
  switch (error.code) {
    case 'writers_active':
      return { kind: 'ask', replies: error.replies, focusRuns: error.focus_runs }
    case 'writers_still_running':
      return { kind: 'ceiling', replies: error.replies, focusRuns: error.focus_runs }
    case 'writer_not_stopping':
      return { kind: 'not_stopping' }
    default:
      return null
  }
}

/** The user answered the prompt: the waiting screen for that answer, keeping
 *  the counts that were on screen. */
export function stateAfterChoice(state: WriterState, choice: WaitingChoice): WriterState {
  if (state.kind === 'ask' || state.kind === 'ceiling' || state.kind === 'waiting') {
    return { kind: 'waiting', choice, replies: state.replies, focusRuns: state.focusRuns }
  }
  return { kind: 'waiting', choice, replies: 0, focusRuns: 0 }
}

/** What is blocking, for the wording. */
export type Blocker = 'replies' | 'tasks' | 'both' | 'none'

export function blockerOf(replies: number, focusRuns: number): Blocker {
  if (replies > 0 && focusRuns > 0) return 'both'
  if (replies > 0) return 'replies'
  if (focusRuns > 0) return 'tasks'
  return 'none'
}

/** Whether 'Stop and save' means anything: only visible work can be stopped. */
export function canStop(replies: number, focusRuns: number): boolean {
  return replies + focusRuns > 0
}

/** Whether the screen is showing a prompt or a wait (the form is hidden and
 *  its submitted values are still held). */
export function isActive(state: WriterState): boolean {
  return state.kind !== 'idle'
}

/** Every i18n leaf under `auth.rotationWriters.` the prompt can use, for the
 *  test that checks en.json has each. */
export function allWriterTextKeys(): string[] {
  return [
    'ask.title.replies',
    'ask.title.tasks',
    'ask.title.both',
    'ask.body',
    'ask.finishFirst',
    'ask.finishFirstHint',
    'ask.stopAndSave',
    'ask.stopAndSaveHint',
    'ask.cancel',
    'waiting.finish',
    'waiting.stop',
    'waiting.paused',
    'waiting.blocker.replies',
    'waiting.blocker.tasks',
    'waiting.blocker.both',
    'waiting.cancel',
    'ceiling.title',
    'ceiling.bodyWork',
    'ceiling.bodyOther',
    'ceiling.keepWaiting',
    'ceiling.stopAndSave',
    'ceiling.cancel',
    'notStopping.title',
    'notStopping.body',
    'notStopping.ok',
  ].sort()
}
