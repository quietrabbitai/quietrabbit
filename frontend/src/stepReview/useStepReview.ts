// The hook behind StepReviewHost (items.id=884 stage 2): listens for the two events
// that can mean a run is parked on a review, asks the backend what it has, and runs
// the effects stepReview.ts's state machine asks for. All the decisions are in
// stepReview.ts and tested there; this file only wires them to Tauri.
//
// Listeners are detached on unmount (CLAUDE.md: event listeners must be explicitly
// detached on SPA view unmount), with the cancelled flag that covers StrictMode's
// mount-unmount-mount: listen() resolves asynchronously, so a cleanup that runs first
// must detach the listener the moment it arrives.
import { useCallback, useEffect, useRef, useState } from 'react'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import type { ElementDecision } from '../cloudChatAccess/consentDecisions'
import {
  declinePendingReview,
  listPendingReviews,
  pullPendingReview,
  resumeParkedRun,
  sendAnswers,
} from './stepReviewApi'
import {
  INITIAL_STATE,
  reduce,
  shown,
  type Effect,
  type HostEvent,
  type HostState,
  type Shown,
} from './stepReview'

/** Hand-declared like ChatPane's RunStatusPayload: both are emitted by AppHandle::emit
 *  (gate3.rs, lifecycle.rs), so tauri-specta never exports them. Only the fields used
 *  here. */
interface ConsentRequestEvent {
  focus_run_id: string
}
interface RunStatusEvent {
  focus_run_id: string
  status: string
}

export interface StepReviewController {
  shown: Shown | null
  /** The user's answers from the modal. */
  answer: (runId: string, decisions: ElementDecision[]) => void
  decline: (runId: string) => void
  retrySend: (runId: string) => void
  recheck: (runId: string) => void
  dismiss: (runId: string) => void
}

export interface UseStepReviewOptions {
  userId: string
  personaId: string
}

export function useStepReview({ userId, personaId }: UseStepReviewOptions): StepReviewController {
  const [state, setState] = useState<HostState>(INITIAL_STATE)
  const stateRef = useRef<HostState>(INITIAL_STATE)
  const aliveRef = useRef(true)

  // The latest dispatch, reachable from effects that finish later.
  const dispatchRef = useRef<(e: HostEvent) => void>(() => undefined)

  const runEffect = useCallback(
    (effect: Effect) => {
      const dispatch = (e: HostEvent) => dispatchRef.current(e)
      switch (effect.kind) {
        case 'pull':
          void pullPendingReview(personaId, effect.runId).then((r) =>
            dispatch(
              r.status === 'ok'
                ? { type: 'pulled', runId: effect.runId, review: r.review }
                : { type: 'pulled', runId: effect.runId, review: null, failed: true },
            ),
          )
          return
        case 'resume':
          void resumeParkedRun(userId, personaId, effect.runId).then((r) => {
            if (r.status !== 'ok') dispatch({ type: 'resume_failed', runId: effect.runId })
          })
          return
        case 'send':
          void sendAnswers(userId, personaId, effect.runId, effect.decisions).then((r) =>
            dispatch(
              r.status === 'ok'
                ? { type: 'send_done', runId: effect.runId }
                : { type: 'send_failed', runId: effect.runId, message: r.error },
            ),
          )
          return
        case 'decline':
          void declinePendingReview(personaId, effect.runId).then((r) =>
            dispatch(
              r.status === 'ok'
                ? { type: 'decline_done', runId: effect.runId }
                : { type: 'decline_failed', runId: effect.runId, message: r.error },
            ),
          )
          return
      }
    },
    [userId, personaId],
  )

  const dispatch = useCallback(
    (event: HostEvent) => {
      if (!aliveRef.current) return
      const step = reduce(stateRef.current, event)
      if (step.state !== stateRef.current) {
        stateRef.current = step.state
        setState(step.state)
      }
      for (const effect of step.effects) runEffect(effect)
    },
    [runEffect],
  )

  useEffect(() => {
    dispatchRef.current = dispatch
  }, [dispatch])

  useEffect(() => {
    aliveRef.current = true
    let cancelled = false
    const unlisteners: UnlistenFn[] = []
    const attach = (p: Promise<UnlistenFn>) => {
      p.then((fn) => {
        if (cancelled) fn()
        else unlisteners.push(fn)
      }).catch(() => {
        // No Tauri bridge (a plain browser): nothing to listen to.
      })
    }
    attach(
      listen<ConsentRequestEvent>('consent_request', (event) =>
        dispatch({ type: 'consent_request', runId: event.payload.focus_run_id }),
      ),
    )
    attach(
      listen<RunStatusEvent>('run-status-update', (event) =>
        dispatch({
          type: 'run_status',
          runId: event.payload.focus_run_id,
          status: event.payload.status,
        }),
      ),
    )
    return () => {
      cancelled = true
      aliveRef.current = false
      for (const fn of unlisteners) fn()
    }
  }, [dispatch])

  // Once on mount: every review of this Persona whose run is open and waiting
  // (list_pending_step_reviews). The listeners above are attached first, so an event
  // that arrives meanwhile is not lost, and each review found goes through the same
  // state machine as a pulled one (a ready one opens the modal, a needs_resume one
  // resumes its run). This is what finds a review after a restart, when no event will
  // ever come for it. A failed read leaves the host as it is: the next event still
  // finds a review.
  useEffect(() => {
    let cancelled = false
    void listPendingReviews(personaId).then((r) => {
      if (cancelled || r.status !== 'ok') return
      for (const review of r.reviews) {
        dispatchRef.current({ type: 'pulled', runId: review.run_id, review })
      }
    })
    return () => {
      cancelled = true
    }
  }, [personaId])

  return {
    shown: shown(state),
    answer: useCallback(
      (runId: string, decisions: ElementDecision[]) => dispatch({ type: 'answer', runId, decisions }),
      [dispatch],
    ),
    decline: useCallback((runId: string) => dispatch({ type: 'decline', runId }), [dispatch]),
    retrySend: useCallback((runId: string) => dispatch({ type: 'retry_send', runId }), [dispatch]),
    recheck: useCallback((runId: string) => dispatch({ type: 'recheck', runId }), [dispatch]),
    dismiss: useCallback((runId: string) => dispatch({ type: 'dismiss', runId }), [dispatch]),
  }
}
