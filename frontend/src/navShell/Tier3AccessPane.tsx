// Tier 2/Tier 3 access -- rail + content-pane hosting (items.id=359,
// replacing the former two-box selector + fixed-split-column model,
// items.id=3/202/223's original harness-derived layout).
//
// TIER3_ACCESS_MODEL.md Session 3 (decisions.id=731-733): the rail lists
// every candidate provider (no cap); at most one pane is ever composited
// at a time (activeProviderId below, mirrored to Rust's own
// PaneManager.active_pane via commands.setActivePane -- see
// pane_host.rs); QR's own conversation collapses to a minimal floor
// whenever a provider is active, freeing near-full height for that
// provider's own page. This last piece is NEW behavior (see this item's
// plan file / session handoff for why: MiddleZone's existing resting/
// active-ratio mechanism is a side-by-side two-slot WIDTH splitter, never
// actually wired to Tier 3 panes in the live code, and can't do a
// collapse-a-whole-panel-to-a-floor-beside-an-unrelated-sibling-block
// the way this design needs -- so this is built fresh here, following
// IA Section 3's own principles (focus-location trigger, not a timer)
// rather than literally reusing MiddleZone's code). MiddleZone itself is
// untouched.
//
// Section 9's hard requirement (relaxed by decisions.id=731 specifically
// for the provider-active case): QR's own conversation and a Tier 3
// exchange must remain simultaneously visible where possible. QR is
// collapsed, not unmounted, while a provider is active -- ChatPane's own
// `collapsed` prop keeps its message state (and live entry bar) mounted
// throughout, per Jason's explicit build-time preference for showing the
// real last response in the collapsed floor, not a placeholder.
//
// items.id=233's outbound Privacy Guardian gate (PG_GATE_3,
// conductor/privacy/gate3.rs) ahead of the rail appearing at all is
// unchanged by this item -- see handleDraftReady/the consent_request
// listener below, carried over from the prior selector-screen version of
// this file.

import { useCallback, useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { commands, type PaneRectFraction } from '../bindings'
import { ChatPane } from '../chat/ChatPane'
import { FocusSettingsControls } from './FocusSettingsControls'
import { requireCurrentUserId } from './navShellConfig'
import { computeActivePaneRect, pixelRectToFraction, type PanePixelRect } from '../tier3Access/paneLayout'
import { PaneHitLayer } from '../tier3Access/PaneHitLayer'
import { PopupHitLayer } from '../tier3Access/PopupHitLayer'
import {
  PrivacyGuardianModal,
  type ConsentRequestPayload,
  type ElementDecision,
} from '../tier3Access/PrivacyGuardianModal'
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
  /** The persona this Tier 3 session was opened from -- captured by
   *  NavShell.tsx at the moment the Tier 3 button is clicked (see its own
   *  comment on why NavState can't carry this through the anchor switch
   *  on its own). null only if this somehow renders before that capture
   *  happens (isTier3Enabled's gating should prevent that in practice). */
  personaId: string | null
}

export function Tier3AccessPane({ personaId }: Tier3AccessPaneProps) {
  const { t } = useTranslation()
  const [providers, setProviders] = useState<Provider[]>([])
  const [providerError, setProviderError] = useState<string | null>(null)
  /** Providers with an open (loaded) pane in Rust -- may or may not
   *  include activeProviderId (loaded-but-inactive rows show nowhere
   *  except their own rail row, per the rail model). */
  const [openPaneIds, setOpenPaneIds] = useState<string[]>([])
  /** The one provider currently shown in the content pane, or null when
   *  QR is expanded / nothing has been activated yet. Mirrored to Rust's
   *  PaneManager.active_pane via commands.setActivePane -- see that
   *  command's own doc (commands/tier3_pane.rs) for the deactivation
   *  side-effects (scroll-to-bottom, was_hidden) this triggers there. */
  const [activeProviderId, setActiveProviderId] = useState<string | null>(null)
  const [openError, setOpenError] = useState<string | null>(null)
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
  // frontend-measured the way paneRects is.
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
  }, [activeProviderId])

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

  // items.id=330: `PaneManager` (Rust) has no way to know this component
  // lost track of a pane -- the only other caller of closeTier3Pane is
  // handleClose's own manual "Close" button below. Without this, navigating
  // to a different top-level tab (a real unmount -- NavShell.tsx returns an
  // entirely different JSX branch per content.type, this isn't just
  // CSS-hidden) leaves the pane compositing with nothing left to route
  // input to it or ever close it, short of quitting the whole app
  // (RunEvent::Exit's close_all_panes(), main.rs -- and even that bypasses
  // cookie persistence, unlike closeTier3Pane).
  //
  // openPaneIdsRef mirrors openPaneIds into a ref: the effect below has `[]`
  // deps (must run its cleanup exactly once, on this component's actual
  // unmount) and would otherwise close over whatever openPaneIds was at
  // mount time -- empty, always -- not whatever's actually open when the
  // unmount happens.
  const openPaneIdsRef = useRef<string[]>([])
  useEffect(() => {
    openPaneIdsRef.current = openPaneIds
  }, [openPaneIds])

  useEffect(() => {
    // Also covers an actual page/webview reload, not just in-app
    // navigation: RunEvent::Exit only fires on real app-process exit, not a
    // reload that leaves the Rust process (and this pane) running.
    const closeAllOpenPanes = () => {
      for (const id of openPaneIdsRef.current) {
        void commands.closeTier3Pane(id)
      }
    }
    window.addEventListener('beforeunload', closeAllOpenPanes)
    return () => {
      window.removeEventListener('beforeunload', closeAllOpenPanes)
      closeAllOpenPanes()
    }
  }, [])

  /** Idle row -> load then activate in one step; loaded row -> just
   *  switch the content pane (no reload). The doc's "the large content
   *  pane itself is clickable, not only the rail row" is already
   *  satisfied structurally, not by a second handler here: PaneHitLayer's
   *  hit-div sits on top of the content pane's exact rect (position:
   *  fixed, no z-index needed -- painted after ordinary in-flow content
   *  regardless of DOM order) and already calls CEF's set_focus(true) on
   *  every real click that lands there (pane_host.rs's MouseClick dispatch
   *  arm) -- and "provider active" and "QR collapsed" are the same
   *  boolean in this design, so there is no separate "QR re-expanded
   *  while a pane still shows" state left to reclaim from. */
  const handleActivate = (providerId: string) => {
    setOpenError(null)
    if (openPaneIds.includes(providerId)) {
      setActiveProviderId(providerId)
      commands.setActivePane(providerId).then((result) => {
        if (result.status !== 'ok') setOpenError(result.error)
      })
      return
    }
    commands.openTier3Panes([providerId]).then((result) => {
      if (result.status !== 'ok') {
        setOpenError(result.error)
        return
      }
      setOpenPaneIds((ids) => (ids.includes(providerId) ? ids : [...ids, providerId]))
      setActiveProviderId(providerId)
      commands.setActivePane(providerId).then((setResult) => {
        if (setResult.status !== 'ok') setOpenError(setResult.error)
      })
    })
  }

  const handleClose = (providerId: string) => {
    commands.closeTier3Pane(providerId).then((result) => {
      if (result.status === 'ok') {
        setOpenPaneIds((ids) => ids.filter((id) => id !== providerId))
        if (activeProviderId === providerId) setActiveProviderId(null)
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
      } else {
        setOpenError(result.error)
      }
    })
  }

  /** decisions.id=731's symmetric transition: clicking the collapsed QR
   *  strip, or focusing its entry input, re-expands QR and returns the
   *  currently-active provider to loaded (its row, no longer active) --
   *  nothing is closed. */
  const handleExpandQR = useCallback(() => {
    setActiveProviderId(null)
    commands.setActivePane(null).then((result) => {
      if (result.status !== 'ok') setOpenError(result.error)
    })
  }, [])

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
    const allKeptPrivate = decisions.every((d) => d.decision === 'keep_private')
    const status = allKeptPrivate ? 'withheld' : 'approved'

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
  const handleDevForceTier3 = () => {
    if (!personaId) return
    commands
      .devSeedTier3DraftMessage(
        requireCurrentUserId(),
        personaId,
        `tier3-access-${personaId}`,
      )
      .then((result) => {
        if (result.status === 'ok') {
          handleDraftReady(result.data)
        } else {
          setOpenError(result.error)
        }
      })
  }

  const handleModalCancel = () => {
    setConsentPayload(null)
    setPendingMessageId(null)
    setReviewOutcome(null)
  }

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

  return (
    <div className="tier3-access-pane">
      <PaneHitLayer rects={paneRects} />
      {/* items.id=234: mounted AFTER PaneHitLayer -- DOM source order alone
          resolves popup-vs-parent-pane hit-test precedence in any
          overlapping region (see PopupHitLayer's own doc). */}
      <PopupHitLayer popups={activePopupRects} />

      <div
        className="tier3-access-pane__qr"
        data-collapsed={activeProviderId !== null ? '' : undefined}
      >
        {personaId ? (
          <ChatPane
            contextKey={`tier3-access-${personaId}`}
            userId={requireCurrentUserId()}
            personaId={personaId}
            focusId="quick-ask"
            gate3Track={true}
            onDraftReady={handleDraftReady}
            collapsed={activeProviderId !== null}
            onExpand={handleExpandQR}
          />
        ) : (
          <p>{t('navShell.content.tier3ChatUnavailable')}</p>
        )}
      </div>

      <div className="tier3-access-pane__split-area">
        <div className="tier3-access-pane__rail-col">
          <h3>{t('navShell.tier3AccessPane.heading')}</h3>
          {/* DIAG_329 (items.id=329): dev-only, see handleDevForceTier3's own comment. */}
          {import.meta.env.DEV && personaId && (
            <button type="button" onClick={handleDevForceTier3}>
              Dev: force Tier 3 escalation
            </button>
          )}
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
          {reviewOutcome === 'withheld' && (
            <p>{t('navShell.tier3AccessPane.gate3Withheld')}</p>
          )}
          {providers.length > 0 && reviewOutcome === 'approved' && (
            // items.id=359: the rail is persistent once the gate clears --
            // unlike the retired selector screen, it does not disappear
            // once a pane opens (TIER3_ACCESS_MODEL.md States section 3).
            <Tier3Selector
              providers={providers}
              openPaneIds={openPaneIds}
              activeProviderId={activeProviderId}
              onActivate={handleActivate}
              onClose={handleClose}
            />
          )}
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
    </div>
  )
}
