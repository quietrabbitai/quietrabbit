// Real chat/transcript component -- the thing behind MiddleZone's chatPane
// prop, replacing the placeholder <p> stubs previously in NavShell.tsx's
// personaHub branch and CloudChatAccessPane.tsx's conversation pane. One
// component for both: gate3Track is the only behavioral difference (whether
// the assistant reply gets gate3_review_status="drafted").

import { useCallback, useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import {
  commands,
  type AutoResolvedSpan,
  type MessageInfo,
  type PendingCrossPersonaFact,
} from '../bindings'
import { CrossPersonaConfirmModal, type CrossPersonaFactDecision } from './CrossPersonaConfirmModal'
import { classifyRunStatus, findInFlightAssistantMessage } from './lateReplyRecovery'
import { findLastCopyableReply } from './copyLastResponse'
import { applyAllCopyDecisions, COPY_REVIEW_RUN_ID_PREFIX } from './copyDecisions'
import { devSimulatedCopyFlag } from './devSimulatedCopyFlag'
import {
  PrivacyGuardianModal,
  type ConsentRequestPayload,
  type ElementDecision,
} from '../cloudChatAccess/PrivacyGuardianModal'
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
   *  invoking commands.requestCloudFrontierGate3Review with the given messageId. */
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
  /** items.id=501 slice 2: true while a flagged copy's Privacy Guardian
   *  modal is open. The caller hides any native provider pane while it is,
   *  because a native pane draws above the webview and would cover the modal. */
  onCopyModalOpenChange?: (open: boolean) => void
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
// items.id=587: no longer a hard give-up point -- see the pollTimeoutId
// handler below. This is purely "how long before the informational banner
// first appears," so the original margin still applies unchanged.
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

/** Where a copy review started: the native copy event (Ctrl+C / right-click)
 *  or the "Copy last response" button. Native copies report in the
 *  transcript; button copies report on the button itself (no separate
 *  notice slot, and the compact floor has no transcript at all). */
type CopySource = 'native' | 'button'

type CopyReviewState =
  | { phase: 'idle' }
  | { phase: 'scanning'; source: CopySource }
  | { phase: 'passed'; source: CopySource; includesWithheld: boolean }
  | { phase: 'blocked'; source: CopySource; message: string | null }
  | { phase: 'failed'; source: CopySource }
  /** items.id=501 slice 2 (decisions.id=846 addendum Q2): the copy review
   *  flagged something. The Privacy Guardian modal is open; `text` is the
   *  stored copy (completion uses it, NOT a re-run -- a re-run mints a new
   *  review key, so the decisions would not be found). `payload` is null for
   *  the instant between the command returning and the consent_request event
   *  arriving (the modal shows its scanning state). Cancelling returns to
   *  idle without touching the clipboard. */
  | {
      phase: 'awaiting-consent'
      source: CopySource
      text: string
      includesWithheld: boolean
      payload: ConsentRequestPayload | null
      /** Spans gate3 resolved silently from a prior/standing decision --
       *  not in `payload`, but their decisions apply to the copied text too. */
      autoResolved: AutoResolvedSpan[]
    }
  /** decisions.id=766's standalone-withheld carve-out: copying exactly one
   *  previously-withheld message needs a harder re-confirmation, not the
   *  same pass/fail scan a fresh composition gets. */
  | { phase: 'confirm-withheld'; source: CopySource; text: string }

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
  onCopyModalOpenChange,
}: ChatPaneProps) {
  const { t } = useTranslation()
  const [messages, setMessages] = useState<MessageInfo[]>([])
  const [loadError, setLoadError] = useState<string | null>(null)
  const [draft, setDraft] = useState('')
  const [sendError, setSendError] = useState<string | null>(null)
  /** items.id=416: state machine for the native-copy Gate3 review UX --
   *  see the CopyReviewState/COPY_REVIEW_* constants above this component. */
  const [copyReview, setCopyReview] = useState<CopyReviewState>({ phase: 'idle' })
  const copyReviewClearRef = useRef<number | null>(null)
  /** A copy-review consent_request payload that arrived before the copy
   *  command returned (the event is emitted from inside gate3, before the
   *  command's own response) -- consumed when the copy moves to
   *  awaiting-consent. */
  const copyPayloadRef = useRef<ConsentRequestPayload | null>(null)

  const [activeRunId, setActiveRunId] = useState<string | null>(null)
  const [liveStepDisplayName, setLiveStepDisplayName] = useState<
    string | null
  >(null)
  const [liveContent, setLiveContent] = useState('')
  const [elapsedSeconds, setElapsedSeconds] = useState(0)
  /** items.id=320: set once the CONTENT_POLL_TIMEOUT_MS fallback elapses
   *  with the placeholder row still empty -- surfaces a visible notice
   *  instead of silently leaving a blank bubble forever. items.id=587: no
   *  longer terminal -- polling keeps going past this point (see the
   *  pollTimeoutId/pollIntervalId effect below), so this is purely
   *  informational and gets cleared the moment real content arrives. */
  const [contentTimedOut, setContentTimedOut] = useState(false)
  /** items.id=587: focus_run_ids confirmed dead (classifyRunStatus ===
   *  'dead' -- failed or cancelled) whose placeholder is still empty.
   *  Rendered as a plain "didn't finish" fallback in place of the empty
   *  content for that row (see the message-list render below) -- scoped to
   *  this mount, same lifetime as every other live-tracking state here;
   *  re-derived fresh (via the mount effect's own status check) if the
   *  user leaves and comes back. */
  const [deadRunIds, setDeadRunIds] = useState<Set<string>>(new Set())
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
    setDeadRunIds(new Set())
    setProvenanceOmitted(false)

    let cancelledOnMount = false
    const mountRecheckTimeoutIds: number[] = []

    setLoadError(null)
    commands.listMessages(userId, personaId, contextKey).then(
      (result) => {
        if (result.status === 'ok') {
          setMessages(result.data)
          // items.id=587 (late-reply recovery): a fresh mount has no way to
          // know a run is still in flight for this context_key except by
          // noticing the signature a not-yet-backfilled placeholder leaves
          // behind (findInFlightAssistantMessage). But that signature alone
          // isn't enough -- it also matches a run that crashed, was killed,
          // or is paused at a consent/Gate3 step (whose placeholder is
          // intentionally still empty; that has its own UI elsewhere). A
          // real run_status check (classifyRunStatus) is required before
          // resuming: only 'active' plugs into the existing polling/
          // listener effect below (the same way handleSend's own post-send
          // setActiveRunId does, so reopening a chat via History while its
          // reply is genuinely still generating shows "Generating..." and
          // picks up the real content the moment it arrives); 'dead' marks
          // the row to render a plain "didn't finish" fallback instead;
          // 'inactive' (paused/awaiting_*/complete/unknown) does nothing --
          // left exactly as today's pre-existing rendering already handles
          // it.
          const inFlight = findInFlightAssistantMessage(result.data)
          if (inFlight?.focus_run_id) {
            const runId = inFlight.focus_run_id
            const inFlightId = inFlight.id
            commands.getRunStatus(runId, userId, personaId).then((statusResult) => {
              if (cancelledOnMount) return
              const status = statusResult.status === 'ok' ? statusResult.data : null
              const runClass = classifyRunStatus(status)
              if (runClass === 'active') {
                setActiveRunId(runId)
                return
              }
              // Grace-retry guard (live-confirmed 2026-10-03): confirmed
              // against the actual backend ordering (conductor/lifecycle.rs)
              // that cleanup() always writes focus_runs.status to its
              // terminal value and that write is awaited BEFORE
              // execute_full()/resume_execution() returns -- which is
              // BEFORE send_message's background task even calls
              // finalize_chat_reply (messages.rs), the thing that actually
              // writes this run's content. So this status read can land
              // terminal while the content write is still moments away, on
              // a success just as much as a failure -- not a rare race, a
              // guaranteed ordering. A single immediate re-check isn't
              // enough margin (confirmed live: a real 2500+-char reply was
              // still missing a beat later against an earlier,
              // single-recheck version of this guard). Retry a few times,
              // spaced out, before concluding anything -- only 'dead'
              // (failed/cancelled) ever marks the row as not finished;
              // 'inactive' (overwhelmingly "complete" read a beat early)
              // just stops trying and leaves today's pre-existing
              // empty-bubble rendering alone, same as it always has.
              const MOUNT_RECHECK_ATTEMPTS = 3
              let attempt = 0
              const tryContent = () => {
                if (cancelledOnMount) return
                commands.listMessages(userId, personaId, contextKey).then((recheck) => {
                  if (cancelledOnMount) return
                  if (recheck.status === 'ok') {
                    const freshRow = recheck.data.find((m) => m.id === inFlightId)
                    if (freshRow?.content) {
                      setMessages(recheck.data)
                      return
                    }
                  }
                  attempt += 1
                  if (attempt < MOUNT_RECHECK_ATTEMPTS) {
                    mountRecheckTimeoutIds.push(
                      window.setTimeout(tryContent, CONTENT_POLL_INTERVAL_MS),
                    )
                  } else if (runClass === 'dead') {
                    setDeadRunIds((prev) => new Set(prev).add(runId))
                  }
                })
              }
              tryContent()
            })
          }
        } else {
          setLoadError(result.error)
        }
      },
    )

    return () => {
      cancelledOnMount = true
      mountRecheckTimeoutIds.forEach((id) => window.clearTimeout(id))
    }
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
    // items.id=587: set once CONTENT_POLL_TIMEOUT_MS has already elapsed --
    // gates the run_status check below so a normal, fast reply never pays
    // for an extra IPC round trip on every single poll tick; only once
    // things are already unusual is the extra check worth its cost.
    let timedOut = false
    // items.id=587: consecutive checkStatusAndMaybeStop calls that found
    // the run 'dead' (failed/cancelled) with no content yet -- see that
    // function's own comment for why a single reading isn't enough.
    let deadStreak = 0
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
      // items.id=587: a late arrival (the fallback poll below now keeps
      // running past the timeout instead of giving up) must clear a banner
      // it already showed -- otherwise real content would render sitting
      // right under a stale "taking longer than expected" notice forever.
      setContentTimedOut(false)
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

    // items.id=587: the run_status-informed counterpart to finalize() above
    // -- used only once a run is confirmed 'dead' (failed/cancelled) AND
    // DEAD_STREAK_LIMIT consecutive content re-checks still found nothing
    // (see checkStatusAndMaybeStop's own comment on why both are required).
    // Marks the placeholder so the render below shows a plain "didn't
    // finish" instead of an empty bubble -- a genuine dead end, nothing
    // more will ever arrive. There is deliberately no "finalize as merely
    // inactive" counterpart: an 'inactive' run_status reading (paused/
    // awaiting_user/etc., but in practice overwhelmingly "complete" read a
    // beat before finalize_chat_reply's own content write lands) is not a
    // stop signal for a run this component is actively tracking -- content
    // for it will still arrive via the normal listener/poll above.
    const finalizeAsDead = () => {
      if (cancelled || settled) return
      settled = true
      clearPoll()
      setContentTimedOut(false)
      setDeadRunIds((prev) => new Set(prev).add(activeRunId))
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

    // items.id=587 (redesigned after a live regression, 2026-10-03): checks
    // real run_status for a run with no content yet. Confirmed against the
    // actual backend ordering (conductor/lifecycle.rs's execute_full/
    // resume_execution): cleanup() always writes focus_runs.status to its
    // terminal value (e.g. "complete", or "failed" via handle_step_failure)
    // and that write is awaited BEFORE execute_full()/resume_execution()
    // returns -- which is BEFORE send_message's background task even calls
    // finalize_chat_reply (messages.rs), the thing that actually writes the
    // reply content. So for a run this component is actively tracking in
    // this same session, run_status reads terminal *every single time*,
    // on a success just as much as a failure -- this is not a rare timing
    // race to patch around with one extra check, it is the guaranteed
    // order of operations. Confirmed live: a perfectly good 2500+-char
    // reply was silently dropped twice by an earlier, single-recheck
    // version of this guard, because finalize_chat_reply's own write
    // consistently lands more than one IPC round trip after cleanup()'s
    // status write.
    //
    // Conclusion: 'inactive' (overwhelmingly "complete" read a beat early)
    // is NOT a stop signal here -- content for this run WILL arrive, via
    // the message-content-ready listener or the interval poll above, exactly
    // as it always did before this item. Only 'dead' (failed/cancelled)
    // means nothing more is coming on the success path, but
    // finalize_chat_reply still owes this run an error-text write even
    // then, under the same ordering -- so 'dead' requires DEAD_STREAK_LIMIT
    // consecutive dead-and-still-no-content readings (spaced
    // CONTENT_POLL_INTERVAL_MS apart, reusing the interval's own cadence
    // instead of inventing a separate retry timer) before finalizeAsDead()
    // actually gives up. `onActive` is the only caller that needs to react
    // to 'active' (the pollTimeoutId handler, to raise the informational
    // banner); 'inactive' and a not-yet-confirmed 'dead' both simply return
    // and let the existing poll/listener keep going untouched.
    const DEAD_STREAK_LIMIT = 2
    const checkStatusAndMaybeStop = (onActive?: () => void) => {
      commands.getRunStatus(activeRunId, userId, personaId).then((statusResult) => {
        if (cancelled || settled) return
        const status = statusResult.status === 'ok' ? statusResult.data : null
        const runClass = classifyRunStatus(status)
        if (runClass === 'active') {
          deadStreak = 0
          onActive?.()
          return
        }
        if (runClass === 'inactive') return
        // runClass === 'dead' from here down.
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
            return
          }
          deadStreak += 1
          if (deadStreak >= DEAD_STREAK_LIMIT) finalizeAsDead()
        })
      })
    }

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
          return
        }
        if (timedOut) checkStatusAndMaybeStop()
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
        if (liveRow?.content) {
          // Content arrived in the same tick the timeout fired -- resolve
          // normally, same as the interval poll below would have.
          finalize(result)
          return
        }
        // items.id=587: still nothing after CONTENT_POLL_TIMEOUT_MS -- check
        // real run_status right away (rather than waiting for the next
        // interval tick) so an already-dead/inactive run resolves
        // immediately instead of sitting under the banner for one more
        // CONTENT_POLL_INTERVAL_MS. The informational banner only makes
        // sense for a confirmed-active run -- onActive is the ONLY place
        // that raises it. `timedOut` is set regardless of the outcome, so
        // later interval ticks check status too (a no-op once this has
        // already settled things).
        timedOut = true
        checkStatusAndMaybeStop(() => setContentTimedOut(true))
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

  const lastAssistantSnippet = lastAssistantMessage
    ? lastAssistantMessage.id === liveMessageId && liveContent
      ? liveContent
      : lastAssistantMessage.content
    : null

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
    (text: string, includesWithheld: boolean, source: CopySource) => {
      let revealed = false
      let settled = false

      const revealTimer = window.setTimeout(() => {
        revealed = true
        setCopyReview({ phase: 'scanning', source })
      }, COPY_REVIEW_REVEAL_THRESHOLD_MS)

      const finish = (state: CopyReviewState) => {
        if (revealed) {
          window.setTimeout(() => showCopyReviewOutcome(state), COPY_REVIEW_MIN_HOLD_MS)
        } else {
          showCopyReviewOutcome(state)
        }
      }

      // items.id=501 slice 2: a flagged copy hands over to the Privacy
      // Guardian modal. No timer applies from here on -- the 4s cap below
      // guards only the backend request (cleared when it returns), and
      // writing the clipboard happens later, in the continuation of the
      // modal's own confirm click (a fresh user gesture), not the copy click.
      const awaitConsent = (
        payload: ConsentRequestPayload | null,
        autoResolved: AutoResolvedSpan[],
      ) => {
        if (copyReviewClearRef.current !== null) {
          window.clearTimeout(copyReviewClearRef.current)
          copyReviewClearRef.current = null
        }
        setCopyReview({
          phase: 'awaiting-consent',
          source,
          text,
          includesWithheld,
          payload,
          autoResolved,
        })
      }

      const capTimer = window.setTimeout(() => {
        if (settled) return
        settled = true
        window.clearTimeout(revealTimer)
        // Gap 2 floor: never fail silently, even on a proactive give-up
        // rather than a real writeText() rejection.
        finish({ phase: 'failed', source })
      }, COPY_REVIEW_TRANSIENT_ACTIVATION_CAP_MS)

      copyPayloadRef.current = null
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
            finish({ phase: 'failed', source })
            return
          }
          const data = result.data
          if (data.pending_consent) {
            const payload = copyPayloadRef.current
            copyPayloadRef.current = null
            awaitConsent(payload, data.auto_resolved)
            return
          }
          if (data.approved) {
            // Dev-only: stands in for flags/silent resolutions the Privacy
            // Filter would have produced (none exists in the dev build).
            // `import.meta.env.DEV` is a literal Vite replaces with `false`
            // in a production build, so this call (and the whole module)
            // is eliminated there.
            const sim = import.meta.env.DEV
              ? devSimulatedCopyFlag(
                  text,
                  `${COPY_REVIEW_RUN_ID_PREFIX}${personaId}-sim-${Date.now()}`,
                )
              : null
            const autoResolved = [...data.auto_resolved, ...(sim?.auto ?? [])]
            if (sim?.payload) {
              awaitConsent(sim.payload, autoResolved)
              return
            }
            // Spans gate3 resolved silently (all of them, since nothing was
            // left for the modal) still apply to the copied text -- a
            // keep_private span must never reach the clipboard unredacted.
            // Null (bad offsets) fails the copy; there is no fallback to
            // the original.
            const approvedText =
              autoResolved.length > 0 ? applyAllCopyDecisions(text, [], [], autoResolved) : text
            if (approvedText === null) {
              finish({ phase: 'failed', source })
              return
            }
            void navigator.clipboard
              .writeText(approvedText)
              .then(() => {
                // Silent on a fast, unflagged native pass -- "no visible
                // interruption at all" per this item's own UX spec. The
                // withheld-inclusion flag is informational, and the button
                // always confirms ("Copied"); both always surface.
                if (revealed || includesWithheld || source === 'button') {
                  finish({ phase: 'passed', source, includesWithheld })
                }
              })
              .catch(() => {
                // Gap 2 floor: writeText() rejected (transient activation
                // expired) -- explicit failure, never a silent no-op.
                finish({ phase: 'failed', source })
              })
            return
          }
          finish({ phase: 'blocked', source, message: data.plain_language })
        })
        .catch(() => {
          if (settled) return
          settled = true
          window.clearTimeout(revealTimer)
          window.clearTimeout(capTimer)
          finish({ phase: 'failed', source })
        })
    },
    [userId, personaId, showCopyReviewOutcome],
  )

  // items.id=501 slice 2 (decisions.id=846 addendum Q2 option A): the user
  // confirmed the flagged copy's modal. One action, one outcome: record the
  // decisions (keyed to the copy review's synthetic key -- no message row),
  // then write the DECIDED text to the clipboard. Both happen here, in the
  // continuation of the modal's confirm click, so the clipboard write has a
  // fresh user gesture even when the modal was open long past the original
  // click's transient-activation window. Fails closed: if the decisions
  // cannot be recorded or applied, nothing is copied.
  const handleCopyConsentResolve = useCallback(
    (decisions: ElementDecision[]) => {
      const pending = copyReview
      if (pending.phase !== 'awaiting-consent' || !pending.payload) return
      const { source, text, includesWithheld, payload, autoResolved } = pending
      const finalText = applyAllCopyDecisions(text, payload.spans, decisions, autoResolved)
      if (finalText === null) {
        showCopyReviewOutcome({ phase: 'failed', source })
        return
      }
      setCopyReview({ phase: 'scanning', source })
      commands
        .submitChatCopyConsentDecision({
          run_id: payload.focus_run_id,
          user_id: userId,
          persona_id: personaId,
          decisions_json: JSON.stringify(decisions),
        })
        .then((result) => {
          if (result.status !== 'ok') {
            showCopyReviewOutcome({ phase: 'failed', source })
            return
          }
          return navigator.clipboard
            .writeText(finalText)
            .then(() => showCopyReviewOutcome({ phase: 'passed', source, includesWithheld }))
        })
        .catch(() => showCopyReviewOutcome({ phase: 'failed', source }))
    },
    [copyReview, userId, personaId, showCopyReviewOutcome],
  )

  // Cancel leaves the clipboard untouched: nothing was written before the
  // modal (the native path already preventDefault()ed, the button never
  // writes first), and nothing is recorded.
  const handleCopyConsentCancel = useCallback(() => {
    copyPayloadRef.current = null
    setCopyReview({ phase: 'idle' })
  }, [])

  // decisions.id=766's standalone-withheld carve-out (distinct from Gap 1's
  // multi-message flag above): copying JUST one previously-withheld message
  // -- a decision the user already made once -- needs a harder
  // re-confirmation, not a fresh pass/fail scan.
  const requestWithheldReconfirm = useCallback((text: string, source: CopySource) => {
    if (copyReviewClearRef.current !== null) {
      window.clearTimeout(copyReviewClearRef.current)
      copyReviewClearRef.current = null
    }
    setCopyReview({ phase: 'confirm-withheld', source, text })
  }, [])

  const confirmWithheldCopy = useCallback(() => {
    if (copyReview.phase !== 'confirm-withheld') return
    const { source, text } = copyReview
    void navigator.clipboard
      .writeText(text)
      .then(() => {
        // The button promises "Copied" on success; the native path stays
        // silent here as before.
        if (source === 'button') {
          showCopyReviewOutcome({ phase: 'passed', source, includesWithheld: false })
        } else {
          setCopyReview({ phase: 'idle' })
        }
      })
      .catch(() => {
        showCopyReviewOutcome({ phase: 'failed', source })
      })
  }, [copyReview, showCopyReviewOutcome])

  const cancelWithheldCopy = useCallback(() => {
    setCopyReview({ phase: 'idle' })
  }, [])

  // The consent_request event carries a copy review's payload (focus_run_id
  // = the review's synthetic key). Buffer it for the command's own response
  // (the event fires first), or hand it to the already-open modal. Per-reply
  // review payloads are CloudChatAccessPane's and are ignored here. Detached
  // on unmount per CLAUDE.md.
  useEffect(() => {
    let unlisten: UnlistenFn | undefined
    let cancelled = false
    listen<ConsentRequestPayload>('consent_request', (event) => {
      if (!event.payload.focus_run_id.startsWith(COPY_REVIEW_RUN_ID_PREFIX)) return
      copyPayloadRef.current = event.payload
      setCopyReview((current) =>
        current.phase === 'awaiting-consent' && current.payload === null
          ? { ...current, payload: event.payload }
          : current,
      )
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
  }, [])

  const copyModalOpen = copyReview.phase === 'awaiting-consent'
  useEffect(() => {
    onCopyModalOpenChange?.(copyModalOpen)
  }, [copyModalOpen, onCopyModalOpenChange])

  // items.id=501 slice 2: "Copy last response". The newest assistant reply
  // only (see findLastCopyableReply); runs the SAME review as a native copy.
  const lastCopyableReply = findLastCopyableReply(messages)
  const handleCopyLast = useCallback(() => {
    const reply = findLastCopyableReply(messages)
    if (!reply) return
    if (reply.gate3_review_status === 'withheld') {
      requestWithheldReconfirm(reply.content, 'button')
      return
    }
    runCopyReview(reply.content, false, 'button')
  }, [messages, requestWithheldReconfirm, runCopyReview])

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
  //
  // items.id=547 fix: a native `copy` event's target/bubble path is the
  // FOCUSED element (or body/document if nothing is focused), never the
  // selection's own container (Clipboard API spec + confirmed live) --
  // message content here (<li>/<span>) has no tabIndex, so a mouse-drag
  // selection over transcript text never moves focus into a JSX
  // `onCopy`-bound <ul>, and that handler silently never fired. Attaching
  // at `document` instead sidesteps focus entirely -- `copy` always
  // bubbles to `document` regardless of what's focused -- and relevance is
  // determined the same way the per-li filter below already did: whether
  // the actual selection Range intersects this transcript's own list
  // element, a purely geometric check independent of focus. Deliberately
  // does NOT preventDefault until that check passes, so a copy elsewhere
  // in the app (including out of an <input>/<textarea>, whose own internal
  // selection window.getSelection() can't see at all) is left completely
  // untouched -- same passthrough-by-default safety the old bubble-scoped
  // handler had implicitly, just made explicit now that this listens
  // app-wide.
  const messageListRef = useRef<HTMLUListElement>(null)
  useEffect(() => {
    const handleDocumentCopy = (e: ClipboardEvent) => {
      const listEl = messageListRef.current
      if (!listEl) return

      const selection = window.getSelection()
      const text = selection?.toString() ?? ''
      if (!text || !selection || selection.rangeCount === 0) return

      const range = selection.getRangeAt(0)
      if (!range.intersectsNode(listEl)) return // copy is unrelated to this transcript

      e.preventDefault() // synchronous, before any async work

      const selectedMessages = Array.from(
        listEl.querySelectorAll<HTMLLIElement>('li[data-message-id]'),
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
        requestWithheldReconfirm(text, 'native')
        return
      }

      runCopyReview(text, includesWithheld, 'native')
    }

    document.addEventListener('copy', handleDocumentCopy)
    return () => document.removeEventListener('copy', handleDocumentCopy)
  }, [runCopyReview, requestWithheldReconfirm])

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

  // Disabled while a reply is generating, while a copy is in flight (scan,
  // modal, reconfirm), and when there is no copyable newest reply.
  const copyInFlight =
    copyReview.phase === 'scanning' ||
    copyReview.phase === 'awaiting-consent' ||
    copyReview.phase === 'confirm-withheld'
  const copyLastDisabled = isGenerating || copyInFlight || lastCopyableReply === null
  const copyLastLabel =
    copyReview.phase === 'scanning' || copyReview.phase === 'awaiting-consent'
      ? t('navShell.chat.copyLastChecking')
      : copyReview.phase === 'passed'
        ? t('navShell.chat.copyLastCopied')
        : copyReview.phase === 'failed' || copyReview.phase === 'blocked'
          ? t('navShell.chat.copyLastFailed')
          : t('navShell.chat.copyLastButton')

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
              (cloud-chat-collapsed-strip__name -- "Active Board", "Second
              opinion ready"/a provider name); this row led with the
              snippet instead, with nothing identifying it as QR's own bar
              at all. Same name QR's own expanded header uses
              (cloudChatAccessPane.qrBannerName) -- one string, reused, not a
              shorter alternate that could drift from it. */}
          <span className="chat-pane__collapsed-name">
            {t('navShell.cloudChatAccessPane.qrBannerName')}
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
          <ul className="chat-pane__message-list" ref={messageListRef}>
            {messages.map((m) => {
              // items.id=359 piece 6: an approved gate3 draft is a
              // visually distinct message type, not another plain bubble.
              // items.id=501 slice 2: its own "Copy starter" button is gone
              // (it wrote the clipboard without the copy review); copying
              // goes through "Copy last response" or a native copy. The
              // styling itself is retired with the per-reply review in
              // slice 3. 'pending-review' (pre-gate, below) keeps the
              // plain-bubble rendering.
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
                    {m.id === liveMessageId && liveContent
                      ? liveContent
                      : m.content === '' && m.focus_run_id && deadRunIds.has(m.focus_run_id)
                        ? t('navShell.chat.replyDidNotFinish')
                        : m.content}
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
          {copyReview.phase === 'scanning' && copyReview.source === 'native' && (
            <p className="chat-pane__copy-review-notice" aria-live="polite">
              {t('navShell.chat.copyScanning')}
            </p>
          )}
          {copyReview.phase === 'passed' && copyReview.source === 'native' && (
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
          {copyReview.phase === 'blocked' && copyReview.source === 'native' && (
            <p
              role="alert"
              className="chat-pane__copy-review-notice chat-pane__copy-review-notice--blocked"
            >
              {copyReview.message ?? t('navShell.chat.copyBlocked')}
            </p>
          )}
          {copyReview.phase === 'failed' && copyReview.source === 'native' && (
            <p
              role="alert"
              className="chat-pane__copy-review-notice chat-pane__copy-review-notice--failed"
            >
              {t('navShell.chat.copyFailed')}
            </p>
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
      {/* Outside the transcript branch on purpose: the compact floor renders
          no transcript, and the button's single-withheld reconfirm must show
          there too. */}
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
      <PrivacyGuardianModal
        open={copyReview.phase === 'awaiting-consent'}
        payload={copyReview.phase === 'awaiting-consent' ? copyReview.payload : null}
        onResolve={handleCopyConsentResolve}
        onCancel={handleCopyConsentCancel}
        onDecline={handleCopyConsentCancel}
      />
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
      {/* items.id=501 slice 2: the generic copy affordance, in the expanded
          chat and the compact floor alike. Its label is the feedback
          (no separate notice slot). */}
      <div className="chat-pane__copy-last-row">
        <button
          type="button"
          className="chat-pane__copy-last-button"
          disabled={copyLastDisabled}
          onClick={handleCopyLast}
        >
          {copyLastLabel}
        </button>
      </div>
    </div>
  )
}
