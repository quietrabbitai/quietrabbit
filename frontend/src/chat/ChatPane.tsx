// Real chat/transcript component -- the thing behind MiddleZone's chatPane
// prop, replacing the placeholder <p> stubs previously in NavShell.tsx's
// personaHub branch and CloudChatAccessPane.tsx's conversation pane. One
// component for both: gate3Track is the only behavioral difference (whether
// the assistant reply gets gate3_review_status="drafted").

import { useCallback, useEffect, useRef, useState, type ClipboardEvent } from 'react'
import { useTranslation } from 'react-i18next'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { commands, type MessageInfo, type PendingCrossPersonaFact } from '../bindings'
import { CrossPersonaConfirmModal, type CrossPersonaFactDecision } from './CrossPersonaConfirmModal'
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
   *  Persona-hub usage. The caller (CloudChatAccessPane) is responsible for
   *  invoking commands.requestTier3Gate3Review with the given messageId. */
  onDraftReady?: (messageId: string) => void
  /** items.id=359 (decisions.id=731): when true, renders only a minimal
   *  floor -- QR's mark, the latest assistant response as a snippet, and
   *  the existing entry bar below (kept mounted and focusable either
   *  way) -- instead of the full transcript. Driven by the caller
   *  (CloudChatAccessPane), never by this component's own state: this
   *  component owns message data, not the layout decision of whether
   *  Cloud Chat currently has focus. Omitted/false renders exactly
   *  as before this prop existed. */
  collapsed?: boolean
  /** Fires when the collapsed strip is clicked, or its entry input
   *  receives focus while collapsed -- the caller re-expands and returns
   *  whichever provider was active back to loaded (decisions.id=731's
   *  symmetric transition). Ignored when collapsed is false/omitted. */
  onExpand?: () => void
  /** items.id=391: fires whenever the latest assistant message changes
   *  (including to null, on mount/contextKey change before any messages
   *  have loaded, or for a transcript with no assistant turns yet) --
   *  lets the caller (CloudChatAccessPane's chat toolbar) offer an on-demand
   *  "2nd opinion" action against the real last response, reusing this
   *  component's own existing lastAssistantMessage lookup rather than
   *  duplicating message-list tracking one level up. Carries
   *  gate3_review_status alongside the id -- confirmed live (Jason,
   *  2026-09-02): requestTier3Gate3Review hard-rejects a message whose
   *  status is already terminal ("not awaiting gate3 review"), so the
   *  caller needs the status to decide whether "2nd opinion" should
   *  re-request review at all, not just which id to send. */
  onLastAssistantMessageChange?: (
    message: { id: string; gate3_review_status: string | null } | null,
  ) => void
  /** items.id=406 (decisions.id=755): fires on every "Copy starter" click,
   *  after the clipboard write -- lets the caller (CloudChatAccessPane) record
   *  which message/text was last copied, for the provider-selection
   *  re-check trigger's clipboard-provenance check (QR only ever evaluates
   *  clipboard content it can prove it wrote itself). Purely an
   *  observation of this component's own existing click-initiated copy
   *  action -- does not change what handleCopyStarter itself does. */
  onCopyStarter?: (messageId: string, content: string) => void
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
   *  cloud_frontier pause or step failure -- mutually exclusive with step_content in
   *  practice. */
  crisis_resource_block: string | null
  /** Cross-Persona entity_facts omitted from this run's context
   *  (decisions.id=815, items.id=27) -- declined, or caught by the
   *  documented pre-send-query/INITIALIZE-read race decisions.id=639
   *  accepts. Empty when nothing was omitted. No field_value, matching
   *  PendingCrossPersonaFact's own no-raw-values convention -- this only
   *  ever drives a generic notice, never names the actual value withheld. */
  provenance_omissions: OmittedCrossPersonaFact[]
}

/** Hand-declared, same convention as RunStatusPayload above -- mirrors
 *  conductor/types.rs::OmittedCrossPersonaFact, which is likewise only
 *  reachable via the run-status-update event, not any #[tauri::command]. */
interface OmittedCrossPersonaFact {
  field_name: string
  sensitivity: string
  origin_persona_id: string | null
}

/** Hand-declared, same convention/rationale as RunStatusPayload above --
 *  matches MessageContentReadyPayload (commands/messages.rs). items.id=320:
 *  the only reliable "safe to re-fetch now" signal. Every run-status-update
 *  status (including "awaiting_feedback") is emitted from inside
 *  execute_full_inner()/cleanup(), which completes and returns well before
 *  send_message's background backfill task even starts -- so no status on
 *  that event can be trusted to mean "list_messages will now show real
 *  content." This event is emitted unconditionally, once, only after that
 *  backfill attempt (success, crisis block, cloud_frontier/gate3 draft, or genuinely
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

// items.id=416 (decisions.id=766): speed-conditional copy-review UX
// constants. EMPIRICALLY MEASURED 2026-09-04 against this app's real
// installed webkit2gtk-4.1 2.52.6 (confirmed via `pacman -Q webkit2gtk-4.1`
// -- the Rust `webkit2gtk` crate version 2.0.2 in Cargo.lock is an unrelated
// binding-crate version, not the engine): a throwaway diagnostic screen
// swapped into main.tsx (reverted immediately after) ran
// navigator.clipboard.writeText() at increasing delays after a real click
// gesture, in the actual running dev window. Result: PASS through 4900ms,
// FAIL (NotAllowedError) at 5000ms -- i.e. this webkit2gtk build's real
// transient-activation window is essentially the commonly-cited ~5s
// Chromium/Firefox figure, NOT the ~1s-class WebKit-family failure this
// item's own research had flagged as a real risk (that risk does not
// materialize on this engine/version). REVEAL_THRESHOLD_MS +
// COPY_REVIEW_MIN_HOLD_MS (825ms total) has wide headroom under the ~4.9s
// real deadline as a result -- the tight coupling decisions.id=766 worried
// about does not bind here.
/** Below this, no "Scanning..." indicator shows at all -- Nielsen's
 *  ~0.1s "feels instantaneous" threshold, converged on ~140-200ms by
 *  several independent 2024-2026 UX sources as the point below which a
 *  loading indicator is pure noise. */
const COPY_REVIEW_REVEAL_THRESHOLD_MS = 175
/** Once "Scanning..." is shown, it stays up at least this long before
 *  flipping to the outcome -- prevents a several-frames-only flash reading
 *  as a glitch rather than information. */
const COPY_REVIEW_MIN_HOLD_MS = 650
/** How long a resolved outcome (passed/blocked/needs-review/failed) stays
 *  visible before auto-clearing back to idle. */
const COPY_REVIEW_DISPLAY_HOLD_MS = 2500
/** Gap 2 (transient activation expiry): rather than waiting out the full
 *  PF_TIMEOUT_SECS=10 backend budget only to fail anyway, give up and
 *  surface "Copy failed" once this elapses -- gives the scanning UI a
 *  clean, honest place to terminate. Set with a ~20% safety margin below
 *  the empirically measured ~4900ms real pass/fail boundary (see comment
 *  above) -- not the full measured ceiling, since gate3's own remaining
 *  network/processing time still has to fit inside this budget too. */
const COPY_REVIEW_TRANSIENT_ACTIVATION_CAP_MS = 4000

type CopyReviewState =
  | { phase: 'idle' }
  | { phase: 'scanning' }
  | { phase: 'passed'; includesWithheld: boolean }
  | { phase: 'blocked'; message: string | null }
  /** Gap 3's resolved simple fallback -- gate3 already runs automatically
   *  before content is ever visible/selectable (handleDraftReady), so a
   *  copy-triggered re-run reaching pending_consent here is the rare
   *  persistence-cascade-miss case, not the common path. A light prompt,
   *  not the full PrivacyGuardianModal. */
  | { phase: 'needs-review' }
  | { phase: 'failed' }
  /** decisions.id=766's standalone-withheld carve-out: copying exactly one
   *  previously-withheld message needs a harder re-confirmation, not the
   *  same pass/fail scan a fresh composition gets. */
  | { phase: 'confirm-withheld'; text: string }

export function ChatPane({
  contextKey,
  userId,
  personaId,
  focusId,
  gate3Track,
  onGenerating,
  onDraftReady,
  collapsed = false,
  onExpand,
  onLastAssistantMessageChange,
  onCopyStarter,
}: ChatPaneProps) {
  const { t } = useTranslation()
  const [messages, setMessages] = useState<MessageInfo[]>([])
  const [loadError, setLoadError] = useState<string | null>(null)
  const [draft, setDraft] = useState('')
  const [sendError, setSendError] = useState<string | null>(null)
  /** items.id=359 piece 6: which starter message's copy button most
   *  recently fired, for the transient "Copied" label swap -- cleared by
   *  its own timeout, not on every render. */
  const [copiedStarterId, setCopiedStarterId] = useState<string | null>(null)
  /** items.id=416: state machine for the native-copy Gate3 review UX --
   *  see the CopyReviewState/COPY_REVIEW_* constants above this component. */
  const [copyReview, setCopyReview] = useState<CopyReviewState>({ phase: 'idle' })
  const copyReviewClearRef = useRef<number | null>(null)

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

  /** Cross-Persona export confirmation (decisions.id=546/639/815,
   *  items.id=27): per-Persona, per-session (never persisted) bookkeeping of
   *  which entity_facts.id values this user has already answered -- keyed
   *  by personaId, not reset on a Persona switch (this component isn't
   *  remounted for one -- see CloudChatAccessPane.tsx's `{personaId ? <ChatPane
   *  .../> : ...}` branch), only ever cleared by this component unmounting
   *  (logout), matching KeyRegistry's own clear-on-logout lifetime. Declines
   *  are remembered too, so declining a fact once doesn't re-prompt on every
   *  following send -- if that's not the wanted behavior, drop the
   *  declinedFactIdsRef half and always re-offer declined facts instead. */
  const confirmedFactIdsRef = useRef<Map<string, Set<string>>>(new Map())
  const declinedFactIdsRef = useRef<Map<string, Set<string>>>(new Map())
  /** null = no confirmation prompt showing. Non-null = the facts newly
   *  pending as of the most recent pre-send query, awaiting a decision. */
  const [pendingCrossPersonaFacts, setPendingCrossPersonaFacts] = useState<
    PendingCrossPersonaFact[] | null
  >(null)
  /** Resolves the in-flight resolveCrossPersonaConfirmation() promise once
   *  the modal above is answered -- null (cancelled) aborts the send. */
  const crossPersonaResolverRef = useRef<((decisions: CrossPersonaFactDecision[] | null) => void) | null>(
    null,
  )
  /** Set when the most recent run's run-status-update carried a non-empty
   *  provenance_omissions -- the race/decline case decisions.id=815's
   *  live-signal requirement covers (see this item's plan). */
  const [provenanceOmitted, setProvenanceOmitted] = useState(false)
  /** Set when getPendingCrossPersonaConfirmations itself fails (network/IPC
   *  error, not a user decision) -- previously only a console.warn, invisible
   *  to the user. Fail-open behavior is unchanged: the send still proceeds
   *  with whatever was already confirmed this session; this only makes that
   *  fallback visible instead of silent. Cleared on the next send attempt,
   *  same convention as sendError. */
  const [crossPersonaCheckError, setCrossPersonaCheckError] = useState<string | null>(null)

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
    setCrossPersonaCheckError(null)
    setActiveRunId(null)
    setLiveContent('')
    setLiveStepDisplayName(null)
    setContentTimedOut(false)
    setProvenanceOmitted(false)

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
  // MiddleZone's debounce-timer cleanup and CloudChatAccessPane's ResizeObserver
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
      if (payload.provenance_omissions && payload.provenance_omissions.length > 0) {
        setProvenanceOmitted(true)
      }
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

  // Cross-Persona export confirmation (decisions.id=546/639/815, items.id=27):
  // the pre-Focus-start IPC query decisions.id=639 specifies, run before
  // every send -- not once per mount/Persona -- since a fact can flip to
  // cross_persona_export=true at any point in the session. Cheap on the
  // common path: an empty/already-answered result returns immediately with
  // no modal. Returns null if the user cancels the prompt (abort the send,
  // draft is preserved by the caller); otherwise the full accumulated
  // confirmed-id set for this Persona, ready to pass to sendMessage.
  const resolveCrossPersonaConfirmation = useCallback(async (): Promise<string[] | null> => {
    let confirmedSet = confirmedFactIdsRef.current.get(personaId)
    if (!confirmedSet) {
      confirmedSet = new Set()
      confirmedFactIdsRef.current.set(personaId, confirmedSet)
    }
    let declinedSet = declinedFactIdsRef.current.get(personaId)
    if (!declinedSet) {
      declinedSet = new Set()
      declinedFactIdsRef.current.set(personaId, declinedSet)
    }

    const result = await commands.getPendingCrossPersonaConfirmations({
      user_id: userId,
      persona_id: personaId,
    })
    if (result.status !== 'ok') {
      // Fail open on the query only -- apply_entity_fact_provenance_check
      // (lifecycle.rs) still omits anything unconfirmed regardless of why
      // this query didn't run; that omission then surfaces via this file's
      // own provenance_omissions notice instead of a pre-send prompt, this
      // one time. Never block sending over a failed pre-flight read -- but
      // the fallback must be visible, not just a console.warn (code-review
      // finding: crossPersonaConfirmCheckError was added to en.json but
      // never wired to anything).
      console.warn('getPendingCrossPersonaConfirmations failed:', result.error)
      setCrossPersonaCheckError(result.error)
      return Array.from(confirmedSet)
    }

    const newPending = result.data.filter(
      (f) => !confirmedSet!.has(f.fact_id) && !declinedSet!.has(f.fact_id),
    )
    if (newPending.length === 0) {
      return Array.from(confirmedSet)
    }

    setPendingCrossPersonaFacts(newPending)
    const decisions = await new Promise<CrossPersonaFactDecision[] | null>((resolve) => {
      crossPersonaResolverRef.current = resolve
    })
    setPendingCrossPersonaFacts(null)
    crossPersonaResolverRef.current = null

    if (decisions === null) {
      return null
    }
    for (const d of decisions) {
      if (d.include) {
        confirmedSet.add(d.fact_id)
        declinedSet.delete(d.fact_id)
      } else {
        declinedSet.add(d.fact_id)
      }
    }
    return Array.from(confirmedSet)
  }, [userId, personaId])

  const handleCrossPersonaResolve = useCallback((decisions: CrossPersonaFactDecision[]) => {
    crossPersonaResolverRef.current?.(decisions)
  }, [])

  const handleCrossPersonaCancel = useCallback(() => {
    crossPersonaResolverRef.current?.(null)
  }, [])

  const handleSend = useCallback(() => {
    const text = draft.trim()
    if (!text) return

    setSendError(null)
    setCrossPersonaCheckError(null)
    resolveCrossPersonaConfirmation().then((confirmedFactIds) => {
      if (confirmedFactIds === null) {
        // User cancelled the confirmation prompt -- abort the send. Draft
        // text is untouched (never cleared before this point), matching the
        // rest of this file's "don't lose what the user typed" convention.
        return
      }
      setDraft('')
      setProvenanceOmitted(false)
      commands
        .sendMessage({
          user_id: userId,
          persona_id: personaId,
          context_key: contextKey,
          content: text,
          focus_id: focusId,
          gate3_track: gate3Track,
          confirmed_cross_persona_fact_ids: confirmedFactIds,
        })
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
    })
  }, [draft, userId, personaId, contextKey, focusId, gate3Track, resolveCrossPersonaConfirmation])

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

  // items.id=359 piece 3: the collapsed floor's snippet -- the real last
  // response, not a placeholder (Jason's explicit build-time preference,
  // recorded in decisions.id=731). Same live-content substitution
  // liveMessageId already drives for the full transcript, so a
  // still-streaming response shows up here too, not just a finished one.
  const lastAssistantMessage = [...messages].reverse().find((m) => m.sender === 'assistant')

  const lastAssistantMessageId = lastAssistantMessage?.id ?? null
  const lastAssistantMessageReviewStatus = lastAssistantMessage?.gate3_review_status ?? null
  useEffect(() => {
    onLastAssistantMessageChange?.(
      lastAssistantMessageId === null
        ? null
        : { id: lastAssistantMessageId, gate3_review_status: lastAssistantMessageReviewStatus },
    )
  }, [
    lastAssistantMessageId,
    lastAssistantMessageReviewStatus,
    onLastAssistantMessageChange,
  ])

  const lastAssistantSnippet = lastAssistantMessage
    ? lastAssistantMessage.id === liveMessageId && liveContent
      ? liveContent
      : lastAssistantMessage.content
    : null

  // items.id=359 piece 6: click-initiated only, per the locked
  // no-passive-clipboard-monitoring rule -- this IS the click.
  const handleCopyStarter = useCallback(
    (messageId: string, content: string) => {
      void navigator.clipboard.writeText(content)
      setCopiedStarterId(messageId)
      onCopyStarter?.(messageId, content)
      window.setTimeout(() => {
        setCopiedStarterId((current) => (current === messageId ? null : current))
      }, 1400)
    },
    [onCopyStarter],
  )

  // items.id=416 (decisions.id=766): shows a copy-review outcome, then
  // auto-clears it back to idle after COPY_REVIEW_DISPLAY_HOLD_MS --
  // same "own timeout, not on every render" shape as copiedStarterId above.
  const showCopyReviewOutcome = useCallback((state: CopyReviewState) => {
    if (copyReviewClearRef.current !== null) {
      window.clearTimeout(copyReviewClearRef.current)
    }
    setCopyReview(state)
    copyReviewClearRef.current = window.setTimeout(() => {
      setCopyReview({ phase: 'idle' })
      copyReviewClearRef.current = null
    }, COPY_REVIEW_DISPLAY_HOLD_MS)
  }, [])

  // items.id=416 (decisions.id=766, core mechanism): extends Gate3 review to
  // EVERY copy of chat content, not just handleCopyStarter's dedicated
  // button -- previously an already-approved message got zero re-protection
  // against a plain manual copy, and a withheld message (durable,
  // permanently persisted, per messages_001.sql) rendered identically to
  // approved with no protection at all. Cross-message selection
  // (items.id=416 Gap 1): treated as ONE new composition -- a single
  // combined gate3 call against the concatenated text, not fragmented
  // per-message sub-reviews.
  const runCopyReview = useCallback(
    (text: string, includesWithheld: boolean) => {
      let revealed = false
      let settled = false

      const revealTimer = window.setTimeout(() => {
        revealed = true
        setCopyReview({ phase: 'scanning' })
      }, COPY_REVIEW_REVEAL_THRESHOLD_MS)

      const finish = (state: CopyReviewState) => {
        if (revealed) {
          window.setTimeout(() => showCopyReviewOutcome(state), COPY_REVIEW_MIN_HOLD_MS)
        } else {
          showCopyReviewOutcome(state)
        }
      }

      const capTimer = window.setTimeout(() => {
        if (settled) return
        settled = true
        window.clearTimeout(revealTimer)
        // Gap 2 floor: never fail silently, even on a proactive give-up
        // rather than a real writeText() rejection.
        finish({ phase: 'failed' })
      }, COPY_REVIEW_TRANSIENT_ACTIVATION_CAP_MS)

      commands
        .requestChatCopyGate3Review({
          user_id: userId,
          persona_id: personaId,
          content_text: text,
        })
        .then((result) => {
          if (settled) return
          settled = true
          window.clearTimeout(revealTimer)
          window.clearTimeout(capTimer)

          if (result.status !== 'ok') {
            finish({ phase: 'failed' })
            return
          }
          const data = result.data
          if (data.pending_consent) {
            finish({ phase: 'needs-review' })
            return
          }
          if (data.approved) {
            void navigator.clipboard
              .writeText(text)
              .then(() => {
                // Silent on a fast, unflagged pass -- "no visible
                // interruption at all" per this item's own UX spec. The
                // withheld-inclusion flag is informational, not part of the
                // scan-timing UI it's meant to avoid, so it always surfaces.
                if (revealed || includesWithheld) {
                  finish({ phase: 'passed', includesWithheld })
                }
              })
              .catch(() => {
                // Gap 2 floor: writeText() rejected (transient activation
                // expired) -- explicit failure, never a silent no-op.
                finish({ phase: 'failed' })
              })
            return
          }
          finish({ phase: 'blocked', message: data.plain_language })
        })
    },
    [userId, personaId, showCopyReviewOutcome],
  )

  // decisions.id=766's standalone-withheld carve-out (distinct from Gap 1's
  // multi-message flag above): copying JUST one previously-withheld message
  // -- a decision the user already made once -- needs a harder
  // re-confirmation, not a fresh pass/fail scan.
  const requestWithheldReconfirm = useCallback((text: string) => {
    if (copyReviewClearRef.current !== null) {
      window.clearTimeout(copyReviewClearRef.current)
      copyReviewClearRef.current = null
    }
    setCopyReview({ phase: 'confirm-withheld', text })
  }, [])

  const confirmWithheldCopy = useCallback(() => {
    setCopyReview((current) => {
      if (current.phase !== 'confirm-withheld') return current
      void navigator.clipboard.writeText(current.text).catch(() => {
        showCopyReviewOutcome({ phase: 'failed' })
      })
      return { phase: 'idle' }
    })
  }, [showCopyReviewOutcome])

  const cancelWithheldCopy = useCallback(() => {
    setCopyReview({ phase: 'idle' })
  }, [])

  // items.id=416 (decisions.id=766, core mechanism): the native `copy`
  // event (Ctrl+C / right-click-copy) on this transcript -- same
  // click-initiated-only discipline as handleCopyStarter's own comment
  // above (this event IS the user's copy gesture), extended to cover every
  // copy path instead of just the dedicated button. Hard scope boundary
  // (must be preserved): never fires on or inspects clipboard contents from
  // outside this element, never a background poll, never
  // navigator.clipboard.readText() -- strictly reactive to a copy gesture
  // on QR's own rendered content, per the locked
  // no-passive-clipboard-monitoring rule.
  const handleTranscriptCopy = useCallback(
    (e: ClipboardEvent<HTMLUListElement>) => {
      e.preventDefault() // synchronous, before any async work

      const selection = window.getSelection()
      const text = selection?.toString() ?? ''
      if (!text || !selection || selection.rangeCount === 0) return

      const range = selection.getRangeAt(0)
      const selectedMessages = Array.from(
        e.currentTarget.querySelectorAll<HTMLLIElement>('li[data-message-id]'),
      ).filter((li) => range.intersectsNode(li))

      if (selectedMessages.length === 0) {
        // Selection touched no message content (pure chrome/whitespace) --
        // nothing Gate3-relevant to review.
        void navigator.clipboard.writeText(text)
        return
      }

      const statuses = selectedMessages.map((li) => li.dataset.gate3Status)
      const includesWithheld = statuses.includes('withheld')

      if (selectedMessages.length === 1 && statuses[0] === 'withheld') {
        requestWithheldReconfirm(text)
        return
      }

      runCopyReview(text, includesWithheld)
    },
    [runCopyReview, requestWithheldReconfirm],
  )

  // items.id=391 (Jason, 2026-09-02): the transcript never auto-scrolled
  // to the newest message at all -- confirmed as the actual blocker
  // behind "click 2nd opinion, forget to copy the QR chat message, quickly
  // click QR chat to copy it and go back to the Cloud Chat screen": the
  // approved starter message (this transcript's own copy affordance,
  // above) is always the LATEST message when it exists, but reclaiming
  // Chat re-expands the transcript wherever it happened to be scrolled --
  // top, on a fresh mount -- not to that message, so it could be scrolled
  // well out of view in anything but a short conversation. Scrolls to the
  // bottom whenever new messages arrive AND whenever re-expanding from
  // collapsed, so the thing the user almost certainly came back to look
  // at (the newest message) is immediately visible, no manual scrolling
  // needed before they can even find the copy button.
  const transcriptRef = useRef<HTMLDivElement>(null)
  useEffect(() => {
    if (collapsed) return
    const el = transcriptRef.current
    if (!el) return
    el.scrollTop = el.scrollHeight
  }, [collapsed, messages])

  useEffect(() => {
    return () => {
      if (copyReviewClearRef.current !== null) {
        window.clearTimeout(copyReviewClearRef.current)
      }
    }
  }, [])

  return (
    <div className="chat-pane" data-collapsed={collapsed ? '' : undefined}>
      {collapsed ? (
        <button
          type="button"
          className="chat-pane__collapsed-strip"
          onClick={() => onExpand?.()}
        >
          <span className="chat-pane__collapsed-mark" aria-hidden="true" />
          {/* items.id=391 (eleventh pass): confirmed live (Jason) -- "on
              the second opinion screen, there is no qr chat bar." Board's
              and Cloud Chat's own collapsed bars both lead with a name
              (tier3-collapsed-strip__name -- "Active Board", "Second
              opinion ready"/a provider name); this row led with the
              snippet instead, with nothing identifying it as QR's own bar
              at all. Same name QR's own expanded header uses
              (tier3AccessPane.qrBannerName) -- one string, reused, not a
              shorter alternate that could drift from it. */}
          <span className="chat-pane__collapsed-name">
            {t('navShell.tier3AccessPane.qrBannerName')}
          </span>
          <span className="chat-pane__collapsed-snippet">
            {lastAssistantSnippet ?? t('navShell.chat.collapsedEmptySnippet')}
          </span>
          <span className="chat-pane__collapsed-expand" aria-hidden="true">
            {t('navShell.chat.collapsedExpandLabel')}
          </span>
        </button>
      ) : (
        <div className="chat-pane__transcript" ref={transcriptRef}>
          {loadError && (
            <p role="alert">
              {t('navShell.chat.loadError', { message: loadError })}
            </p>
          )}
          {messages.length === 0 && !loadError && (
            <p>{t('navShell.chat.emptyTranscript')}</p>
          )}
          <ul className="chat-pane__message-list" onCopy={handleTranscriptCopy}>
            {messages.map((m) => {
              // items.id=359 piece 6: an approved gate3 draft is a
              // visually distinct message type, not another plain bubble
              // -- the thing meant to leave the device gets its own
              // explicit copy affordance. 'pending-review' (pre-gate,
              // below) deliberately keeps the plain-bubble rendering: no
              // copy affordance exists before the gate clears.
              if (m.gate3_review_status === 'approved') {
                return (
                  <li
                    key={m.id}
                    className="chat-pane__message chat-pane__message--starter"
                    data-message-id={m.id}
                    data-gate3-status={m.gate3_review_status ?? ''}
                  >
                    <span className="chat-pane__starter-label">
                      {t('navShell.chat.starterLabel')}
                    </span>
                    <span className="chat-pane__message-content">{m.content}</span>
                    <button
                      type="button"
                      className="chat-pane__starter-copy-button"
                      onClick={() => handleCopyStarter(m.id, m.content)}
                    >
                      {copiedStarterId === m.id
                        ? t('navShell.chat.starterCopiedLabel')
                        : t('navShell.chat.starterCopyButton')}
                    </button>
                  </li>
                )
              }
              return (
                <li
                  key={m.id}
                  className={`chat-pane__message chat-pane__message--${m.sender}`}
                  data-message-id={m.id}
                  data-gate3-status={m.gate3_review_status ?? ''}
                >
                  <span className="chat-pane__message-content">
                    {m.id === liveMessageId && liveContent ? liveContent : m.content}
                  </span>
                  {m.gate3_review_status === 'pending-review' && (
                    <span className="chat-pane__pending-review-notice">
                      {t('navShell.chat.pendingReviewNotice')}
                    </span>
                  )}
                </li>
              )
            })}
          </ul>
          {copyReview.phase === 'scanning' && (
            <p className="chat-pane__copy-review-notice" aria-live="polite">
              {t('navShell.chat.copyScanning')}
            </p>
          )}
          {copyReview.phase === 'passed' && (
            <p className="chat-pane__copy-review-notice" aria-live="polite">
              {t('navShell.chat.copyScanPassed')}
              {copyReview.includesWithheld && (
                <span className="chat-pane__copy-review-flag">
                  {' '}
                  {t('navShell.chat.copyIncludesWithheldFlag')}
                </span>
              )}
            </p>
          )}
          {copyReview.phase === 'blocked' && (
            <p
              role="alert"
              className="chat-pane__copy-review-notice chat-pane__copy-review-notice--blocked"
            >
              {copyReview.message ?? t('navShell.chat.copyBlocked')}
            </p>
          )}
          {copyReview.phase === 'needs-review' && (
            <p role="alert" className="chat-pane__copy-review-notice">
              {t('navShell.chat.copyNeedsReview')}
            </p>
          )}
          {copyReview.phase === 'failed' && (
            <p
              role="alert"
              className="chat-pane__copy-review-notice chat-pane__copy-review-notice--failed"
            >
              {t('navShell.chat.copyFailed')}
            </p>
          )}
          {copyReview.phase === 'confirm-withheld' && (
            <div className="chat-pane__copy-withheld-confirm" role="alertdialog">
              <p>{t('navShell.chat.copyWithheldConfirmTitle')}</p>
              <p>{t('navShell.chat.copyWithheldConfirmBody')}</p>
              <button type="button" onClick={confirmWithheldCopy}>
                {t('navShell.chat.copyWithheldConfirmConfirm')}
              </button>
              <button type="button" onClick={cancelWithheldCopy}>
                {t('navShell.chat.copyWithheldConfirmCancel')}
              </button>
            </div>
          )}
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
          {crossPersonaCheckError && (
            <p role="alert" className="chat-pane__send-error">
              {t('navShell.chat.crossPersonaConfirmCheckError', {
                message: crossPersonaCheckError,
              })}
            </p>
          )}
          {contentTimedOut && (
            <p role="alert" className="chat-pane__content-timeout">
              {t('navShell.chat.contentTimeout')}
            </p>
          )}
          {provenanceOmitted && (
            <p role="alert" className="chat-pane__send-error">
              {t('navShell.chat.provenanceOmissionNotice')}
            </p>
          )}
        </div>
      )}
      <CrossPersonaConfirmModal
        open={pendingCrossPersonaFacts !== null}
        facts={pendingCrossPersonaFacts ?? []}
        onResolve={handleCrossPersonaResolve}
        onCancel={handleCrossPersonaCancel}
      />
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
          onFocus={() => {
            if (collapsed) onExpand?.()
          }}
        />
        <button type="submit" disabled={draft.trim().length === 0}>
          {t('navShell.chat.sendButton')}
        </button>
      </form>
    </div>
  )
}
