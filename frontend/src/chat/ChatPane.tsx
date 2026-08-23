// Real chat/transcript component -- the thing behind MiddleZone's chatPane
// prop, replacing the placeholder <p> stubs previously in NavShell.tsx's
// personaHub branch and Tier3AccessPane.tsx's conversation pane. One
// component for both: gate3Track is the only behavioral difference (whether
// the assistant reply gets gate3_review_status="drafted").

import { useCallback, useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { commands, type MessageInfo } from '../bindings'
import './ChatPane.css'

export interface ChatPaneProps {
  /** Matches MiddleZone's own contextKey prop verbatim -- the caller-owned
   *  transcript identity MiddleZone's doc comment says it does not own. */
  contextKey: string
  userId: string
  personaId: string
  focusId: string
  gate3Track: boolean
  /** Lets the caller compute MiddleZone's isGenerating prop for real,
   *  replacing today's hardcoded false at both call sites. */
  onGenerating?: (isGenerating: boolean) => void
  /** Fires once, per run, the moment a gate3Track=true assistant reply has
   *  been backfilled with real content and is still gate3_review_status
   *  'drafted' -- the signal that it's ready for Privacy Guardian review.
   *  Only ever fires when gate3Track is true; ignored (never called) for
   *  Persona-hub usage. The caller (Tier3AccessPane) is responsible for
   *  invoking commands.requestTier3Gate3Review with the given messageId. */
  onDraftReady?: (messageId: string) => void
}

/** Hand-declared, not generated: RunStatusPayload (conductor/lifecycle.rs)
 *  is emitted via AppHandle::emit(), not returned/accepted by any
 *  #[tauri::command] -- tauri-specta's collect_commands! only walks types
 *  reachable from the registered command surface, so specta::Type on the
 *  Rust struct alone doesn't get it into bindings.ts. This codebase has no
 *  typed-event registration (no collect_events!/mount_events anywhere) to
 *  fix that with; adopting tauri-specta's separate Event system for one
 *  payload was judged out of proportion for this item. Keep this in sync
 *  by hand with RunStatusPayload's field list if that struct changes. */
interface RunStatusPayload {
  focus_run_id: string
  status: string
  current_step: number
  total_steps: number
  step_display_name: string | null
  step_content: string | null
  /** R1 crisis-handling floor (items.id=297): mirrors RunResult's field of
   *  the same name (conductor/lifecycle.rs). Some(...) only on the same
   *  "failed"/"awaiting_user" emit that accompanies a crisis-flagged run's
   *  Tier 3 pause or step failure -- mutually exclusive with step_content in
   *  practice. */
  crisis_resource_block: string | null
}

/** Hand-declared, same convention/rationale as RunStatusPayload above --
 *  matches MessageContentReadyPayload (commands/messages.rs). items.id=320:
 *  the only reliable "safe to re-fetch now" signal. Every run-status-update
 *  status (including "awaiting_feedback") is emitted from inside
 *  execute_full_inner()/cleanup(), which completes and returns well before
 *  send_message's background backfill task even starts -- so no status on
 *  that event can be trusted to mean "list_messages will now show real
 *  content." This event is emitted unconditionally, once, only after that
 *  backfill attempt (success, crisis block, Tier 3/gate3 draft, or genuinely
 *  nothing to backfill) has finished. */
interface MessageContentReadyPayload {
  focus_run_id: string
  message_id: string
}

/** items.id=320 fallback safety net: covers a missed/lost
 *  "message-content-ready" event (app restart mid-run, IPC hiccup, the
 *  backend's own bounded DB-write timeout) that the event listener alone
 *  wouldn't recover from. Poll interval and overall cap are starting points
 *  -- comfortably clear of the ~2s extraction-pass gap observed in repro,
 *  with margin for slow-system contention. */
const CONTENT_POLL_INTERVAL_MS = 2000
const CONTENT_POLL_TIMEOUT_MS = 25000

export function ChatPane({
  contextKey,
  userId,
  personaId,
  focusId,
  gate3Track,
  onGenerating,
  onDraftReady,
}: ChatPaneProps) {
  const { t } = useTranslation()
  const [messages, setMessages] = useState<MessageInfo[]>([])
  const [loadError, setLoadError] = useState<string | null>(null)
  const [draft, setDraft] = useState('')
  const [sendError, setSendError] = useState<string | null>(null)

  const [activeRunId, setActiveRunId] = useState<string | null>(null)
  const [liveStepDisplayName, setLiveStepDisplayName] = useState<
    string | null
  >(null)
  const [liveContent, setLiveContent] = useState('')
  const [elapsedSeconds, setElapsedSeconds] = useState(0)
  /** items.id=320: set only if the CONTENT_POLL_TIMEOUT_MS fallback expires
   *  with the placeholder row still empty -- surfaces a visible notice
   *  instead of silently leaving a blank bubble forever. */
  const [contentTimedOut, setContentTimedOut] = useState(false)
  const elapsedIntervalRef = useRef<number | null>(null)

  const isGenerating = activeRunId !== null

  useEffect(() => {
    onGenerating?.(isGenerating)
  }, [isGenerating, onGenerating])

  // Re-fetch on mount / contextKey change -- MiddleZone won't remount this
  // component for a contextKey change on its own (see this component's own
  // header comment), so re-fetching on identity change is this component's
  // job, not MiddleZone's.
  useEffect(() => {
    setSendError(null)
    setActiveRunId(null)
    setLiveContent('')
    setLiveStepDisplayName(null)
    setContentTimedOut(false)

    setLoadError(null)
    commands.listMessages(userId, personaId, contextKey).then(
      (result) => {
        if (result.status === 'ok') {
          setMessages(result.data)
        } else {
          setLoadError(result.error)
        }
      },
    )
  }, [contextKey, userId, personaId])

  // First listen() call in this frontend (see this file's header comment on
  // RunStatusPayload) -- effect-returns-cleanup-closure shape, matching
  // MiddleZone's debounce-timer cleanup and Tier3AccessPane's ResizeObserver
  // cleanup, per CLAUDE.md's "Tauri event listeners must be explicitly
  // detached on SPA view unmount."
  //
  // items.id=320: run-status-update and message-content-ready are two
  // separate concerns here. run-status-update drives only the live/staged
  // preview (liveContent/liveStepDisplayName) -- none of its statuses,
  // including "awaiting_feedback", are trustworthy signals that
  // list_messages will show real content yet (see this item's plan: every
  // status is emitted from inside execute_full_inner()/cleanup(), which
  // completes before send_message's background backfill even starts).
  // message-content-ready (plus the poll/timeout fallback below, for a
  // missed event) is the sole trigger for re-fetching and finalizing.
  useEffect(() => {
    if (activeRunId === null) return

    let statusUnlisten: UnlistenFn | undefined
    let contentUnlisten: UnlistenFn | undefined
    let cancelled = false
    let settled = false
    let pollIntervalId: number | null = null
    let pollTimeoutId: number | null = null

    const clearPoll = () => {
      if (pollIntervalId !== null) {
        window.clearInterval(pollIntervalId)
        pollIntervalId = null
      }
      if (pollTimeoutId !== null) {
        window.clearTimeout(pollTimeoutId)
        pollTimeoutId = null
      }
    }

    // Reconciliation point (see this item's plan): re-fetch rather than
    // trusting liveContent as final -- avoids ChatPane's own live-rendered
    // text silently diverging from what's actually persisted. Idempotent
    // via `settled`: the message-content-ready listener and the poll/timeout
    // fallback both call this, and only the first to arrive should act.
    const finalize = (result: Awaited<ReturnType<typeof commands.listMessages>>) => {
      if (cancelled || settled) return
      settled = true
      clearPoll()
      if (result.status === 'ok') {
        setMessages(result.data)
        // Draft-ready signal (items.id=233): only for gate3Track usage, and
        // only the first time this run's assistant row is seen still
        // 'drafted' -- a later re-fetch (e.g. contextKey unchanged, a second
        // send on the same mount) would otherwise re-fire for the same
        // message once its status has already moved past 'drafted'.
        if (gate3Track) {
          const drafted = [...result.data]
            .reverse()
            .find(
              (m) =>
                m.sender === 'assistant' &&
                m.focus_run_id === activeRunId &&
                m.gate3_review_status === 'drafted',
            )
          // Hard guard, not just belt-and-suspenders: still correct under
          // the new design too -- a run with genuinely nothing to backfill
          // (a real failure) still fires message-content-ready, but the
          // drafted row's content stays empty, and onDraftReady must never
          // fire for that.
          if (drafted && drafted.content) {
            onDraftReady?.(drafted.id)
          }
        }
      }
      setActiveRunId(null)
      setLiveContent('')
      setLiveStepDisplayName(null)
    }

    listen<RunStatusPayload>('run-status-update', (event) => {
      const payload = event.payload
      if (payload.focus_run_id !== activeRunId) return

      // Replace, not concatenate: lifecycle.rs's output() phase persists
      // task_track.last_output() -- the most recent step's content only,
      // for both single- and multi-step Focuses -- never a join of every
      // step. Concatenating here (verified in this item's throwaway
      // reconciliation harness) would show the user a multi-paragraph
      // live view that then visibly collapses down to just the final
      // step's content the moment the run completes and this re-fetches.
      // Currently a latent-only concern for this item's own two call
      // sites (both "quick-ask", a single-step Focus, so there's only
      // ever one step_content event to begin with) but a real mismatch
      // for any future multi-step Focus wired through ChatPane.
      // R1 crisis-handling floor (items.id=297): crisis_resource_block is
      // mutually exclusive with step_content in practice (Rust never sets
      // both on the same emit -- see lifecycle.rs's emit_status_with_content
      // call sites), so this is a plain either/or, not a merge.
      if (payload.crisis_resource_block) {
        setLiveContent(payload.crisis_resource_block)
      } else if (payload.step_content) {
        setLiveContent(payload.step_content)
      }
      setLiveStepDisplayName(payload.step_display_name)
    }).then((fn) => {
      if (cancelled) {
        fn()
      } else {
        statusUnlisten = fn
      }
    })

    listen<MessageContentReadyPayload>('message-content-ready', (event) => {
      if (event.payload.focus_run_id !== activeRunId) return
      commands.listMessages(userId, personaId, contextKey).then(finalize)
    }).then((fn) => {
      if (cancelled) {
        fn()
      } else {
        contentUnlisten = fn
      }
    })

    // Fallback safety net (items.id=320): protects against the event above
    // being lost outright (app restart mid-run, IPC hiccup, the backend's
    // own bounded DB-write timeout), not just late.
    pollIntervalId = window.setInterval(() => {
      commands.listMessages(userId, personaId, contextKey).then((result) => {
        if (cancelled || settled) return
        const liveRow =
          result.status === 'ok'
            ? [...result.data]
                .reverse()
                .find((m) => m.sender === 'assistant' && m.focus_run_id === activeRunId)
            : undefined
        if (liveRow?.content) {
          finalize(result)
        }
      })
    }, CONTENT_POLL_INTERVAL_MS)

    pollTimeoutId = window.setTimeout(() => {
      commands.listMessages(userId, personaId, contextKey).then((result) => {
        if (cancelled || settled) return
        const liveRow =
          result.status === 'ok'
            ? [...result.data]
                .reverse()
                .find((m) => m.sender === 'assistant' && m.focus_run_id === activeRunId)
            : undefined
        if (!liveRow?.content) {
          setContentTimedOut(true)
        }
        finalize(result)
      })
    }, CONTENT_POLL_TIMEOUT_MS)

    return () => {
      cancelled = true
      clearPoll()
      statusUnlisten?.()
      contentUnlisten?.()
    }
  }, [activeRunId, userId, personaId, contextKey, gate3Track, onDraftReady])

  // Elapsed-time-aware "generating..." messaging for the gaps between step
  // reveals -- pure frontend presentation, no backend involvement.
  useEffect(() => {
    if (!isGenerating) {
      setElapsedSeconds(0)
      return
    }
    const startedAt = Date.now()
    elapsedIntervalRef.current = window.setInterval(() => {
      setElapsedSeconds(Math.floor((Date.now() - startedAt) / 1000))
    }, 1000)
    return () => {
      if (elapsedIntervalRef.current !== null) {
        window.clearInterval(elapsedIntervalRef.current)
        elapsedIntervalRef.current = null
      }
    }
  }, [isGenerating])

  const handleSend = useCallback(() => {
    const text = draft.trim()
    if (!text) return
    setDraft('')

    setSendError(null)
    commands
      .sendMessage(userId, personaId, contextKey, text, focusId, gate3Track)
      .then((result) => {
        if (result.status === 'ok') {
          setMessages(result.data)
          const lastAssistant = [...result.data]
            .reverse()
            .find((m) => m.sender === 'assistant')
          setActiveRunId(lastAssistant?.focus_run_id ?? null)
        } else {
          setSendError(result.error)
        }
      })
  }, [draft, userId, personaId, contextKey, focusId, gate3Track])

  // The transcript row that should render liveContent instead of its own
  // (still-empty, not-yet-backfilled) content -- the placeholder assistant
  // message whose focus_run_id matches the currently-active run.
  const liveMessageId =
    activeRunId === null
      ? null
      : [...messages]
          .reverse()
          .find((m) => m.sender === 'assistant' && m.focus_run_id === activeRunId)
          ?.id ?? null

  return (
    <div className="chat-pane">
      <div className="chat-pane__transcript">
        {loadError && (
          <p role="alert">
            {t('navShell.chat.loadError', { message: loadError })}
          </p>
        )}
        {messages.length === 0 && !loadError && (
          <p>{t('navShell.chat.emptyTranscript')}</p>
        )}
        <ul className="chat-pane__message-list">
          {messages.map((m) => (
            <li key={m.id} className={`chat-pane__message chat-pane__message--${m.sender}`}>
              <span className="chat-pane__message-content">
                {m.id === liveMessageId && liveContent ? liveContent : m.content}
              </span>
              {m.gate3_review_status === 'pending-review' && (
                <span className="chat-pane__pending-review-notice">
                  {t('navShell.chat.pendingReviewNotice')}
                </span>
              )}
            </li>
          ))}
        </ul>
        {isGenerating && (
          <p className="chat-pane__generating" aria-live="polite">
            {liveStepDisplayName
              ? t('navShell.chat.generatingWithStep', {
                  step: liveStepDisplayName,
                  elapsed: elapsedSeconds,
                })
              : t('navShell.chat.generating', { elapsed: elapsedSeconds })}
          </p>
        )}
        {sendError && (
          <p role="alert" className="chat-pane__send-error">
            {t('navShell.chat.sendError', { message: sendError })}
          </p>
        )}
        {contentTimedOut && (
          <p role="alert" className="chat-pane__content-timeout">
            {t('navShell.chat.contentTimeout')}
          </p>
        )}
      </div>
      <form
        className="chat-pane__input-row"
        onSubmit={(e) => {
          e.preventDefault()
          handleSend()
        }}
      >
        <label className="chat-pane__input-label" htmlFor={`chat-pane-input-${contextKey}`}>
          {t('navShell.chat.inputLabel')}
        </label>
        <input
          id={`chat-pane-input-${contextKey}`}
          type="text"
          className="chat-pane__input"
          value={draft}
          placeholder={t('navShell.chat.inputPlaceholder')}
          onChange={(e) => setDraft(e.target.value)}
        />
        <button type="submit" disabled={draft.trim().length === 0}>
          {t('navShell.chat.sendButton')}
        </button>
      </form>
    </div>
  )
}
