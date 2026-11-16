// The pure logic of the outbound review of an external step's prompt, as the
// front end sees it (items.id=884 stage 2, decisions.id=943 point 6). No React, no
// Tauri: a plain node test covers it (stepReview.test.ts). The hook
// (useStepReview.ts) feeds it events and runs the effects it asks for.
//
// WHAT THE BACKEND DECIDES, AND WHAT THIS DOES NOT. The server renders the prompt,
// applies the user's recorded answers to it and sends it (conductor/step_review.rs).
// This module never sees or builds the text that is sent. It only (1) finds out that
// a run is parked on a review, (2) shows the flagged spans in the Privacy Guardian
// modal, (3) records the answers through submit_element_consent_decision and
// resumes the run, (4) refuses through decline_step_review.
//
// HOW A REVIEW IS FOUND. The consent_request event is a hint, never the source:
// gate3 emits it BEFORE the review record is written, and the same event also
// carries a copy review's and an API Chat send's request (told apart by the run id
// prefix) and a Gate 3 promotion's. So an event only makes the host ask the backend
// (get_pending_step_review), and the backend's answer is the only thing that opens
// the modal. The same pull runs when a run's status becomes awaiting_user, which is
// the moment after the record exists (run-status-update carries no park reason).
//
// THREE RULES THE STATE MACHINE KEEPS:
//   * needs_resume (the app restarted: the backend no longer holds the flagged
//     text) is answered by calling resume_run, which asks the review again. At most
//     MAX_AUTO_RESUMES times in a row without reaching a review the user can see;
//     after that the host stops and says so, so a fault cannot loop.
//   * A modal whose run no longer has a pending review is dismissed: the run parked
//     again for another reason (the review blocked, the spend gate stopped it), or it
//     ended (failed, cancelled, complete). The status event carries no reason, so
//     "the backend has no pending review for this run" is the signal.
//   * Answers that could not be recorded keep the review open (error phase) with the
//     user's choices, so they can try again or refuse; nothing is sent meanwhile.
import type { PendingStepReview, PendingStepReviewSpan } from '../bindings.ts'
import type { ElementDecision } from '../cloudChatAccess/consentDecisions.ts'
import { COPY_REVIEW_RUN_ID_PREFIX } from '../chat/copyDecisions.ts'

/** output_store::API_SEND_REVIEW_RUN_ID_PREFIX -- keep in sync by hand. */
export const API_SEND_REVIEW_RUN_ID_PREFIX = 'api-chat-send-'

/** The synthetic keys of the two other reviews whose consent_request this host
 *  must leave alone. */
export const OTHER_REVIEW_RUN_ID_PREFIXES: readonly string[] = [
  COPY_REVIEW_RUN_ID_PREFIX,
  API_SEND_REVIEW_RUN_ID_PREFIX,
]

/** A run id that can belong to an external step's review: not a copy review's or an
 *  API Chat send's synthetic key. */
export function isStepReviewRunId(runId: string): boolean {
  return runId.length > 0 && !OTHER_REVIEW_RUN_ID_PREFIXES.some((p) => runId.startsWith(p))
}

/** Resumes in a row (without reaching a review that can be shown) before the host
 *  stops asking the backend to ask again. */
export const MAX_AUTO_RESUMES = 2

export type RunPhase =
  /** The backend has a review for this run and holds its text: the modal is open. */
  | { phase: 'review'; review: PendingStepReview }
  /** The answers are being recorded and the run resumed. */
  | { phase: 'sending'; review: PendingStepReview; decisions: ElementDecision[] }
  /** The refusal is being recorded. */
  | { phase: 'declining'; review: PendingStepReview }
  /** needs_resume: resume_run was called so the review is asked again. */
  | { phase: 'resuming'; reviewId: string }
  /** needs_resume persisted after MAX_AUTO_RESUMES. */
  | { phase: 'stuck'; reviewId: string }
  /** An action failed. `decisions` is null for a failed refusal. */
  | {
      phase: 'error'
      review: PendingStepReview
      decisions: ElementDecision[] | null
      message: string
    }

export interface HostState {
  runs: Record<string, RunPhase>
  /** Auto-resumes in a row, per run. Reset when a review can be shown or the run ends. */
  resumes: Record<string, number>
}

export const INITIAL_STATE: HostState = { runs: {}, resumes: {} }

export type HostEvent =
  | { type: 'consent_request'; runId: string }
  | { type: 'run_status'; runId: string; status: string }
  /** Answer of get_pending_step_review. `null` = no pending review (or the call
   *  failed: the modal is left as it is only when `failed`). */
  | { type: 'pulled'; runId: string; review: PendingStepReview | null; failed?: boolean }
  | { type: 'resume_failed'; runId: string }
  | { type: 'answer'; runId: string; decisions: ElementDecision[] }
  | { type: 'send_done'; runId: string }
  | { type: 'send_failed'; runId: string; message: string }
  | { type: 'decline'; runId: string }
  | { type: 'decline_done'; runId: string }
  | { type: 'decline_failed'; runId: string; message: string }
  /** The user closes the error or stuck notice without acting (the run stays parked). */
  | { type: 'dismiss'; runId: string }
  /** The user presses Check again on the stuck notice. */
  | { type: 'recheck'; runId: string }
  /** The user presses Try again on a failed send. */
  | { type: 'retry_send'; runId: string }

export type Effect =
  | { kind: 'pull'; runId: string }
  | { kind: 'resume'; runId: string }
  | { kind: 'send'; runId: string; decisions: ElementDecision[] }
  | { kind: 'decline'; runId: string }

export interface Step {
  state: HostState
  effects: Effect[]
}

const TERMINAL_STATUSES: readonly string[] = ['failed', 'cancelled', 'complete']

function without<T>(rec: Record<string, T>, key: string): Record<string, T> {
  const { [key]: _gone, ...rest } = rec
  return rest
}

function withRun(state: HostState, runId: string, phase: RunPhase): HostState {
  return { ...state, runs: { ...state.runs, [runId]: phase } }
}

function forget(state: HostState, runId: string): HostState {
  return { runs: without(state.runs, runId), resumes: without(state.resumes, runId) }
}

const none = (state: HostState): Step => ({ state, effects: [] })

export function reduce(state: HostState, event: HostEvent): Step {
  const current = state.runs[event.runId]

  switch (event.type) {
    case 'consent_request': {
      if (!isStepReviewRunId(event.runId)) return none(state)
      return { state, effects: [{ kind: 'pull', runId: event.runId }] }
    }

    case 'run_status': {
      if (!isStepReviewRunId(event.runId)) return none(state)
      if (TERMINAL_STATUSES.includes(event.status)) {
        return none(forget(state, event.runId))
      }
      if (event.status === 'awaiting_user') {
        return { state, effects: [{ kind: 'pull', runId: event.runId }] }
      }
      return none(state)
    }

    case 'pulled': {
      if (!isStepReviewRunId(event.runId)) return none(state)
      if (event.failed) return none(state) // the modal, if any, is left as it is
      const review = event.review
      if (review === null) {
        // Nothing pending: a modal for this run is stale (the run parked again for
        // another reason, or the review was superseded). While an action of ours
        // is in flight the run is not stale: the answer is on its way.
        if (current && (current.phase === 'sending' || current.phase === 'declining')) {
          return none(state)
        }
        return none(forget(state, event.runId))
      }
      if (review.state === 'needs_resume') {
        const done = state.resumes[event.runId] ?? 0
        if (current?.phase === 'sending' || current?.phase === 'declining') return none(state)
        if (done >= MAX_AUTO_RESUMES) {
          return none(withRun(state, event.runId, { phase: 'stuck', reviewId: review.review_id }))
        }
        return {
          state: {
            runs: { ...state.runs, [event.runId]: { phase: 'resuming', reviewId: review.review_id } },
            resumes: { ...state.resumes, [event.runId]: done + 1 },
          },
          effects: [{ kind: 'resume', runId: event.runId }],
        }
      }
      // ready: the same review keeps whatever phase it is in (the modal keeps the
      // user's edits), a different or new one opens.
      if (
        current &&
        (current.phase === 'review' ||
          current.phase === 'sending' ||
          current.phase === 'declining' ||
          current.phase === 'error') &&
        current.review.review_id === review.review_id
      ) {
        return none(state)
      }
      return none({
        runs: { ...state.runs, [event.runId]: { phase: 'review', review } },
        resumes: without(state.resumes, event.runId),
      })
    }

    case 'resume_failed': {
      // The run cannot be resumed (it finished, or is not parked any more): there is
      // nothing to review for it from here.
      return none(forget(state, event.runId))
    }

    case 'answer': {
      if (current?.phase !== 'review') return none(state)
      return {
        state: withRun(state, event.runId, {
          phase: 'sending',
          review: current.review,
          decisions: event.decisions,
        }),
        effects: [{ kind: 'send', runId: event.runId, decisions: event.decisions }],
      }
    }

    case 'retry_send': {
      if (current?.phase !== 'error' || current.decisions === null) return none(state)
      return {
        state: withRun(state, event.runId, {
          phase: 'sending',
          review: current.review,
          decisions: current.decisions,
        }),
        effects: [{ kind: 'send', runId: event.runId, decisions: current.decisions }],
      }
    }

    case 'send_done': {
      // The answers are recorded and the run resumed. The run now re-renders the
      // prompt and applies them: its next status says what happened, and a new
      // pending review (if any) is pulled when it parks again.
      if (current?.phase !== 'sending') return none(state)
      return none(forget(state, event.runId))
    }

    case 'send_failed': {
      if (current?.phase !== 'sending') return none(state)
      return none(
        withRun(state, event.runId, {
          phase: 'error',
          review: current.review,
          decisions: current.decisions,
          message: event.message,
        }),
      )
    }

    case 'decline': {
      if (current?.phase !== 'review' && current?.phase !== 'error') return none(state)
      return {
        state: withRun(state, event.runId, { phase: 'declining', review: current.review }),
        effects: [{ kind: 'decline', runId: event.runId }],
      }
    }

    case 'decline_done': {
      return none(forget(state, event.runId))
    }

    case 'decline_failed': {
      if (current?.phase !== 'declining') return none(state)
      return none(
        withRun(state, event.runId, {
          phase: 'error',
          review: current.review,
          decisions: null,
          message: event.message,
        }),
      )
    }

    case 'dismiss': {
      if (current?.phase !== 'error' && current?.phase !== 'stuck') return none(state)
      return none(forget(state, event.runId))
    }

    case 'recheck': {
      if (current?.phase !== 'stuck') return none(state)
      return {
        state: {
          runs: without(state.runs, event.runId),
          resumes: without(state.resumes, event.runId),
        },
        effects: [{ kind: 'pull', runId: event.runId }],
      }
    }
  }
}

/** What the host shows: at most one thing at a time, the earliest run first. */
export type Shown =
  | { kind: 'modal'; runId: string; review: PendingStepReview; sending: boolean }
  | { kind: 'error'; runId: string; message: string; canRetry: boolean }
  | { kind: 'stuck'; runId: string }

export function shown(state: HostState): Shown | null {
  for (const [runId, p] of Object.entries(state.runs)) {
    if (p.phase === 'review') return { kind: 'modal', runId, review: p.review, sending: false }
    if (p.phase === 'sending' || p.phase === 'declining') {
      return { kind: 'modal', runId, review: p.review, sending: true }
    }
    if (p.phase === 'error') {
      return { kind: 'error', runId, message: p.message, canRetry: p.decisions !== null }
    }
    if (p.phase === 'stuck') return { kind: 'stuck', runId }
  }
  return null
}

/** The consent_request payload shape PrivacyGuardianModal takes, built from the
 *  backend's pending review. `focusName` is the label the caller shows (a pending
 *  review does not carry the Focus's display name). */
export interface ModalPayload {
  focus_run_id: string
  focus_name: string
  review_tier: 'low' | 'medium' | 'high'
  spans: ModalSpan[]
}

export interface ModalSpan {
  span_id: string
  category: string
  user_label: string
  original_text: string
  suggestion: string | null
  start_byte: number
  end_byte: number
  score: number
  review_tier: 'low' | 'medium' | 'high'
  fact_key: string | null
}

function modalSpan(s: PendingStepReviewSpan): ModalSpan {
  return {
    span_id: s.span_id,
    category: s.category,
    user_label: s.user_label,
    original_text: s.original_text,
    suggestion: s.suggestion,
    start_byte: s.start_byte,
    end_byte: s.end_byte,
    score: s.score ?? 0,
    review_tier: s.review_tier,
    fact_key: s.fact_key,
  }
}

const TIER_RANK = { low: 0, medium: 1, high: 2 } as const

export function modalPayload(review: PendingStepReview, focusName: string): ModalPayload {
  const spans = review.spans.map(modalSpan)
  // Only used by the modal for an empty list (the worst tier of the spans otherwise).
  const worst = spans.reduce<'low' | 'medium' | 'high'>(
    (acc, s) => (TIER_RANK[s.review_tier] > TIER_RANK[acc] ? s.review_tier : acc),
    'low',
  )
  return {
    focus_run_id: review.run_id,
    focus_name: focusName,
    review_tier: spans.length === 0 ? 'high' : worst,
    spans,
  }
}

/** The command's own error codes (commands/step_review.rs ERROR_CODES), as the keys
 *  the host shows. Anything else is the generic one. */
export type ErrorKey = 'persona_not_found' | 'invalid_run_id' | 'step_review_unavailable'

export function errorKeyOf(code: string): ErrorKey {
  if (code === 'persona_not_found' || code === 'invalid_run_id') return code
  return 'step_review_unavailable'
}
