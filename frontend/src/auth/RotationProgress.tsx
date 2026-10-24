// Key-rotation progress overlay (items.id=769 stage 1). Mounted ONCE at the App
// root, outside the login gate: login() resumes an interrupted rotation and
// restore_with_recovery_phrase runs one with no session, and both report on the
// same "task-progress" channel as a rotation started from inside the app.
//
// All decisions (what is trusted, when an outcome may be announced) are in
// rotationProgress.ts. This file only wires: one event listener, a status read
// on mount and about once a second while running, the blocking-layer
// registration, and the markup. The backend `message` field is never rendered.

import './RotationProgress.css'
import { useEffect, useReducer, useRef, useSyncExternalStore } from 'react'
import { useTranslation } from 'react-i18next'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { commands } from '../bindings'
import { acquireBlockingOverlay } from './blockingOverlay'
import { getRotationTask, subscribeRotationTask } from './rotationTask'
import {
  INITIAL_OVERLAY_STATE,
  OWNER_RELEASE_GRACE_MS,
  STATUS_POLL_INTERVAL_MS,
  TASK_PROGRESS_EVENT,
  isKeyRotationProgress,
  outcomeSuffix,
  isWaitingOnLargeStep,
  overlayVisible,
  phaseLabelSuffix,
  reduceOverlay,
  titleSuffix,
} from './rotationProgress'

export interface RotationProgressProps {
  /** The person acknowledged a rotation that did not finish and that this
   *  webview did not start (typically after a reload). The session may now
   *  hold a key that no longer matches the account's, so the app signs out. */
  onFailedAcknowledged?: () => void
}

export function RotationProgress({ onFailedAcknowledged }: RotationProgressProps) {
  const { t } = useTranslation()
  const [state, dispatch] = useReducer(reduceOverlay, INITIAL_OVERLAY_STATE)
  const registeredKind = useSyncExternalStore(subscribeRotationTask, getRotationTask)
  const dialogRef = useRef<HTMLDivElement>(null)
  const visible = overlayVisible(state)

  // One listener for the life of the overlay. The cancelled flag covers React
  // StrictMode's mount-unmount-mount: listen() resolves asynchronously, so a
  // cleanup that runs first must detach the listener the moment it arrives.
  useEffect(() => {
    let cancelled = false
    let unlisten: UnlistenFn | undefined
    listen<unknown>(TASK_PROGRESS_EVENT, (event) => {
      if (isKeyRotationProgress(event.payload)) {
        dispatch({ type: 'event', payload: event.payload })
      }
    })
      .then((fn) => {
        if (cancelled) fn()
        else unlisten = fn
      })
      .catch(() => {
        // No Tauri bridge (plain browser): the status poll below still works
        // where commands do, and there is nothing else to do.
      })
    return () => {
      cancelled = true
      unlisten?.()
    }
  }, [])

  // Status on mount (a reload mid-rotation finds it running), then about once a
  // second while running. last_outcome is read only through the reducer, which
  // ignores it unless this webview watched the rotation run.
  useEffect(() => {
    let cancelled = false
    const refresh = () => {
      void commands.getRotationStatus().then((result) => {
        if (!cancelled && result.status === 'ok') {
          dispatch({ type: 'status', status: result.data })
        }
      })
    }
    refresh()
    if (!state.running) {
      return () => {
        cancelled = true
      }
    }
    const id = window.setInterval(refresh, STATUS_POLL_INTERVAL_MS)
    return () => {
      cancelled = true
      window.clearInterval(id)
    }
  }, [state.running])

  // The form that started the rotation owns announcing its outcome. It keeps
  // that ownership for a short grace after letting go (see
  // OWNER_RELEASE_GRACE_MS).
  useEffect(() => {
    if (registeredKind !== null) {
      dispatch({ type: 'owner', kind: registeredKind })
      return
    }
    const id = window.setTimeout(() => dispatch({ type: 'owner', kind: null }), OWNER_RELEASE_GRACE_MS)
    return () => window.clearTimeout(id)
  }, [registeredKind])

  // Native Cloud Chat panes draw above the webview: hide them while this shows.
  useEffect(() => {
    if (!visible) return
    return acquireBlockingOverlay()
  }, [visible])

  // Keyboard focus stays inside the dialog: the page underneath is covered but
  // is still reachable by Tab otherwise. While progress runs the panel itself
  // holds focus (nothing in it is interactive); once an outcome shows, focus
  // moves to Close, Tab and Shift+Tab cycle among the dialog's buttons, and
  // Escape dismisses.
  const hasOutcome = state.outcome !== null
  useEffect(() => {
    if (!visible) return
    const dialog = dialogRef.current
    if (!dialog) return
    const buttons = () => Array.from(dialog.querySelectorAll<HTMLElement>('button'))
    if (hasOutcome) buttons()[0]?.focus()
    else dialog.focus()
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape' && hasOutcome) {
        event.preventDefault()
        dialog.querySelector<HTMLElement>('button')?.click()
        return
      }
      if (event.key !== 'Tab') return
      event.preventDefault()
      const items = buttons()
      if (items.length === 0) {
        dialog.focus()
        return
      }
      const index = items.indexOf(document.activeElement as HTMLElement)
      const step = event.shiftKey ? -1 : 1
      items[(index + step + items.length) % items.length].focus()
    }
    document.addEventListener('keydown', onKeyDown)
    return () => document.removeEventListener('keydown', onKeyDown)
  }, [visible, hasOutcome])

  if (!visible) return null

  // Member access (labels.x) keeps the t() keys below statically checkable by
  // styleAndI18nConsistency.test.ts; a call or bare const from another file is not.
  const labels = {
    title: titleSuffix(state.ownerKind),
    phase: phaseLabelSuffix(state.phase),
    outcome: outcomeSuffix(state.outcome),
  }

  return (
    <div className="rotation-overlay">
      <div
        ref={dialogRef}
        className="rotation-overlay__panel"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="rotation-overlay-title"
        tabIndex={-1}
      >
        <h2 id="rotation-overlay-title" className="rotation-overlay__title">
          {t(`auth.rotation.title.${labels.title}`)}
        </h2>

        {state.outcome === null ? (
          <>
            <p className="rotation-overlay__phase">{t(`auth.rotation.phase.${labels.phase}`)}</p>
            <progress
              className="rotation-overlay__bar"
              aria-label={t('auth.rotation.progressLabel')}
              max={100}
              value={state.percent ?? undefined}
            />
            {/* Percent and hint share one grid cell, both always rendered: the
                cell is as tall as the taller of the two, so neither appearing
                nor going away moves the panel. The inactive one is
                visibility:hidden (also out of the accessibility tree). */}
            <div className="rotation-overlay__detail">
              <p className="rotation-overlay__percent" data-active={state.percent !== null}>
                {t('auth.rotation.percent', { percent: state.percent ?? 0 })}
              </p>
              <p className="rotation-overlay__note" data-active={isWaitingOnLargeStep(state)}>
                {t('auth.rotation.largeStepHint')}
              </p>
            </div>
            <p className="rotation-overlay__note">{t('auth.rotation.keepOpen')}</p>
            <p className="rotation-overlay__note">{t('auth.rotation.crashSafe')}</p>
          </>
        ) : (
          <>
            <p className="rotation-overlay__outcome">
              {t(`auth.rotation.outcome.${labels.outcome}`)}
            </p>
            <button
              type="button"
              className="rotation-overlay__close"
              onClick={() => {
                dispatch({ type: 'dismiss' })
                if (state.outcome === 'failed') onFailedAcknowledged?.()
              }}
            >
              {t('auth.rotation.close')}
            </button>
          </>
        )}
      </div>
    </div>
  )
}
