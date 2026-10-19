// Runs a credential change through the finish-or-stop prompt (items.id=778).
//
// The form hands over a function that makes the backend call for a given
// WritersChoice. The first attempt asks (nothing is changed). If work is
// running the hook moves to the prompt; the user's answer repeats the call.
// The function (a closure over what the user typed) is kept in a ref only
// while a prompt or a wait is on screen, and is dropped on cancel, on any
// error and on completion; it is never logged or put in state.

import { useCallback, useEffect, useRef, useState } from 'react'
import { commands, type KeyRotationError, type WritersChoice } from '../bindings'
import {
  IDLE,
  stateAfterChoice,
  stateAfterError,
  type WaitingChoice,
  type WriterState,
} from './rotationWriters'

type CallResult = { status: 'ok' } | { status: 'error'; error: KeyRotationError }

export type WritersCall = (writers: WritersChoice) => Promise<CallResult>

export interface WritersFlowOutcome {
  /** ok, or the coded failure. A cancelled wait is reported as `null`. */
  result: CallResult | null
}

export interface WritersFlow {
  state: WriterState
  /** A backend call is in flight (the first check, or a wait). */
  inFlight: boolean
  /** `call` makes the rotation call for a choice; `release` undoes whatever
   *  the form registered for the start (the rotation task) and is called once,
   *  when the flow ends. */
  begin: (call: WritersCall, release: () => void) => void
  choose: (choice: WaitingChoice) => void
  /** Cancel: during a wait it asks the backend to stop waiting; on a prompt it
   *  just drops the held values. Nothing is changed either way. */
  cancel: () => void
  /** Dismiss the 'did not stop' screen. */
  dismiss: () => void
}

export function useWritersFlow(onEnd: (outcome: WritersFlowOutcome) => void): WritersFlow {
  const [state, setState] = useState<WriterState>(IDLE)
  const [inFlight, setInFlight] = useState(false)
  const callRef = useRef<WritersCall | null>(null)
  const releaseRef = useRef<(() => void) | null>(null)
  const stateRef = useRef<WriterState>(IDLE)
  const onEndRef = useRef(onEnd)
  useEffect(() => {
    onEndRef.current = onEnd
  })

  const show = useCallback((next: WriterState) => {
    stateRef.current = next
    setState(next)
  }, [])

  const finish = useCallback(
    (result: CallResult | null) => {
      callRef.current = null
      const release = releaseRef.current
      releaseRef.current = null
      release?.()
      show(IDLE)
      setInFlight(false)
      onEndRef.current({ result })
    },
    [show],
  )

  const attempt = useCallback(
    async (choice: WritersChoice) => {
      const call = callRef.current
      if (call === null) return
      setInFlight(true)
      let result: CallResult
      try {
        result = await call(choice)
      } catch {
        // The IPC call itself threw (the bridge, not a coded error).
        result = { status: 'error', error: { code: 'internal' } }
      }
      setInFlight(false)
      if (result.status === 'ok') {
        finish(result)
        return
      }
      const prompt = stateAfterError(result.error)
      if (prompt !== null) {
        show(prompt)
        return
      }
      finish(result.error.code === 'drain_cancelled' ? null : result)
    },
    [finish, show],
  )

  const begin = useCallback(
    (call: WritersCall, release: () => void) => {
      callRef.current = call
      releaseRef.current = release
      void attempt('ask')
    },
    [attempt],
  )

  const choose = useCallback(
    (choice: WaitingChoice) => {
      show(stateAfterChoice(stateRef.current, choice))
      void attempt(choice)
    },
    [attempt, show],
  )

  const cancel = useCallback(() => {
    if (stateRef.current.kind === 'waiting') {
      // The waiting call answers drain_cancelled, which ends the flow.
      void commands.cancelWriterWait()
      return
    }
    finish(null)
  }, [finish])

  const dismiss = useCallback(() => finish(null), [finish])

  return { state, inFlight, begin, choose, cancel, dismiss }
}
