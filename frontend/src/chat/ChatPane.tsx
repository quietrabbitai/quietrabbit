// Real chat/transcript component -- the thing behind MiddleZone's chatPane
// prop, replacing the placeholder <p> stubs previously in NavShell.tsx's
// personaHub branch and Tier3AccessPane.tsx's conversation pane. One
// component for both: gate3Track is the only behavioral difference (whether
// the assistant reply gets gate3_review_status="drafted").
//
// keyHex is null at both call sites today -- there is no code path for this
// frontend to obtain a real key_hex yet (see navShellConfig.ts's
// requireCurrentUserId() note: the real-session pattern built for user_id
// is deliberately NOT extended to key_hex, items.id=268). When null, send() never calls
// IPC at all -- it short-circuits client-side with an actionable "not sent"
// notice, same bar as commands/library.rs's prepare_clipboard_copy blocked
// message (states what happened and what the user can still do, no dead
// affordances). Real IPC wiring (commands.sendMessage/listMessages) is
// fully built and used the moment a real keyHex is supplied.

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
  /** null until Layer 8 auth exists -- see this file's header comment. */
  keyHex: string | null
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
}

const TERMINAL_STATUSES = new Set([
  'complete',
  'failed',
  'cancelled',
  'awaiting_user',
  'awaiting_feedback',
])

interface LocalUnsentMessage {
  id: string
  content: string
}

export function ChatPane({
  contextKey,
  userId,
  personaId,
  keyHex,
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
  const [localUnsent, setLocalUnsent] = useState<LocalUnsentMessage[]>([])
  const [showNotSentNotice, setShowNotSentNotice] = useState(false)

  const [activeRunId, setActiveRunId] = useState<string | null>(null)
  const [liveStepDisplayName, setLiveStepDisplayName] = useState<
    string | null
  >(null)
  const [liveContent, setLiveContent] = useState('')
  const [elapsedSeconds, setElapsedSeconds] = useState(0)
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
    setLocalUnsent([])
    setShowNotSentNotice(false)
    setSendError(null)
    setActiveRunId(null)
    setLiveContent('')
    setLiveStepDisplayName(null)

    if (keyHex === null) {
      // No real session to fetch against -- nothing was ever really sent
      // for this contextKey, so an empty transcript is the honest state,
      // not a fetch failure. See this file's header comment.
      setMessages([])
      setLoadError(null)
      return
    }

    setLoadError(null)
    commands.listMessages(userId, personaId, keyHex, contextKey).then(
      (result) => {
        if (result.status === 'ok') {
          setMessages(result.data)
        } else {
          setLoadError(result.error)
        }
      },
    )
  }, [contextKey, userId, personaId, keyHex])

  // First listen() call in this frontend (see this file's header comment on
  // RunStatusPayload) -- effect-returns-cleanup-closure shape, matching
  // MiddleZone's debounce-timer cleanup and Tier3AccessPane's ResizeObserver
  // cleanup, per CLAUDE.md's "Tauri event listeners must be explicitly
  // detached on SPA view unmount."
  useEffect(() => {
    if (activeRunId === null) return

    let unlisten: UnlistenFn | undefined
    let cancelled = false

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
      if (payload.step_content) {
        setLiveContent(payload.step_content)
      }
      setLiveStepDisplayName(payload.step_display_name)

      if (TERMINAL_STATUSES.has(payload.status)) {
        // Reconciliation point (see this item's plan): the backend has by
        // now backfilled the placeholder assistant message's real content
        // (commands::messages::send_message's background completion hook),
        // so re-fetch rather than trusting liveContent as final -- avoids
        // ChatPane's own live-rendered text silently diverging from what's
        // actually persisted.
        commands.listMessages(userId, personaId, keyHex ?? '', contextKey).then(
          (result) => {
            if (cancelled) return
            if (result.status === 'ok') {
              setMessages(result.data)
              // Draft-ready signal (items.id=233): only for gate3Track
              // usage, and only the first time this run's assistant row is
              // seen still 'drafted' -- a later re-fetch (e.g. contextKey
              // unchanged, a second send on the same mount) would otherwise
              // re-fire for the same message once its status has already
              // moved past 'drafted'.
              if (gate3Track) {
                const drafted = [...result.data]
                  .reverse()
                  .find(
                    (m) =>
                      m.sender === 'assistant' &&
                      m.focus_run_id === activeRunId &&
                      m.gate3_review_status === 'drafted',
                  )
                if (drafted) {
                  onDraftReady?.(drafted.id)
                }
              }
            }
            setActiveRunId(null)
            setLiveContent('')
            setLiveStepDisplayName(null)
          },
        )
      }
    }).then((fn) => {
      if (cancelled) {
        fn()
      } else {
        unlisten = fn
      }
    })

    return () => {
      cancelled = true
      unlisten?.()
    }
  }, [activeRunId, userId, personaId, keyHex, contextKey, gate3Track, onDraftReady])

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

    if (keyHex === null) {
      setLocalUnsent((prev) => [
        ...prev,
        { id: `local-${Date.now()}-${prev.length}`, content: text },
      ])
      setShowNotSentNotice(true)
      return
    }

    setSendError(null)
    commands
      .sendMessage(userId, personaId, keyHex, contextKey, text, focusId, gate3Track)
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
  }, [draft, keyHex, userId, personaId, contextKey, focusId, gate3Track])

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
        {messages.length === 0 && localUnsent.length === 0 && !loadError && (
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
          {localUnsent.map((m) => (
            <li
              key={m.id}
              className="chat-pane__message chat-pane__message--user chat-pane__message--unsent"
            >
              <span className="chat-pane__message-content">{m.content}</span>
              <span className="chat-pane__unsent-badge">
                {t('navShell.chat.unsentBadge')}
              </span>
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
        {showNotSentNotice && (
          <p className="chat-pane__not-sent-notice">
            {t('navShell.chat.notSentNotice')}
          </p>
        )}
        {sendError && (
          <p role="alert" className="chat-pane__send-error">
            {t('navShell.chat.sendError', { message: sendError })}
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
