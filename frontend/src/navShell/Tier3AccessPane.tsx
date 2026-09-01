// Tier 2/Tier 3 access -- rail + content-pane hosting (items.id=359,
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
// symmetric. When Tier 3 is dominant, the layout is exactly what it
// always was (QR collapsed beside the rail+content-pane). When Chat is
// dominant, QR renders full-size and the ENTIRE rail+content-pane is
// replaced by Tier3CollapsedStrip -- a small click-to-expand row, no
// live entry field (that's the resolved answer to decisions.id=738's
// flagged question: only QR's own collapsed floor gets a live entry
// field, since only QR has a QR-owned conversation to keep live).
// Gate3 review UI (PrivacyGuardianModal and the blocked/withheld/
// ceiling-raise messaging) is rendered OUTSIDE this dominant-conditional
// -- deliberately: a draft can enter Gate3 review from a full-screen
// Chat send regardless of whether Tier 3 currently has any pane loaded,
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
import { commands, type ChatInfo, type PaneRectFraction, type PersonaInfo } from '../bindings'
import { ChatPane } from '../chat/ChatPane'
import { ChatHistoryList } from '../chat/ChatHistoryList'
import { NewChatPersonaPicker } from '../chat/NewChatPersonaPicker'
import { FocusSettingsControls } from './FocusSettingsControls'
import { requireCurrentUserId, type DominancePairState } from './navShellConfig'
import { Tier3CollapsedStrip } from './Tier3CollapsedStrip'
import { useDominancePair } from './useDominancePair'
import { computeActivePaneRect, pixelRectToFraction, type PanePixelRect } from '../tier3Access/paneLayout'
import { PaneHitLayer } from '../tier3Access/PaneHitLayer'
import { PopupHitLayer } from '../tier3Access/PopupHitLayer'
import {
  PrivacyGuardianModal,
  type ConsentRequestPayload,
  type ElementDecision,
} from '../tier3Access/PrivacyGuardianModal'
import { isAllKeptPrivate } from '../tier3Access/consentDecisions'
import { Tier3Selector } from '../tier3Access/Tier3Selector'
import {
  fetchActiveProviders,
  type Provider,
} from '../tier3Access/tier3AccessConfig'

type ReviewOutcome = 'pending' | 'approved' | 'withheld' | 'blocked'

// items.id=234 -- host-owned popup subsystem. Hand-declared, not generated:
// event payloads, not command args, same convention as
// PrivacyGuardianModal.tsx's own ConsentRequestPayload -- see
// commands/tier3_pane.rs's PopupOpenedPayload/PopupClosedPayload for the
// Rust side these mirror.
interface PopupOpenedPayload {
  provider_id: string
  rect: PaneRectFraction
}
interface PopupClosedPayload {
  provider_id: string
}

export interface Tier3AccessPaneProps {
  /** The currently-active Persona (navShellConfig.ts's NavState.activePersonaId,
   *  a standing field independent of which top-level screen is showing --
   *  items.id=384 slice 1 removed the earlier capture-on-transition
   *  workaround this comment used to describe). Routinely null now --
   *  items.id=384 slice 3 removed the old gate requiring a Persona be
   *  selected before this screen is even reachable (WorkspaceShell.tsx
   *  mounts this component regardless), so null here means "no Persona
   *  chosen yet," a real, expected state, not just a brief render-order
   *  gap -- see the tier3ChatUnavailable branch below. */
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
}

export function Tier3AccessPane({
  personaId,
  onPersonaChange,
  personas,
  pair,
  onUpdatePair,
}: Tier3AccessPaneProps) {
  const { t } = useTranslation()
  const [providers, setProviders] = useState<Provider[]>([])
  const [providerError, setProviderError] = useState<string | null>(null)
  // items.id=384 slice 7 (decisions.id=739): null means "the pre-existing
  // flat tier3-access-{personaId} conversation" -- the default view, not
  // a loading state. Set to a real ChatInfo by either starting a new chat
  // (NewChatPersonaPicker) or picking a past one (ChatHistoryList).
  // Deliberately NOT persisted to NavState -- unlike `pair`, losing this
  // selection on a Board-toggle remount just means the default view
  // reappears, not any data loss (nothing here is hidden/torn down the
  // way a live CEF pane would be).
  const [activeChat, setActiveChat] = useState<ChatInfo | null>(null)

  // A chat belongs to exactly one Persona (decisions.id=741) -- if
  // personaId changes out from under this component (the Persona hub's
  // own Persona buttons still work independently of this pane, per
  // navShellConfig.ts's setActivePersonaId doc comment), whatever chat
  // was showing belongs to the OLD Persona and must not keep showing
  // under the new one. Falls back to the new Persona's own default view,
  // same as a fresh mount would.
  useEffect(() => {
    setActiveChat(null)
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
    markTier3Ready,
  } = useDominancePair(pair, onUpdatePair)

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
    current: number
  } | null>(null)
  const [consentPayload, setConsentPayload] = useState<ConsentRequestPayload | null>(null)
  // ConsentRequestPayload carries focus_run_id, not message_id -- gate3()
  // only knows about content_key/step_id/focus_run_id, never the
  // messages.db row that triggered it. Stashed here from handleDraftReady
  // so handleModalResolve has the right id to pass to resolveTier3Gate3Review.
  const [pendingMessageId, setPendingMessageId] = useState<string | null>(null)

  // items.id=384 slice 4: the bridge between Gate3 review and dominance.
  // Pre-merge, the rail was simply always visible once reviewOutcome
  // reached 'approved' -- there was no separate "dominant" concept to
  // update. Post-merge, Tier3 must actually BECOME dominant at that same
  // moment (see useDominancePair.ts's own header comment for why
  // markTier3Ready is a distinct trigger from activate/reclaimChat, and
  // why dominant is no longer purely derived from activeProviderId).
  // Fires from all three paths that can set reviewOutcome to 'approved'
  // (handleDraftReady, handleModalResolve, handleDevForceTier3) via this
  // one effect rather than duplicating the call at each of those three
  // sites.
  //
  // BUG FOUND + FIXED (2026-09-01 live verification pass, items.id=384
  // slice 7): the original version of this effect fired
  // `if (reviewOutcome === 'approved')` unconditionally on every render,
  // not just on the actual transition INTO 'approved' -- a LEVEL trigger
  // where an EDGE trigger was needed. markTier3Ready's own identity is
  // unstable across renders (it closes over `setPair`, which is
  // NavShell.tsx's `onUpdatePair` prop, itself a fresh closure on every
  // NavShell render, never memoized) -- so this effect's dependency array
  // never actually settles, and it re-ran on essentially every render
  // while reviewOutcome stayed 'approved'. In practice this meant
  // reclaimChat's own dominant:'chat' update got silently overwritten
  // back to 'tier3' on the very next render, PERMANENTLY breaking the
  // "click QR's collapsed floor to reclaim it" gesture for the rest of
  // the session, the instant any draft had ever been approved once.
  // Confirmed live: clicking the collapsed strip, its "Expand" label, and
  // focusing its entry field all correctly triggered reclaimChat, and all
  // three were silently reverted a moment later.
  //
  // Fix: track the PREVIOUS reviewOutcome in a ref and only call
  // markTier3Ready on an actual null/blocked/withheld -> 'approved'
  // transition, matching what this effect was always meant to express
  // ("Gate3 just cleared") rather than what it accidentally implemented
  // ("Gate3 has cleared at some point and something else re-rendered").
  // This makes the effect correct regardless of markTier3Ready's own
  // identity stability -- the deeper fix (memoizing onUpdatePair through
  // the whole prop chain so callback identities stay stable) is a real,
  // separate improvement flagged for its own pass, not applied here.
  const prevReviewOutcomeRef = useRef<ReviewOutcome | null>(null)
  useEffect(() => {
    if (reviewOutcome === 'approved' && prevReviewOutcomeRef.current !== 'approved') {
      markTier3Ready()
    }
    prevReviewOutcomeRef.current = reviewOutcome
  }, [reviewOutcome, markTier3Ready])

  const syncPaneLayout = useCallback(() => {
    const body = contentBodyRef.current
    if (!body) {
      setPaneRects({})
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
    if (!body) return
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
  // that ISN'T "the user left Tier 3 entirely" -- WorkspaceShell.tsx
  // unmounts it whenever Board is 'full' (a real unmount, not
  // CSS-hiding, per that file's own doc on why). Pre-slice-4, unmounting
  // always meant "gone for good in practice" (no other way back short of
  // re-selecting the Persona and reopening Tier 3), so closing every open
  // pane on unmount was correct. Now, with the pair's own state persisted
  // in NavState.workspace.pair, an unmount here can be purely cosmetic --
  // the user may toggle straight back to 'compact'/'minimized' a moment
  // later, and the whole point of the persistence decision (2026-09-01
  // scoping session) is that the pane should still be there when they do.
  //
  // Fix: HIDE the active pane on unmount instead of closing it, reusing
  // commands.setActivePane's own existing was_hidden side effect
  // (commands/tier3_pane.rs -- the same mechanism reclaimChat already
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
        void commands.closeTier3Pane(id)
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
        // fires tier3-popup-closed for the other close paths (self-close,
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
        .requestTier3Gate3Review({
          user_id: requireCurrentUserId(),
          persona_id: personaId,
          message_id: messageId,
        })
        .then((result) => {
          if (result.status !== 'ok') {
            setReviewOutcome('blocked')
            setReviewMessage(
              t('navShell.tier3AccessPane.gate3ReviewError', { message: result.error }),
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
          // server-side (see request_tier3_gate3_review's own doc comment);
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
  // listener above. tier3-popup-opened/-closed are the two popup-close
  // paths the frontend has no other way to learn about (self-close,
  // parent navigate-away) -- see PopupClosedPayload's own doc for why the
  // parent-pane-close path (handleClose above) does not rely on this.
  useEffect(() => {
    let unlistenOpened: UnlistenFn | undefined
    let unlistenClosed: UnlistenFn | undefined
    let cancelled = false

    listen<PopupOpenedPayload>('tier3-popup-opened', (event) => {
      const { provider_id, rect } = event.payload
      setPopupRects((rects) => ({ ...rects, [provider_id]: rect }))
    }).then((fn) => {
      if (cancelled) {
        fn()
      } else {
        unlistenOpened = fn
      }
    })

    listen<PopupClosedPayload>('tier3-popup-closed', (event) => {
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
        commands.resolveTier3Gate3Review({
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

  // DIAG_329 (items.id=329): dev-only test scaffolding -- jumps straight to
  // the rail/pane-open state by seeding a synthetic drafted message
  // (commands.devSeedTier3DraftMessage, debug builds only -- see its Rust
  // doc comment) instead of typing a message and waiting several seconds for
  // the local model's response. From there it calls the exact same
  // handleDraftReady this pane already uses for a real ChatPane draft, so
  // the seeded message runs through the real requestTier3Gate3Review ->
  // gate3() approve/deny path unmodified -- this button only fabricates the
  // input Gate3 reviews, not Gate3's own decision. If focus_settings'
  // max_permitted_tier for this persona's quick-ask Focus is below 3, this
  // hits the same real tier-ceiling block (and "raise it now" affordance,
  // FocusSettingsControls below) a real message would. Remove once
  // items.id=329's Tier 3 pane work no longer needs fast iteration.
  // DIAG_356 (items.id=356): bypasses gate3() entirely via the dev-only
  // dev_bypass_tier3_gate3_review command instead of routing the seeded
  // message through handleDraftReady's real requestTier3Gate3Review call --
  // see that Rust command's own doc comment (commands/consent.rs) for why.
  // gate3's own zero-spans-forced-High branch (D6-362/decisions.id=405)
  // reliably fires for this synthetic message and surfaces a Privacy
  // Guardian modal with nothing in it to review, which must not block
  // otherwise-unrelated Tier 3 pane testing -- that empty-modal branch is a
  // real, separately-tracked UX gap (items.id=356), deliberately NOT
  // redesigned by this workaround. reviewOutcome is never set to 'pending'
  // here, so the modal never mounts, not even momentarily. Real ChatPane
  // drafts still go through handleDraftReady, unchanged.
  const handleDevForceTier3 = () => {
    if (!personaId) return
    commands
      .devSeedTier3DraftMessage(
        requireCurrentUserId(),
        personaId,
        `tier3-access-${personaId}`,
      )
      .then((seedResult) => {
        if (seedResult.status !== 'ok') {
          setOpenError(seedResult.error)
          return
        }
        commands
          .devBypassTier3Gate3Review({
            user_id: requireCurrentUserId(),
            persona_id: personaId,
            message_id: seedResult.data,
          })
          .then((bypassResult) => {
            if (bypassResult.status === 'ok') {
              setReviewOutcome('approved')
            } else {
              setOpenError(bypassResult.error)
            }
          })
      })
  }

  const handleModalCancel = () => {
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

  /** decisions.id=740: the new-chat persona dot-picker's one action --
   *  create a fresh chat for `persona` and make it the one showing,
   *  switching activePersonaId too if `persona` isn't already the current
   *  one. See NewChatPersonaPicker's own header comment for why this is
   *  the ONLY way to start a new chat in this build (no separate "New
   *  chat" button). */
  const handleStartNewChat = useCallback(
    (persona: PersonaInfo) => {
      commands.createChat(requireCurrentUserId(), persona.id).then((result) => {
        if (result.status !== 'ok') {
          setOpenError(result.error)
          return
        }
        resetGate3State()
        setActiveChat(result.data)
        if (persona.id !== personaId) {
          onPersonaChange(persona.id)
        }
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

  const activeProvider = providers.find((p) => p.id === activeProviderId) ?? null

  // items.id=368: switching the active pane (setActivePane, Rust) hides the
  // OUTGOING pane's own popup CEF-side (was_hidden(true)) but does not close
  // it -- it stays alive/tracked (PaneManager::popups, "one active popup per
  // pane", not cleared until a real tier3-popup-closed event) so it can be
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
  const chatContextKey = activeChat ? activeChat.context_key : `tier3-access-${personaId}`

  return (
    <div className="tier3-access-pane" data-dominant={dominant}>
      <PaneHitLayer rects={paneRects} />
      {/* items.id=234: mounted AFTER PaneHitLayer -- DOM source order alone
          resolves popup-vs-parent-pane hit-test precedence in any
          overlapping region (see PopupHitLayer's own doc). */}
      <PopupHitLayer popups={activePopupRects} />

      <div
        className="tier3-access-pane__qr"
        data-collapsed={dominant === 'tier3' ? '' : undefined}
      >
        {personaId ? (
          <>
            {/* decisions.id=743: QR's own identity banner -- only while
                expanded. QR's collapsed floor (dominant === 'tier3') has
                its own compact mark via ChatPane's collapsed strip
                already; this banner would be redundant chrome on top of
                that, and the mockup this decision traces to only ever
                shows it alongside the full transcript view. items.id=384
                slice 7: the chat-history/new-chat tools share this same
                row -- the reference mockup shows them in the collapsed
                floor too, but building that means reaching into
                ChatPane's own collapsed-strip markup, which this item's
                plan explicitly keeps unchanged; deferred, not silently
                dropped. Both disabled while a Gate3 review is pending --
                see resetGate3State's own comment on why switching mid-
                review is avoided rather than handled. */}
            {dominant === 'chat' && (
              <div className="tier3-access-pane__qr-banner">
                <span className="tier3-access-pane__qr-banner-name">
                  {t('navShell.tier3AccessPane.qrBannerName')}
                </span>
                <div className="tier3-access-pane__qr-banner-tools">
                  <ChatHistoryList
                    userId={requireCurrentUserId()}
                    personaId={personaId}
                    activeChatId={activeChat?.id ?? null}
                    onSelectChat={handleSelectChat}
                    disabled={reviewOutcome === 'pending'}
                  />
                  <NewChatPersonaPicker
                    personas={personas}
                    activePersonaId={personaId}
                    onStartNewChat={handleStartNewChat}
                    disabled={reviewOutcome === 'pending'}
                  />
                </div>
              </div>
            )}
            <ChatPane
              contextKey={chatContextKey}
              userId={requireCurrentUserId()}
              personaId={personaId}
              focusId="quick-ask"
              gate3Track={true}
              onDraftReady={handleDraftReady}
              collapsed={dominant === 'tier3'}
              onExpand={reclaimChat}
            />
          </>
        ) : (
          <p>{t('navShell.content.tier3ChatUnavailable')}</p>
        )}
      </div>

      {/* DIAG_329 (items.id=329): dev-only, see handleDevForceTier3's own
          comment. Deliberately OUTSIDE the dominant branches below -- it
          needs to be clickable from a fresh dominant:'chat' session too
          (that's the whole point: skip straight to the approved/rail
          state without typing a message first). Its own success path
          sets reviewOutcome to 'approved', which the effect above turns
          into dominant:'tier3' on its own -- no direct call needed here. */}
      {import.meta.env.DEV && personaId && (
        <button type="button" onClick={handleDevForceTier3}>
          Dev: force Tier 3 escalation
        </button>
      )}

      {dominant === 'tier3' ? (
        <div className="tier3-access-pane__split-area">
          <div className="tier3-access-pane__rail-col">
            <h3>{t('navShell.tier3AccessPane.heading')}</h3>
            {providerError && (
              <p role="alert">
                {t('navShell.tier3AccessPane.providerError', {
                  message: providerError,
                })}
              </p>
            )}
            {providers.length === 0 && !providerError && (
              <p>{t('navShell.tier3AccessPane.loadingProviders')}</p>
            )}
            {providers.length > 0 && dominant === 'tier3' && (
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
              // inside the dominant === 'tier3' branch (see below), so this
              // check is really just making explicit what's already true --
              // dominant only ever becomes 'tier3' downstream of Gate3
              // having cleared at least once (markTier3Ready/activate), the
              // same fact reviewOutcome === 'approved' used to capture, but
              // via a field that actually survives the remount reviewOutcome
              // doesn't.
              <Tier3Selector
                providers={providers}
                openPaneIds={openProviderIds}
                activeProviderId={activeProviderId}
                onActivate={activate}
                onClose={handleClose}
              />
            )}
          </div>

          <div className="tier3-access-pane__content-pane">
            {activeProviderId !== null && (
              <div className="tier3-access-pane__content-head">
                <span className="tier3-access-pane__content-head-name">
                  {activeProvider?.name ?? activeProviderId}
                </span>
                <button
                  type="button"
                  className="tier3-access-pane__content-head-close"
                  title={t('navShell.tier3AccessPane.contentCloseButton')}
                  onClick={(e) => {
                    e.stopPropagation()
                    handleClose(activeProviderId)
                  }}
                >
                  &times;
                </button>
              </div>
            )}
            <div ref={contentBodyRef} className="tier3-access-pane__content-body">
              {activeProviderId === null && (
                <p className="tier3-access-pane__content-empty">
                  {t('navShell.tier3AccessPane.contentEmptyPrompt')}
                </p>
              )}
            </div>
          </div>
        </div>
      ) : (
        <Tier3CollapsedStrip
          providers={providers}
          openProviderIds={openProviderIds}
          activeProviderId={activeProviderId}
          onExpand={activate}
        />
      )}

      {/* Gate3 review surfaces -- deliberately OUTSIDE the dominant
          branches above, see this file's own header comment on why. */}
      {reviewOutcome === 'blocked' && (
        <p role="alert">{reviewMessage ?? t('navShell.tier3AccessPane.gate3BlockedFallback')}</p>
      )}
      {reviewOutcome === 'blocked' && reviewCeiling && personaId && (
        <FocusSettingsControls
          userId={requireCurrentUserId()}
          personaId={personaId}
          focusId="quick-ask"
          mode="ceilingOnly"
          suggestedMaxPermittedTier={reviewCeiling.targetTier}
          onSaved={() => {
            setReviewCeiling(null)
            if (pendingMessageId) handleDraftReady(pendingMessageId)
          }}
        />
      )}
      {reviewOutcome === 'withheld' && <p>{t('navShell.tier3AccessPane.gate3Withheld')}</p>}
      <PrivacyGuardianModal
        open={reviewOutcome === 'pending'}
        payload={consentPayload}
        onResolve={handleModalResolve}
        onCancel={handleModalCancel}
      />
      {openError && (
        <p role="alert">
          {t('navShell.tier3AccessPane.openError', { message: openError })}
        </p>
      )}
    </div>
  )
}
