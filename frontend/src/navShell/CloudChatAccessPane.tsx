// Cloud Chat -- rail + content-pane hosting (items.id=359,
// replacing the former two-box selector + fixed-split-column model,
// items.id=3/202/223's original harness-derived layout). items.id=384
// slice 4 (decisions.id=735) generalized the collapse mechanic into a
// true bidirectional dominance pair -- see this file's own inline notes
// below and useDominancePair.ts's header comment for what changed and
// why; the Gate3 outbound-review flow (handleDraftReady and everything
// below it) is UNCHANGED by that work -- none of it ever read
// openPaneIds/activeProviderId, only personaId/messageId.
//
// TIER3_ACCESS_MODEL.md Session 3 (decisions.id=731-733): the rail lists
// every candidate provider (no cap); at most one pane is ever composited
// at a time (activeProviderId, mirrored to Rust's own PaneManager.active_pane
// via commands.setActivePane -- see pane_host.rs); QR's own conversation
// collapses to a minimal floor whenever a provider is active, freeing
// near-full height for that provider's own page. QR is collapsed, not
// unmounted, while a provider is active -- ChatPane's own `collapsed`
// prop keeps its message state (and live entry bar) mounted throughout,
// per Jason's explicit build-time preference for showing the real last
// response in the collapsed floor, not a placeholder.
//
// decisions.id=735/738 (items.id=384 slice 4): the pair is now
// symmetric. When Cloud Chat is the expanded region (dominant === 'cloudChat' and
// Board isn't expanded either), the layout is the rail+content-pane, QR
// collapsed to its own row above it. Whenever Cloud Chat is NOT the expanded
// region -- Chat dominant, or Board expanded (floor) -- the rail+
// content-pane is replaced by CloudChatCollapsedStrip, an always-present
// click-to-expand bar (items.id=391 tenth pass: never hidden outright any
// more, just one of the three peer rows -- see WorkspaceShell.tsx's own
// header comment on the "three bars, one expanded" model). No live entry
// field on CloudChatCollapsedStrip regardless (that's the resolved answer to
// decisions.id=738's flagged question: only QR's own collapsed floor gets
// a live entry field, since only QR has a QR-owned conversation to keep
// live). Gate3 review UI (PrivacyGuardianModal and the blocked/withheld/
// ceiling-raise messaging) is rendered OUTSIDE this dominant-conditional
// -- deliberately: a draft can enter Gate3 review from a full-screen
// Chat send regardless of whether Cloud Chat currently has any pane loaded,
// so that UI must stay visible no matter which side is dominant. It was
// previously nested inside the rail column, which happened to always be
// visible pre-merge (the rail+content-pane never used to be hidden) --
// keeping it there after adding a dominant==='chat' branch that hides
// the rail entirely would have silently made an in-progress consent
// review invisible.
//
// items.id=233's outbound Privacy Guardian gate (PG_GATE_3,
// conductor/privacy/gate3.rs) ahead of the rail appearing at all is
// unchanged by this item -- see handleDraftReady/the consent_request
// listener below, carried over from the prior selector-screen version of
// this file.

import { useCallback, useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { commands, type ChatInfo, type ExternalAccess, type PaneRectFraction, type PersonaInfo } from '../bindings'
import { ChatPane } from '../chat/ChatPane'
import { ChatHistoryList } from '../chat/ChatHistoryList'
import { PersonaBox } from './persona/PersonaBox'
import { FocusSettingsControls } from './FocusSettingsControls'
import { requireCurrentUserId, type DominancePairState } from './navShellConfig'
import { CloudChatCollapsedStrip } from './CloudChatCollapsedStrip'
import { useDominancePair } from './useDominancePair'
import { computeActivePaneRect, pixelRectToFraction, type PanePixelRect } from '../cloudChatAccess/paneLayout'
import { PaneHitLayer } from '../cloudChatAccess/PaneHitLayer'
import { PopupHitLayer } from '../cloudChatAccess/PopupHitLayer'
import {
  PrivacyGuardianModal,
  type ConsentRequestPayload,
  type ElementDecision,
} from '../cloudChatAccess/PrivacyGuardianModal'
import { isAllKeptPrivate } from '../cloudChatAccess/consentDecisions'
import { CloudChatSelector } from '../cloudChatAccess/CloudChatSelector'
import {
  fetchActiveProviders,
  type Provider,
} from '../cloudChatAccess/cloudChatAccessConfig'

type ReviewOutcome = 'pending' | 'approved' | 'withheld' | 'blocked'

// items.id=234 -- host-owned popup subsystem. Hand-declared, not generated:
// event payloads, not command args, same convention as
// PrivacyGuardianModal.tsx's own ConsentRequestPayload -- see
// commands/cloud_chat_pane.rs's PopupOpenedPayload/PopupClosedPayload for the
// Rust side these mirror.
interface PopupOpenedPayload {
  provider_id: string
  rect: PaneRectFraction
}
interface PopupClosedPayload {
  provider_id: string
}

export interface CloudChatAccessPaneProps {
  /** The currently-active Persona (navShellConfig.ts's NavState.activePersonaId,
   *  a standing field independent of which top-level screen is showing --
   *  items.id=384 slice 1 removed the earlier capture-on-transition
   *  workaround this comment used to describe). Routinely null now --
   *  items.id=384 slice 3 removed the old gate requiring a Persona be
   *  selected before this screen is even reachable (WorkspaceShell.tsx
   *  mounts this component regardless), so null here means "no Persona
   *  chosen yet," a real, expected state, not just a brief render-order
   *  gap -- see the cloudChatUnavailable branch below. */
  personaId: string | null
  /** items.id=384 slice 7: the new-chat persona dot-picker's "quiet"
   *  switch -- see navShellConfig.ts's setActivePersonaId for why this is
   *  a distinct action from selecting a Persona via the top-strip/Persona
   *  hub. */
  onPersonaChange: (personaId: string) => void
  /** Already fetched once at NavShell's own top level -- threaded down
   *  through WorkspaceShell rather than re-fetched here. */
  personas: PersonaInfo[]
  /** NavState.workspace.pair -- see useDominancePair.ts. */
  pair: DominancePairState
  onUpdatePair: (updater: (prev: DominancePairState) => DominancePairState) => void
  /** items.id=391 (tenth pass), generalized by items.id=404: true whenever
   *  neither Chat nor Cloud Chat is the outer 5-rail dock's dominant rail
   *  (WorkspaceShell's `dominantRail !== 'chat' && dominantRail !== 'cloudChat'`
   *  -- Board, Library, or History being dominant all set this now, not
   *  just Board as pre-404). Collapses QR to a one-row floor (ChatPane's
   *  own collapsed strip: mark + last-message snippet + a real, focusable
   *  entry bar, matching the mockup's chat-floor) instead of unmounting
   *  it. No CEF-lifecycle reason to unmount: the concern was always about
   *  Cloud Chat's own open panes, and Board/Library/History becoming dominant
   *  already forces dominantRail away from 'cloudChat' first (each of their
   *  own dock-bar click handlers sets dominantRail directly, never
   *  through pair.dominant), so there's never an open pane actively
   *  compositing while floor is true. The dominant rail's own bar and the
   *  second-opinion bar (below) both still render while floor is true --
   *  see this file's header comment on the tenth-pass "three bars, one
   *  expanded" model, now five; floor only collapses QR's own row, not
   *  the others. */
  floor?: boolean
  /** items.id=391: fires when the floor (above) is clicked or its entry
   *  bar focused -- WorkspaceShell's own onDominantRailChange('chat'),
   *  making Chat the expanded region (a one-step swap as of the tenth
   *  pass, not a multi-stage growth -- see WorkspaceShell.tsx's own
   *  header comment). Distinct from reclaimChat (which this component
   *  uses for its OTHER collapse case, dominant === 'cloudChat'): dominant is
   *  already 'chat' whenever floor is true, so reclaiming it again would
   *  be a no-op. */
  onFloorExpand?: () => void
  /** items.id=404: fires whenever an action INSIDE this component means
   *  "make Chat or Cloud Chat the outer 5-rail dock's dominant rail" --
   *  CloudChatCollapsedStrip's own expand, the Cloud Chat content-head's "back to
   *  chat" click, and ChatPane's own non-floor collapsed-strip click (the
   *  dominant === 'cloudChat' case). See WorkspaceShell.tsx's own header
   *  comment for why this is a set of direct calls at those specific
   *  sites rather than a generic effect mirroring pair.dominant. */
  onDominantRailChange: (rail: 'chat' | 'cloudChat') => void
  /** items.id=404: ChatHistoryList's toggle no longer opens its own
   *  dropdown -- it calls this instead, which WorkspaceShell wires to
   *  make the History rail dominant with this Persona pre-selected. */
  onOpenHistory: (target: { personaId: string }) => void
  /** items.id=404: History's "resume this chat" row-action -- set by
   *  WorkspaceShell when a specific past chat is picked from History's
   *  action-pane. Consumed once (same shape as onFloorExpand): this
   *  effect switches to it, then calls onPendingChatSelectionConsumed so
   *  WorkspaceShell clears it and the effect doesn't refire. */
  pendingChatSelection?: ChatInfo | null
  onPendingChatSelectionConsumed?: () => void
}

// items.id=448: Gate3ReviewResult.target_tier deliberately stays a plain
// number (genuinely dual-purpose in gate3.rs -- also feeds
// destination_risk_rating's fallback, an unrelated risk-rating axis; see
// this draft's own "Flag for Chat-PM" section for the forward-looking
// concern this raises). This mirrors Rust's own
// ExternalAccess::from_legacy_tier mapping (conductor/tokens.rs) so the
// inline "raise it now" affordance can prefill its select with a real
// ExternalAccess value from that legacy number. Can never produce
// anonymous_preferred (no legacy slot maps to it) -- acceptable here since
// this is only a suggested starting value the user can still change.
function externalAccessFromLegacyTier(tier: number): ExternalAccess {
  switch (tier) {
    case 1:
      return 'local_only'
    case 2:
      return 'anonymous_required'
    case 3:
      return 'unrestricted'
    default:
      return 'unrestricted'
  }
}

export function CloudChatAccessPane({
  personaId,
  onPersonaChange,
  personas,
  pair,
  onUpdatePair,
  floor = false,
  onFloorExpand,
  onDominantRailChange,
  onOpenHistory,
  pendingChatSelection = null,
  onPendingChatSelectionConsumed,
}: CloudChatAccessPaneProps) {
  const { t } = useTranslation()
  const [providers, setProviders] = useState<Provider[]>([])
  const [providerError, setProviderError] = useState<string | null>(null)
  // items.id=384 slice 7 (decisions.id=739): null means "the pre-existing
  // flat tier3-access-{personaId} conversation" -- the default view, not
  // a loading state. Set to a real ChatInfo by either starting a new chat
  // (PersonaBox's onChange, wired to handleStartNewChat -- items.id=543
  // retired the standalone NewChatPersonaPicker this used to say) or
  // picking a past one (ChatHistoryList).
  // Deliberately NOT persisted to NavState -- unlike `pair`, losing this
  // selection on a Board-toggle remount just means the default view
  // reappears, not any data loss (nothing here is hidden/torn down the
  // way a live CEF pane would be).
  const [activeChat, setActiveChat] = useState<ChatInfo | null>(null)

  // items.id=543: PersonaBox's selector popover -- controlled here (not
  // internal to PersonaBox) because two triggers need to open the SAME
  // popover: the box itself, and clicking Chat's empty rail body while no
  // Persona is active yet (PERSONA_SELECTOR_DESIGN_ITEM543_20260921.md
  // Section 2.1).
  const [personaBoxOpen, setPersonaBoxOpen] = useState(false)

  // items.id=391: mirrors ChatPane's own lastAssistantMessage lookup --
  // see ChatPane.tsx's onLastAssistantMessageChange doc comment for why
  // it's pushed up (id AND gate3_review_status, not just the id) rather
  // than duplicated here. Drives the chat toolbar's "2nd opinion" button:
  // on-demand, real Gate3 review of the most recent response, without
  // composing a new message. Reset on personaId change for the same
  // reason activeChat is, just below.
  const [lastAssistantMessage, setLastAssistantMessage] = useState<{
    id: string
    gate3_review_status: string | null
  } | null>(null)

  // A chat belongs to exactly one Persona (decisions.id=741) -- if
  // personaId changes out from under this component (the Persona hub's
  // own Persona buttons still work independently of this pane, per
  // navShellConfig.ts's setActivePersonaId doc comment), whatever chat
  // was showing belongs to the OLD Persona and must not keep showing
  // under the new one. Falls back to the new Persona's own default view,
  // same as a fresh mount would.
  useEffect(() => {
    setActiveChat(null)
    setLastAssistantMessage(null)
  }, [personaId])

  const {
    dominant,
    openProviderIds,
    activeProviderId,
    openError,
    setOpenError,
    activate,
    close,
    reclaimChat,
    markCloudChatReady,
  } = useDominancePair(pair, onUpdatePair)

  // items.id=406 (decisions.id=755): the provider-selection re-check
  // trigger's own memory of what was last copied -- QR only ever evaluates
  // clipboard content it can prove it wrote itself (the hard provenance
  // boundary from the design doc), never arbitrary/external clipboard
  // contents. Cleared implicitly by comparison at check time, not on a
  // timer -- a stale entry is harmless: if the clipboard no longer holds
  // this exact text (the user copied something else, or pasted already),
  // the provenance check below simply won't match and no re-check fires.
  //
  // items.id=543 (Chat-BRAND finding, confirmed live this session): also
  // stores the copied message's own owning personaId, sourced from
  // ChatPane's personaId prop at copy time (see onCopyStarter below and
  // ChatPane.tsx's handleCopyStarter) -- NOT the live `personaId` prop on
  // this component. Persona is now freely switchable mid-session (this
  // whole item's point), so those two can genuinely diverge between a
  // copy and the later provider click; activateAndPromote below must key
  // its recheck off the message's real owning persona, not whatever's
  // active right now.
  const [lastCopiedStarter, setLastCopiedStarter] = useState<{
    messageId: string
    content: string
    personaId: string
  } | null>(null)

  // items.id=404: the three internal actions that mean "make Cloud Chat/Chat
  // the outer dock's dominant rail" -- see this component's own
  // onDominantRailChange doc comment and WorkspaceShell.tsx's header
  // comment for why these are direct wraps, not a generic effect.
  const activateAndPromote = useCallback(
    (providerId: string) => {
      onDominantRailChange('cloudChat')
      activate(providerId)

      // items.id=406 (decisions.id=755): provider-selection re-check --
      // fires alongside activation (not blocking it; see design doc's own
      // "achievable version" framing -- QR cannot observe paste itself,
      // only the two moments it CAN observe: copy, and selecting a new
      // destination). Mirrors handleDraftReady's own shape: reviewOutcome
      // is set to 'pending' BEFORE the command resolves so the modal is
      // already primed if the independent consent_request listener (below)
      // delivers a payload for it.
      if (lastCopiedStarter) {
        const activeIds = openProviderIds.includes(providerId)
          ? openProviderIds
          : [...openProviderIds, providerId]
        const messageId = lastCopiedStarter.messageId
        const starterPersonaId = lastCopiedStarter.personaId
        void navigator.clipboard
          .readText()
          .then((clipboardText) => {
            if (clipboardText !== lastCopiedStarter.content) return
            setReviewOutcome('pending')
            setReviewMessage(null)
            setReviewCeiling(null)
            setPendingMessageId(messageId)
            return commands.recheckCloudFrontierProviderSelection({
              user_id: requireCurrentUserId(),
              persona_id: starterPersonaId,
              message_id: messageId,
              newly_active_provider_ids: activeIds,
            })
          })
          .then((result) => {
            if (!result) return // provenance mismatch -- nothing was fired
            if (result.status !== 'ok') {
              setReviewOutcome('blocked')
              setReviewMessage(
                t('navShell.cloudChatAccessPane.gate3ReviewError', { message: result.error }),
              )
              return
            }
            const data = result.data
            if (data.pending_consent) {
              // Payload arrives via the consent_request listener.
              return
            }
            if (data.approved) {
              // No new review was actually needed (no-op path, or every
              // fact auto-resolved) -- back to whatever it was before this
              // check, not stuck showing 'pending'.
              setReviewOutcome('approved')
              return
            }
            setReviewOutcome('blocked')
            setReviewMessage(data.plain_language)
          })
          .catch(() => {
            // Clipboard read can reject (permissions, focus) -- a re-check
            // we can't confirm provenance for must not fire, so failing
            // closed (skip) is correct, not swallowed-error negligence.
          })
      }
    },
    [activate, onDominantRailChange, lastCopiedStarter, openProviderIds, t],
  )
  const markCloudChatReadyAndPromote = useCallback(() => {
    onDominantRailChange('cloudChat')
    markCloudChatReady()
  }, [markCloudChatReady, onDominantRailChange])
  const reclaimChatAndPromote = useCallback(() => {
    onDominantRailChange('chat')
    reclaimChat()
  }, [reclaimChat, onDominantRailChange])

  /** The content pane's own placeholder body -- its bounding rect IS the
   *  active pane's on-screen rect (computeActivePaneRect). Deliberately
   *  NOT the same element as the content-pane header (see this file's
   *  module doc / the session handoff): Rust composites CEF's texture
   *  directly into exactly this rect, so any DOM chrome meant to stay
   *  visibly on top (the header/close button) must live in a sibling
   *  element outside it, never inside. */
  const contentBodyRef = useRef<HTMLDivElement>(null)
  const [paneRects, setPaneRects] = useState<Record<string, PanePixelRect>>({})
  // items.id=234: backend-authoritative popup rects, keyed by parent
  // provider id -- see PopupHitLayer's own doc for why this is not
  // frontend-measured the way paneRects is. Stays local to this
  // component (not lifted into useDominancePair) -- popup bookkeeping is
  // CEF-pane-specific plumbing, not part of "which side is dominant."
  const [popupRects, setPopupRects] = useState<Record<string, PaneRectFraction>>({})

  const [reviewOutcome, setReviewOutcome] = useState<ReviewOutcome | null>(null)
  const [reviewMessage, setReviewMessage] = useState<string | null>(null)
  // items.id=321: set only when the block was Gate3's tier-ceiling check
  // (Gate3Result.target_tier/.space_max_permitted_tier, populated only on
  // that one path -- see conductor/privacy/gate3.rs). Drives the inline
  // "raise it now" affordance that replaces the old dead
  // "[Change Focus settings]" bracket text.
  const [reviewCeiling, setReviewCeiling] = useState<{
    targetTier: number
    current: ExternalAccess
  } | null>(null)
  const [consentPayload, setConsentPayload] = useState<ConsentRequestPayload | null>(null)
  // ConsentRequestPayload carries focus_run_id, not message_id -- gate3()
  // only knows about content_key/step_id/focus_run_id, never the
  // messages.db row that triggered it. Stashed here from handleDraftReady
  // so handleModalResolve has the right id to pass to resolveCloudFrontierGate3Review.
  const [pendingMessageId, setPendingMessageId] = useState<string | null>(null)

  // items.id=384 slice 4: the bridge between Gate3 review and dominance.
  // Pre-merge, the rail was simply always visible once reviewOutcome
  // reached 'approved' -- there was no separate "dominant" concept to
  // update. Post-merge, Cloud Chat must actually BECOME dominant at that same
  // moment (see useDominancePair.ts's own header comment for why
  // markCloudChatReady is a distinct trigger from activate/reclaimChat, and
  // why dominant is no longer purely derived from activeProviderId).
  // Fires from both paths that can set reviewOutcome to 'approved'
  // (handleDraftReady, handleModalResolve -- handleSecondOpinion below
  // reuses handleDraftReady rather than setting reviewOutcome itself) via
  // this one effect rather than duplicating the call at each site.
  //
  // BUG FOUND + FIXED (2026-09-01 live verification pass, items.id=384
  // slice 7): the original version of this effect fired
  // `if (reviewOutcome === 'approved')` unconditionally on every render,
  // not just on the actual transition INTO 'approved' -- a LEVEL trigger
  // where an EDGE trigger was needed. markCloudChatReady's own identity is
  // unstable across renders (it closes over `setPair`, which is
  // NavShell.tsx's `onUpdatePair` prop, itself a fresh closure on every
  // NavShell render, never memoized) -- so this effect's dependency array
  // never actually settles, and it re-ran on essentially every render
  // while reviewOutcome stayed 'approved'. In practice this meant
  // reclaimChat's own dominant:'chat' update got silently overwritten
  // back to 'cloudChat' on the very next render, PERMANENTLY breaking the
  // "click QR's collapsed floor to reclaim it" gesture for the rest of
  // the session, the instant any draft had ever been approved once.
  // Confirmed live: clicking the collapsed strip, its "Expand" label, and
  // focusing its entry field all correctly triggered reclaimChat, and all
  // three were silently reverted a moment later.
  //
  // Fix: track the PREVIOUS reviewOutcome in a ref and only call
  // markCloudChatReady on an actual null/blocked/withheld -> 'approved'
  // transition, matching what this effect was always meant to express
  // ("Gate3 just cleared") rather than what it accidentally implemented
  // ("Gate3 has cleared at some point and something else re-rendered").
  // This makes the effect correct regardless of markCloudChatReady's own
  // identity stability -- the deeper fix (memoizing onUpdatePair through
  // the whole prop chain so callback identities stay stable) is a real,
  // separate improvement flagged for its own pass, not applied here.
  const prevReviewOutcomeRef = useRef<ReviewOutcome | null>(null)
  useEffect(() => {
    if (reviewOutcome === 'approved' && prevReviewOutcomeRef.current !== 'approved') {
      // items.id=404: preserves pre-existing behavior -- pre-404, this
      // pair.dominant flip would already force Board (if expanded) back
      // to a bar next render via the old effectiveBoardSize guard. The
      // 5-rail model needs the promotion made explicit since there's no
      // such guard any more (dominantRail is set directly, not derived).
      markCloudChatReadyAndPromote()
    }
    prevReviewOutcomeRef.current = reviewOutcome
  }, [reviewOutcome, markCloudChatReadyAndPromote])

  const syncPaneLayout = useCallback(() => {
    const body = contentBodyRef.current
    if (!body) {
      // items.id=391: reachable via the ResizeObserver effect's own
      // requestAnimationFrame callback (below) if .content-body unmounts
      // in the gap between scheduling and the frame actually firing --
      // same stale-rect risk as the effect's own early return just above
      // it, so the same fix applies here too.
      setPaneRects({})
      void commands.setPaneLayout([])
      return
    }
    const rects = computeActivePaneRect(body.getBoundingClientRect(), activeProviderId)
    setPaneRects(rects)
    const entries = Object.entries(rects).map(([providerId, rect]) => ({
      provider_id: providerId,
      rect: pixelRectToFraction(rect, window.innerWidth, window.innerHeight),
    }))
    commands.setPaneLayout(entries).then((result) => {
      if (result.status !== 'ok') {
        setOpenError(result.error)
      }
    })
  }, [activeProviderId, setOpenError])

  useEffect(() => {
    const body = contentBodyRef.current
    if (!body) {
      // BUG FOUND + FIXED (2026-09-02, live-verification pass): reclaiming
      // Chat (or otherwise leaving Cloud Chat dominant) unmounts .content-body
      // -- syncPaneLayout itself never runs in that case (this early
      // return used to skip even calling it), so nothing ever told Rust
      // the active pane's rect was gone. Confirmed live (Jason): the
      // Cloud Chat pane visually stayed on screen after reclaiming ("not
      // closing"), even though reclaimChat's own setActivePane(null) call
      // already fires was_hidden(true) CEF-side -- that alone was never
      // enough. This mirrors exactly what the mount/unmount effect
      // further below already had to learn the hard way (its own comment
      // there): was_hidden only pauses CEF's internal repaint; render()
      // decides WHERE to draw purely from PaneLayoutState, and a pane
      // with a stale-but-still-registered rect keeps getting composited
      // there regardless of hidden state. Push one final empty layout --
      // same call that effect's own cleanup already uses for the
      // full-component-unmount case, just reached from this internal,
      // still-mounted transition too.
      setPaneRects({})
      void commands.setPaneLayout([])
      return
    }
    let frame: number | null = null
    const scheduleSync = () => {
      if (frame !== null) return
      frame = requestAnimationFrame(() => {
        frame = null
        syncPaneLayout()
      })
    }
    scheduleSync()
    const observer = new ResizeObserver(scheduleSync)
    observer.observe(body)
    // items.id=257 (2026-08-28): ResizeObserver only fires when the
    // observed element's own border-box SIZE changes -- a window move (or
    // QR's own expand/collapse, which shifts the content pane's position
    // without necessarily changing its size) needs this too.
    window.addEventListener('resize', scheduleSync)
    return () => {
      observer.disconnect()
      window.removeEventListener('resize', scheduleSync)
      if (frame !== null) cancelAnimationFrame(frame)
    }
  }, [syncPaneLayout])

  useEffect(() => {
    fetchActiveProviders()
      .then(setProviders)
      .catch((e: unknown) =>
        setProviderError(e instanceof Error ? e.message : String(e)),
      )
  }, [])

  // items.id=384 slice 4: this component can now unmount for a reason
  // that ISN'T "the user left Cloud Chat entirely" -- WorkspaceShell.tsx
  // unmounts it whenever Board is 'full' (a real unmount, not
  // CSS-hiding, per that file's own doc on why). Pre-slice-4, unmounting
  // always meant "gone for good in practice" (no other way back short of
  // re-selecting the Persona and reopening Cloud Chat), so closing every open
  // pane on unmount was correct. Now, with the pair's own state persisted
  // in NavState.workspace.pair, an unmount here can be purely cosmetic --
  // the user may toggle straight back to 'compact'/'minimized' a moment
  // later, and the whole point of the persistence decision (2026-09-01
  // scoping session) is that the pane should still be there when they do.
  //
  // Fix: HIDE the active pane on unmount instead of closing it, reusing
  // commands.setActivePane's own existing was_hidden side effect
  // (commands/cloud_chat_pane.rs -- the same mechanism reclaimChat already
  // relies on for "Chat reclaims dominance without closing anything").
  // Re-activate it on (re)mount if NavState still says one should be
  // active. NavState.workspace.pair.activeProviderId is deliberately NOT
  // touched by either half of this effect -- hiding/showing is a Rust-side
  // compositing concern only; the persisted "which provider is active"
  // fact doesn't change just because this component happened to remount.
  //
  // BUG FOUND + FIXED (2026-09-01 live verification pass, cargo tauri dev):
  // setActivePane(null) alone was NOT enough -- confirmed live, the pane
  // kept rendering on top of the Board screen after unmount, and Rust's
  // own logs showed its GL render/queue_draw loop firing continuously the
  // whole time. Root cause, confirmed by reading pane_host.rs/render.rs,
  // is NOT a sequencing race (awaiting setActivePane's promise, or
  // delaying the unmount by a tick, would not have fixed this): was_hidden
  // (which set_active_pane triggers) only pauses CEF's own internal paint
  // cadence -- it has nothing to do with WHERE Rust draws the pane's
  // (possibly-cached) texture. That's governed entirely by
  // PaneLayoutState, the fraction-rect map set_pane_layout writes and
  // render() reads every frame -- render.rs's own doc comment: "a pane
  // with no reported layout yet is skipped, not drawn." The ResizeObserver
  // effect below (syncPaneLayout) is what normally keeps that map current,
  // but its cleanup only disconnects the observer -- it never pushed one
  // final empty layout on unmount, so the pane's LAST real on-screen rect
  // (from just before Board flipped to 'full') stayed registered in Rust
  // forever, and render() kept compositing the pane there. Fix: this
  // effect's cleanup now also clears the layout explicitly
  // (commands.setPaneLayout([])), the same call computeActivePaneRect's
  // own empty-object return (paneLayout.ts, activeProviderId === null
  // case) would have produced if syncPaneLayout ever got one more chance
  // to run post-unmount -- it doesn't, so this pushes that final state by
  // hand instead. Fire-and-forget is fine here (unlike a hypothetical fix
  // that tried to await this before allowing the unmount): there's no DOM
  // left for this call to race against, it's a pure "stop drawing this"
  // message to Rust.
  // activeProviderIdRef (not activeProviderId itself) is what the effect
  // below reads, on BOTH the mount and unmount side -- same idiom as
  // openProviderIdsRef further down (and the pre-slice-4 openPaneIdsRef
  // this replaces): a ref, not the reactive value, is what lets `[]`
  // deps correctly express "run once on mount, once on unmount," rather
  // than "run every time activeProviderId changes" (which would call
  // setActivePane on every activate/close/reclaimChat too, redundant with
  // those functions' own calls).
  const activeProviderIdRef = useRef(activeProviderId)
  useEffect(() => {
    activeProviderIdRef.current = activeProviderId
  }, [activeProviderId])

  useEffect(() => {
    if (activeProviderIdRef.current !== null) {
      commands.setActivePane(activeProviderIdRef.current)
    }
    return () => {
      if (activeProviderIdRef.current !== null) {
        void commands.setActivePane(null)
        // See this effect's own header comment: was_hidden alone doesn't
        // stop Rust from drawing the pane at its last-known rect. Clearing
        // the layout map is what actually does.
        void commands.setPaneLayout([])
      }
    }
  }, [])

  // items.id=330 (pre-slice-4) / items.id=384 slice 4: `PaneManager`
  // (Rust) has no way to know this component lost track of a pane. The
  // only real "gone for good" case left after slice 4 is a genuine app
  // exit/reload (beforeunload) -- an ordinary component unmount within a
  // running session no longer means that (see the effect above). This
  // effect's own cleanup used to ALSO call closeAllOpenPanes()
  // unconditionally on every unmount; that call is removed here --
  // closing every pane just because Board toggled to 'full' would have
  // silently destroyed exactly the state the persistence decision this
  // slice implements is meant to keep.
  const openProviderIdsRef = useRef<string[]>([])
  useEffect(() => {
    openProviderIdsRef.current = openProviderIds
  }, [openProviderIds])

  useEffect(() => {
    const closeAllOpenPanes = () => {
      for (const id of openProviderIdsRef.current) {
        void commands.closeCloudChatPane(id)
      }
    }
    window.addEventListener('beforeunload', closeAllOpenPanes)
    return () => {
      window.removeEventListener('beforeunload', closeAllOpenPanes)
    }
  }, [])

  const handleClose = useCallback(
    (providerId: string) => {
      close(providerId, () => {
        // items.id=234: close_pane's own popup teardown is synchronous on
        // the Rust side, not queued through the same per-tick drain that
        // fires cloud-chat-popup-closed for the other close paths (self-close,
        // navigate-away) -- see PopupClosedPayload's own doc. Clear
        // proactively here rather than waiting for an event that never
        // comes for this specific path.
        setPopupRects((rects) => {
          if (!(providerId in rects)) return rects
          const next = { ...rects }
          delete next[providerId]
          return next
        })
      })
    },
    [close],
  )

  // items.id=233: fires once ChatPane has a real drafted message ready for
  // outbound Privacy Guardian review. On pending_consent, the
  // consent_request listener below independently picks up the payload
  // gate3() has already emitted by the time this promise resolves
  // (write-before-surface invariant, conductor/privacy/gate3.rs) -- this
  // handler only needs to react to the synchronous terminal outcomes
  // (approved/blocked/timeout) and the not-found/error path.
  const handleDraftReady = useCallback(
    (messageId: string) => {
      if (!personaId) return
      setReviewOutcome('pending')
      setReviewMessage(null)
      setReviewCeiling(null)
      setPendingMessageId(messageId)
      commands
        .requestCloudFrontierGate3Review({
          user_id: requireCurrentUserId(),
          persona_id: personaId,
          message_id: messageId,
        })
        .then((result) => {
          if (result.status !== 'ok') {
            setReviewOutcome('blocked')
            setReviewMessage(
              t('navShell.cloudChatAccessPane.gate3ReviewError', { message: result.error }),
            )
            return
          }
          const data = result.data
          if (data.pending_consent) {
            // Payload arrives via the consent_request listener.
            return
          }
          if (data.approved) {
            setReviewOutcome('approved')
            return
          }
          // blocked or timeout -- gate3_review_status stays 'drafted'
          // server-side (see request_cloud_frontier_gate3_review's own doc comment);
          // surface the plain_language message, no modal.
          setReviewOutcome('blocked')
          setReviewMessage(data.plain_language)
          if (data.target_tier != null && data.space_max_permitted_tier != null) {
            setReviewCeiling({
              targetTier: data.target_tier,
              current: data.space_max_permitted_tier,
            })
          }
        })
    },
    [personaId, t],
  )

  /** items.id=391: an on-demand real Gate3 review of the current
   *  transcript's most recent response, without composing a new message.
   *  Originally the mockup's "2nd opinion" chat-toolbar button; as of the
   *  tenth pass ("three bars, one expanded" redesign) it's invoked from
   *  CloudChatCollapsedStrip's always-visible bar instead (its 'reviewable'
   *  empty state, WorkspaceShell.tsx/CloudChatCollapsedStrip.tsx) -- same
   *  handler, new caller, the toolbar button itself is removed as
   *  redundant with that bar.
   *
   *  BUG FOUND + FIXED (2026-09-02 live verification pass): the first
   *  version of this handler called handleDraftReady unconditionally,
   *  which just resends requestCloudFrontierGate3Review -- confirmed live this
   *  hard-errors ("... is not awaiting gate3 review") the moment the last
   *  message's status is already terminal, exactly the common case this
   *  button exists for (a message approved before an earlier trip to
   *  Board). Branches on the message's own gate3_review_status instead
   *  (VALID_GATE3_REVIEW_STATUS, message_store.rs): 'approved' just needs
   *  Cloud Chat dominant again, no new review request; 'withheld' was an
   *  explicit privacy choice, not something to silently retry -- surfaces
   *  the same "kept private" banner a real withheld outcome shows;
   *  anything else (drafted, or a client-side "blocked" outcome, which
   *  message_store.rs's own doc confirms leaves gate3_review_status at
   *  'drafted' server-side) is a genuine not-yet-resolved case, so only
   *  THAT path calls handleDraftReady -- the real first-review flow,
   *  unchanged. */
  const handleSecondOpinion = useCallback(() => {
    if (!lastAssistantMessage) return
    switch (lastAssistantMessage.gate3_review_status) {
      case 'approved':
        markCloudChatReadyAndPromote()
        return
      case 'withheld':
        setReviewOutcome('withheld')
        return
      default:
        handleDraftReady(lastAssistantMessage.id)
    }
  }, [lastAssistantMessage, handleDraftReady, markCloudChatReadyAndPromote])

  // Same cancelled/unlisten cleanup idiom as ChatPane's own first listen()
  // effect (run-status-update), per CLAUDE.md's "Tauri event listeners must
  // be explicitly detached on SPA view unmount."
  useEffect(() => {
    let unlisten: UnlistenFn | undefined
    let cancelled = false

    listen<ConsentRequestPayload>('consent_request', (event) => {
      setConsentPayload(event.payload)
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

  // items.id=234: same cancelled/unlisten idiom as the consent_request
  // listener above. cloud-chat-popup-opened/-closed are the two popup-close
  // paths the frontend has no other way to learn about (self-close,
  // parent navigate-away) -- see PopupClosedPayload's own doc for why the
  // parent-pane-close path (handleClose above) does not rely on this.
  useEffect(() => {
    let unlistenOpened: UnlistenFn | undefined
    let unlistenClosed: UnlistenFn | undefined
    let cancelled = false

    listen<PopupOpenedPayload>('cloud-chat-popup-opened', (event) => {
      const { provider_id, rect } = event.payload
      setPopupRects((rects) => ({ ...rects, [provider_id]: rect }))
    }).then((fn) => {
      if (cancelled) {
        fn()
      } else {
        unlistenOpened = fn
      }
    })

    listen<PopupClosedPayload>('cloud-chat-popup-closed', (event) => {
      const { provider_id } = event.payload
      setPopupRects((rects) => {
        if (!(provider_id in rects)) return rects
        const next = { ...rects }
        delete next[provider_id]
        return next
      })
    }).then((fn) => {
      if (cancelled) {
        fn()
      } else {
        unlistenClosed = fn
      }
    })

    return () => {
      cancelled = true
      unlistenOpened?.()
      unlistenClosed?.()
    }
  }, [])

  const handleModalResolve = (decisions: ElementDecision[]) => {
    if (!consentPayload || !personaId || !pendingMessageId) return
    const status = isAllKeptPrivate(decisions) ? 'withheld' : 'approved'

    commands
      .submitElementConsentDecision({
        run_id: consentPayload.focus_run_id,
        user_id: requireCurrentUserId(),
        persona_id: personaId,
        decisions_json: JSON.stringify(decisions),
      })
      .then(() =>
        commands.resolveCloudFrontierGate3Review({
          user_id: requireCurrentUserId(),
          persona_id: personaId,
          message_id: pendingMessageId,
          status,
        }),
      )
      .finally(() => {
        setConsentPayload(null)
        setPendingMessageId(null)
        setReviewOutcome(status)
      })
  }

  // items.id=391 (Jason, 2026-09-02): the dev-only force-escalation
  // scaffolding that used to live here (DIAG_329/items.id=329,
  // DIAG_356/items.id=356 -- devSeedTier3DraftMessage +
  // devBypassTier3Gate3Review) is REMOVED, not just hidden -- the chat
  // toolbar's real "2nd opinion" button (below, handleSecondOpinion) now
  // covers the same fast-iteration need through the real
  // requestCloudFrontierGate3Review path, on the real last message, no synthetic
  // seed or gate3() bypass required. The Rust-side dev-only commands
  // themselves are untouched (out of scope here; a separate cleanup if
  // nothing else ever calls them).

  // items.id=540: Cancel used to only reset local state, leaving the
  // message stuck at gate3_review_status='pending-review' server-side
  // forever -- any retry (handleSecondOpinion's default arm, or resending)
  // then hard-erred because requestCloudFrontierGate3Review only accepts
  // 'drafted'. Reverts the message via cancelCloudFrontierGate3Review
  // first, same fire-and-forget-on-cleanup shape as handleModalResolve:
  // the modal closes and local state clears regardless of the call's
  // outcome (a failed revert just leaves the message stuck, the same
  // failure mode as before this fix, not a new one).
  const handleModalCancel = () => {
    if (personaId && pendingMessageId) {
      void commands.cancelCloudFrontierGate3Review({
        user_id: requireCurrentUserId(),
        persona_id: personaId,
        message_id: pendingMessageId,
      })
    }
    setConsentPayload(null)
    setPendingMessageId(null)
    setReviewOutcome(null)
  }

  // items.id=384 slice 7: shared by both switch paths below -- clears any
  // Gate3 review state left over from whichever chat was showing before.
  // A 'pending'/'blocked'/'withheld' banner (or a still-open consent
  // modal) referencing a message that's no longer even on screen would be
  // actively misleading once the transcript underneath it has changed.
  const resetGate3State = () => {
    setReviewOutcome(null)
    setReviewMessage(null)
    setReviewCeiling(null)
    setConsentPayload(null)
    setPendingMessageId(null)
  }

  /** decisions.id=740, superseded by decisions.id=823 (items.id=543):
   *  create a fresh chat for `persona` and make it the one showing,
   *  switching activePersonaId too. Originally NewChatPersonaPicker's one
   *  action (a standalone dot-picker, now retired); this is now
   *  PersonaBox's onChange, both the expanded and floor-mode instances
   *  below. Still the ONLY way to start a new chat in this build (no
   *  separate "New chat" button) -- but only for an ACTUAL persona
   *  switch. Confirmed live (Jason, click-through): reselecting the
   *  already-active persona in PersonaBox's popover used to still call
   *  commands.createChat, silently replacing whatever conversation was
   *  open even though nothing changed -- corrected below to a no-op in
   *  that case (the popover still closes regardless; that's PersonaBox's
   *  own onClick, not gated here). */
  const handleStartNewChat = useCallback(
    (persona: PersonaInfo) => {
      if (persona.id === personaId) return
      commands.createChat(requireCurrentUserId(), persona.id).then((result) => {
        if (result.status !== 'ok') {
          setOpenError(result.error)
          return
        }
        resetGate3State()
        setActiveChat(result.data)
        onPersonaChange(persona.id)
      })
    },
    [personaId, onPersonaChange, setOpenError],
  )

  /** ChatHistoryList's own action -- switch to viewing a past chat.
   *  Persona-scoped already (list_chats is called with the current
   *  personaId), so unlike handleStartNewChat this never touches
   *  activePersonaId. */
  const handleSelectChat = useCallback((chat: ChatInfo) => {
    resetGate3State()
    setActiveChat(chat)
  }, [])

  // items.id=404: History's "resume this chat" row-action lands here --
  // WorkspaceShell sets pendingChatSelection and dominantRail='chat' in
  // the same handler tick (and onActivePersonaIdChange first, if needed),
  // so by the time this effect runs, `personaId` above already matches
  // the chat's own owning Persona. Reuses handleSelectChat's own logic
  // rather than duplicating it.
  useEffect(() => {
    if (!pendingChatSelection) return
    handleSelectChat(pendingChatSelection)
    onPendingChatSelectionConsumed?.()
  }, [pendingChatSelection, handleSelectChat, onPendingChatSelectionConsumed])

  const activeProvider = providers.find((p) => p.id === activeProviderId) ?? null

  // items.id=368: switching the active pane (setActivePane, Rust) hides the
  // OUTGOING pane's own popup CEF-side (was_hidden(true)) but does not close
  // it -- it stays alive/tracked (PaneManager::popups, "one active popup per
  // pane", not cleared until a real cloud-chat-popup-closed event) so it can be
  // reactivated without reloading. This app never emits that close event
  // just because a pane got deactivated, so popupRects (state, keyed by
  // parent pane id) still carried that now-hidden popup's rect -- and its
  // PopupHitLayer div, still pointer-events:auto at its last-known screen
  // position, kept absorbing clicks/keys meant for whichever pane is
  // actually active now, making that pane look frozen (confirmed live,
  // Jason, 2026-08-30: a popup left open on one pane while switching to a
  // different one silently ate input meant for the new pane). Scoping to
  // only the active pane's own popup entry is the fix -- no Rust change
  // needed, this was never a backend focus/activation problem.
  const activePopupRects =
    activeProviderId !== null && activeProviderId in popupRects
      ? { [activeProviderId]: popupRects[activeProviderId] }
      : {}

  // items.id=384 slice 7: ChatPane's own `contextKey` prop already covers
  // this -- no chatId prop was added to ChatPane itself (its own doc
  // comment already calls contextKey "the caller-owned transcript
  // identity," which is exactly what this is). activeChat === null is
  // the pre-existing flat conversation's context_key, unchanged from
  // before this slice; a real chat's own context_key (already
  // "chat-{uuid}", assigned server-side by chat_store::create_chat) is
  // used verbatim, not reconstructed client-side.
  // items.id=534 (Option B): this literal is a persisted context_key
  // format (messages_001.sql/messages_002.sql), intentionally kept
  // unchanged by the tier-vocabulary rename -- do not "fix" it to
  // cloud-chat-access-{personaId}, that would orphan every existing
  // user's default-view chat history.
  const chatContextKey = activeChat ? activeChat.context_key : `tier3-access-${personaId}`

  // items.id=391 (tenth pass): drives CloudChatCollapsedStrip's own
  // `emptyState` prop, consulted only when openProviderIds is empty --
  // see that component's header comment for what each value means. This
  // used to gate a separate chat-toolbar "2nd opinion" button (removed --
  // the always-visible bar below now covers the same action).
  const secondOpinionEmptyState: 'approved' | 'reviewable' | 'none' =
    lastAssistantMessage?.gate3_review_status === 'approved'
      ? 'approved'
      : lastAssistantMessage
        ? 'reviewable'
        : 'none'

  return (
    <div
      className="cloud-chat-access-pane"
      data-dominant={dominant}
      data-floor={floor ? '' : undefined}
    >
      <PaneHitLayer rects={paneRects} />
      {/* items.id=234: mounted AFTER PaneHitLayer -- DOM source order alone
          resolves popup-vs-parent-pane hit-test precedence in any
          overlapping region (see PopupHitLayer's own doc). */}
      <PopupHitLayer popups={activePopupRects} />

      <div
        className="cloud-chat-access-pane__qr"
        data-collapsed={floor || dominant === 'cloudChat' ? '' : undefined}
        data-floor={floor ? '' : undefined}
      >
        {/* items.id=391 (tenth pass -- "three bars, one expanded"
            redesign): the mockup's chat-toolbar (History, persona list)
            no longer lives in its own bordered side column -- confirmed
            live (Jason): a separate boxed toolbar beside the chat panel
            read as "three slightly different screens" bolted together,
            not one consistent design. Folded into THIS bar instead --
            same row that used to carry only the "Quiet Rabbit -- this
            conversation" title, restyled (NavShell.css) to match Board's
            and Cloud Chat's own bars (WorkspaceShell.tsx / CloudChatCollapsedStrip)
            so all three read as the same kind of row. Un-gated from
            personaId, same as the toolbar it replaces -- the persona
            picker inside it is how a user with no Persona yet picks
            their first one, so the bar itself (with a fallback label)
            must render even then, not just once personaId is set.
            ChatHistoryList is the one piece that still needs a real
            personaId (list_chats is Persona-scoped) -- guarded inline.
            Only while QR is fully expanded (dominant === 'chat') AND not
            floored: QR's collapsed floor (dominant === 'cloudChat', or floor
            === true) has its own compact mark via ChatPane's collapsed
            strip already, which has no room for this bar beside it.
            Lives as a SIBLING of .qr-panel (below), never an ancestor of
            ChatPane -- ChatPane's own direct parent must stay constant
            across the dominant toggle so React never remounts it (see
            this pane's own header comment on why: ChatPane's collapsed
            prop is what's supposed to preserve message state, not a
            fresh mount). */}
        {!floor && dominant === 'chat' && (
          <div className="cloud-chat-access-pane__section-header">
            <span className="cloud-chat-access-pane__section-header-name">
              {personaId
                ? t('navShell.cloudChatAccessPane.qrBannerName')
                : t('navShell.content.cloudChatUnavailable')}
            </span>
            <div className="cloud-chat-access-pane__section-header-controls">
              {personaId && (
                <ChatHistoryList
                  onOpenHistory={() => onOpenHistory({ personaId })}
                  disabled={reviewOutcome === 'pending'}
                />
              )}
              <PersonaBox
                personas={personas}
                activePersonaId={personaId}
                onChange={handleStartNewChat}
                open={personaBoxOpen}
                onOpenChange={setPersonaBoxOpen}
                disabled={reviewOutcome === 'pending'}
              />
            </div>
          </div>
        )}
        {floor && (
          // items.id=391 (Jason, 2026-09-02): "compressed" doesn't mean
          // "just a snippet" -- confirmed live, the floor needs the same
          // persona picker the compact/minimized toolbar has, the
          // compact pills treatment (no room for anything wider in a
          // one-row floor) so switching/starting a chat as a different
          // Persona doesn't require expanding Chat first. Sits beside
          // .qr-panel -- .qr switches to row direction specifically
          // while floor is true (NavShell.css's own [data-floor] rule),
          // unlike the banner case above, which needs .qr in its default
          // column direction (bar on top, .qr-panel's real content
          // below).
          <div className="cloud-chat-access-pane__floor-picker">
            <PersonaBox
              personas={personas}
              activePersonaId={personaId}
              onChange={handleStartNewChat}
              open={personaBoxOpen}
              onOpenChange={setPersonaBoxOpen}
              disabled={reviewOutcome === 'pending'}
            />
          </div>
        )}
        <div className="cloud-chat-access-pane__qr-panel">
          {personaId ? (
            // decisions.id=743: QR's own identity header now lives above
            // this element (.section-header, this file's own comment on
            // that class) rather than nested inside this branch -- it needs to
            // render even when personaId is null (the persona-picker's
            // "pick your first Persona" case), which this branch by
            // definition never is.
            <ChatPane
              contextKey={chatContextKey}
              userId={requireCurrentUserId()}
              personaId={personaId}
              focusId="quick-ask"
              gate3Track={true}
              onDraftReady={handleDraftReady}
              collapsed={floor || dominant === 'cloudChat'}
              onExpand={floor ? onFloorExpand : reclaimChatAndPromote}
              onLastAssistantMessageChange={setLastAssistantMessage}
              onCopyStarter={(messageId, content, starterPersonaId) =>
                setLastCopiedStarter({ messageId, content, personaId: starterPersonaId })
              }
            />
          ) : floor ? (
            // items.id=391 (Jason, 2026-09-02, second pass): "compressed"
            // doesn't mean "just a snippet with nothing else" -- reuses
            // ChatPane's own collapsed-strip AND input-row classes
            // (ChatPane.css) together, matching what a real collapsed
            // ChatPane renders below its own strip, so this reads as the
            // same kind of floor, just with nothing to click through to
            // yet. The persona picker lives beside this in the row-level
            // .floor-picker sibling above, not duplicated here.
            //
            // items.id=391 (Jason, 2026-09-02, ninth pass): the input/send
            // used to be plain `disabled` -- confirmed live, that read as
            // genuinely inert instead of as another way to reach the same
            // "pick a Persona" action the collapsed strip and floor-picker
            // already offer. readOnly instead of disabled: still not
            // actually typeable (nothing here is wired to a draft, so
            // picking a Persona -- which drops this component into the
            // real ChatPane branch above -- has nothing that needs to
            // carry over), but focusable/clickable, so clicking or
            // tabbing into the input (or clicking Send) triggers
            // onFloorExpand exactly like clicking the strip does --
            // matching ChatPane's own real collapsed-input onFocus
            // behavior, not a separate mechanism.
            <div className="chat-pane" data-collapsed="">
              <button
                type="button"
                className="chat-pane__collapsed-strip"
                onClick={onFloorExpand}
              >
                <span className="chat-pane__collapsed-mark" aria-hidden="true" />
                {/* items.id=391 (eleventh pass): matches ChatPane.tsx's own
                    real collapsed-strip -- see its comment on why this
                    name is needed at all ("no qr chat bar"). */}
                <span className="chat-pane__collapsed-name">
                  {t('navShell.cloudChatAccessPane.qrBannerName')}
                </span>
                <span className="chat-pane__collapsed-snippet">
                  {t('navShell.content.cloudChatUnavailable')}
                </span>
                <span className="chat-pane__collapsed-expand" aria-hidden="true">
                  {t('navShell.cloudChatCollapsedStrip.expandLabel')}
                </span>
              </button>
              <div className="chat-pane__input-row">
                <label
                  className="chat-pane__input-label"
                  htmlFor="cloud-chat-access-pane-floor-empty-input"
                >
                  {t('navShell.chat.inputLabel')}
                </label>
                <input
                  id="cloud-chat-access-pane-floor-empty-input"
                  type="text"
                  className="chat-pane__input"
                  placeholder={t('navShell.chat.inputPlaceholder')}
                  readOnly
                  onFocus={() => onFloorExpand?.()}
                  onClick={() => onFloorExpand?.()}
                />
                <button type="button" onClick={() => onFloorExpand?.()}>
                  {t('navShell.chat.sendButton')}
                </button>
              </div>
            </div>
          ) : (
            // items.id=391 (Jason, 2026-09-02 live-verification pass): a
            // bare banner here still read as an error, not an invitation
            // to act -- reusing ChatPane's own message-bubble/input-row
            // classes (ChatPane.css, plain global classnames, no CSS
            // Modules -- safe to reuse directly) makes this render as a
            // real chat response ("Select a Persona to begin chatting")
            // instead, with the SAME input row shape a real chat has,
            // just disabled -- there is nowhere to send a message to yet.
            // No picker down here any more (a second copy of it, right
            // above -- see the section-header's own comment on why it's
            // un-gated from personaId now): one picker, not two. Picking a
            // Persona there calls handleStartNewChat (via PersonaBox's
            // onChange), which creates a real chat and switches personaId,
            // dropping this component into the normal ChatPane branch
            // above on the next render -- the disabled input here is
            // never wired to a draft, so nothing needs to carry over.
            //
            // items.id=543 (Section 2.1): clicking this empty rail body
            // itself, not just the PersonaBox above, also opens the same
            // forced selector popover -- the "no Persona active" case has
            // no real content to interact with otherwise. A plain div, not
            // a <button>, because it nests real interactive children (the
            // disabled input/send button below) -- same role="button" +
            // tabIndex + matching onKeyDown convention as PrivacyGuardianModal.tsx's
            // PgCell, for the same reason.
            <div
              className="chat-pane"
              data-empty-persona-body=""
              role="button"
              tabIndex={0}
              aria-label={t('navShell.personaBox.popoverTitleChoose')}
              onClick={() => setPersonaBoxOpen(true)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' || e.key === ' ') {
                  e.preventDefault()
                  setPersonaBoxOpen(true)
                }
              }}
            >
              <div className="chat-pane__transcript">
                <ul className="chat-pane__message-list">
                  <li className="chat-pane__message chat-pane__message--assistant">
                    <span className="chat-pane__message-content">
                      {t('navShell.content.cloudChatUnavailable')}
                    </span>
                  </li>
                </ul>
              </div>
              <div className="chat-pane__input-row">
                <label
                  className="chat-pane__input-label"
                  htmlFor="cloud-chat-access-pane-empty-input"
                >
                  {t('navShell.chat.inputLabel')}
                </label>
                <input
                  id="cloud-chat-access-pane-empty-input"
                  type="text"
                  className="chat-pane__input"
                  placeholder={t('navShell.chat.inputPlaceholder')}
                  disabled
                />
                <button type="button" disabled>
                  {t('navShell.chat.sendButton')}
                </button>
              </div>
            </div>
          )}
        </div>
      </div>

      {/* items.id=391 (tenth pass): this is the SECOND of the three peer
          bars/regions (Board's own bar/region lives in WorkspaceShell.tsx,
          above this component; QR's row is above, within .qr). Exactly
          one of the three is ever the fully-expanded region -- Cloud Chat gets
          the rail+content-pane split ONLY while it's the expanded one
          (!floor && dominant === 'cloudChat'); every other combination
          (floor, or dominant === 'chat') renders CloudChatCollapsedStrip
          instead, unconditionally -- Jason's own framing for this pass:
          "a second opinion bar always visible." No redundant "back to
          Board" button here any more either -- WorkspaceShell's own
          board-bar already covers that whenever Board isn't expanded,
          which is exactly whenever Cloud Chat CAN be the expanded region. */}
      {!floor && dominant === 'cloudChat' ? (
        <>
          {/* items.id=391 (eleventh pass): promoted out of the rail
              column's own <h3> -- confirmed live (Jason): "the QR chat
              and second opinion need consistent headers... expanded
              partially or fully." A heading buried inside the narrow
              220px rail column couldn't visually match QR's own
              full-width header bar no matter how it was styled; this bar
              is the SAME .section-header class QR's own header uses
              (NavShell.css), just title-only -- no controls, unlike QR's
              (History/persona picker have no Cloud Chat equivalent). */}
          <div className="cloud-chat-access-pane__section-header">
            <span className="cloud-chat-access-pane__section-header-name">
              {t('navShell.cloudChatAccessPane.heading')}
            </span>
          </div>
          <div className="cloud-chat-access-pane__split-area">
          <div className="cloud-chat-access-pane__rail-col">
            {providerError && (
              <p role="alert">
                {t('navShell.cloudChatAccessPane.providerError', {
                  message: providerError,
                })}
              </p>
            )}
            {providers.length === 0 && !providerError && (
              <p>{t('navShell.cloudChatAccessPane.loadingProviders')}</p>
            )}
            {providers.length > 0 && dominant === 'cloudChat' && (
              // items.id=359: the rail is persistent once the gate clears --
              // unlike the retired selector screen, it does not disappear
              // once a pane opens (TIER3_ACCESS_MODEL.md States section 3).
              // items.id=384 slice 4 bug fix: this used to gate on
              // reviewOutcome === 'approved', which is local component
              // state that resets to null on every remount (WorkspaceShell
              // unmounts this component whenever Board goes 'full'). That
              // made the rail's own provider list vanish on return even
              // though the pair itself (dominant/activeProviderId/
              // openProviderIds, all in NavState.workspace.pair) correctly
              // persisted -- confirmed live, 2026-09-01 verification pass.
              // dominant is the right signal: this whole block is already
              // inside the dominant === 'cloudChat' branch (see below), so this
              // check is really just making explicit what's already true --
              // dominant only ever becomes 'cloudChat' downstream of Gate3
              // having cleared at least once (markCloudChatReady/activate), the
              // same fact reviewOutcome === 'approved' used to capture, but
              // via a field that actually survives the remount reviewOutcome
              // doesn't.
              <CloudChatSelector
                providers={providers}
                openPaneIds={openProviderIds}
                activeProviderId={activeProviderId}
                onActivate={activateAndPromote}
                onClose={handleClose}
              />
            )}
          </div>

          <div className="cloud-chat-access-pane__content-pane">
            {activeProviderId !== null && (
              // items.id=391 (Jason, 2026-09-02): the mockup's own
              // content-head/content-body both carry "click to collapse
              // Tier 3, bring QR chat forward" -- reclaimChat, the SAME
              // action the QR floor's own collapsed strip already
              // triggers, just from a second, more discoverable spot.
              // Only the head (not content-body) gets this here: the
              // active pane's own CEF texture composites directly into
              // .content-body's bounding rect (that element's own doc
              // comment), so a click handler there would fight the real
              // page's own interactivity -- every click meant for the
              // external provider's page would also collapse Cloud Chat.
              // The head is plain DOM chrome, nothing composited over it,
              // so it's a safe, always-available "back to chat" target.
              // This does NOT touch boardSize -- reclaimChat only ever
              // sets dominant back to 'chat'; Board stays exactly however
              // it was (minimized bar if that's where this session
              // started), never jumping to full/dominant the way the
              // rail's separate "Active Board" button deliberately does.
              <div
                className="cloud-chat-access-pane__content-head"
                onClick={reclaimChatAndPromote}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault()
                    reclaimChatAndPromote()
                  }
                }}
                role="button"
                tabIndex={0}
                title={t('navShell.cloudChatAccessPane.contentHeadReclaimTitle')}
              >
                <span className="cloud-chat-access-pane__content-head-name">
                  {activeProvider?.name ?? activeProviderId}
                </span>
                <button
                  type="button"
                  className="cloud-chat-access-pane__content-head-close"
                  title={t('navShell.cloudChatAccessPane.contentCloseButton')}
                  onClick={(e) => {
                    e.stopPropagation()
                    handleClose(activeProviderId)
                  }}
                >
                  &times;
                </button>
              </div>
            )}
            <div ref={contentBodyRef} className="cloud-chat-access-pane__content-body">
              {activeProviderId === null && (
                <p className="cloud-chat-access-pane__content-empty">
                  {t('navShell.cloudChatAccessPane.contentEmptyPrompt')}
                </p>
              )}
            </div>
          </div>
          </div>
        </>
      ) : (
        <CloudChatCollapsedStrip
          providers={providers}
          openProviderIds={openProviderIds}
          activeProviderId={activeProviderId}
          onExpand={activateAndPromote}
          emptyState={secondOpinionEmptyState}
          onExpandRail={markCloudChatReadyAndPromote}
          onReview={handleSecondOpinion}
          reviewDisabled={reviewOutcome === 'pending'}
        />
      )}

      {/* Gate3 review surfaces -- deliberately OUTSIDE the dominant
          branches above, see this file's own header comment on why. */}
      {reviewOutcome === 'blocked' && (
        <p role="alert">{reviewMessage ?? t('navShell.cloudChatAccessPane.gate3BlockedFallback')}</p>
      )}
      {reviewOutcome === 'blocked' && reviewCeiling && personaId && (
        <FocusSettingsControls
          userId={requireCurrentUserId()}
          personaId={personaId}
          focusId="quick-ask"
          mode="ceilingOnly"
          suggestedMaxPermittedTier={externalAccessFromLegacyTier(reviewCeiling.targetTier)}
          onSaved={() => {
            setReviewCeiling(null)
            if (pendingMessageId) handleDraftReady(pendingMessageId)
          }}
        />
      )}
      {reviewOutcome === 'withheld' && <p>{t('navShell.cloudChatAccessPane.gate3Withheld')}</p>}
      <PrivacyGuardianModal
        open={reviewOutcome === 'pending'}
        payload={consentPayload}
        onResolve={handleModalResolve}
        onCancel={handleModalCancel}
      />
      {openError && (
        <p role="alert">
          {t('navShell.cloudChatAccessPane.openError', { message: openError })}
        </p>
      )}
    </div>
  )
}
