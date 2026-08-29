// Tier 3 access -- pane hosting, re-hosted from the former App.tsx harness
// (items.id=3/202/223) behind the real Tier 3 access button (items.id=232).
// The state/effects/handlers below are the same mechanism the harness
// proved, relocated here rather than rebuilt -- see paneLayout.ts,
// tier3AccessConfig.ts, Tier3Selector.tsx, none of which changed.
//
// Section 9's hard requirement: QR's own conversation and a Tier 3
// exchange must remain simultaneously visible (so content can be copied
// between them), not swapped in place of each other -- hence MiddleZone
// stays mounted alongside the selector/pane dock here, same split this
// item's harness predecessor used.
//
// MiddleZone's chatPane is now ChatPane -- the real starter-drafting
// component (items.id=245-ish), not a placeholder. It reuses the same
// "quick-ask" Focus path Persona hub chat uses: FOCUS_ROADMAP.md states
// plainly (line 346) "Tier 3 -- shared infrastructure, built on-demand,
// not standalone Focuses," and TIER3_ACCESS_MODEL.md (line 413) confirms
// the starter-drafting pre-conversation uses "the same context-assembly
// mechanism QR already uses for its own responses" -- no dedicated
// starter-drafting Focus exists or should exist. gate3Track=true is the
// only thing that differs from Persona hub's ChatPane usage: it marks the
// assistant reply's gate3_review_status="drafted", the row the outbound
// Privacy Guardian review below transitions further.
//
// items.id=233's remaining stub, now built: the outbound Privacy Guardian
// gate (PG_GATE_3, conductor/privacy/gate3.rs) ahead of the Selector
// screen. handleDraftReady calls commands.requestTier3Gate3Review the
// moment ChatPane signals a real drafted message; on pending_consent the
// consent_request listener below picks up the payload (already emitted by
// the time the command's promise resolves -- gate3()'s write-before-surface
// invariant writes the disclosure_log entry and emits synchronously before
// returning) and mounts PrivacyGuardianModal. The Selector only renders
// once reviewOutcome === 'approved'.

import { useCallback, useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { commands, type PaneRectFraction } from '../bindings'
import { ChatPane } from '../chat/ChatPane'
import { MiddleZone } from '../middleZone/MiddleZone'
import { DEFAULT_CONVERSATION_PROFILE } from '../middleZone/middleZoneConfig'
import { FocusSettingsControls } from './FocusSettingsControls'
import { requireCurrentUserId } from './navShellConfig'
import { computePaneRects, pixelRectToFraction, isRowFullyVisible, PANE_ROW_HEIGHT, type PanePixelRect } from '../tier3Access/paneLayout'
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
  const [chatGenerating, setChatGenerating] = useState(false)
  const [providers, setProviders] = useState<Provider[]>([])
  const [providerError, setProviderError] = useState<string | null>(null)
  const [confirmedProviders, setConfirmedProviders] = useState<
    Provider[] | null
  >(null)
  const [openPaneIds, setOpenPaneIds] = useState<string[]>([])
  const [openError, setOpenError] = useState<string | null>(null)
  const paneDockRef = useRef<HTMLDivElement>(null)
  // items.id=334: the dock column's own visible bounds -- distinct from
  // paneDockRef, whose element can now be far taller than what's on screen
  // (PANE_ROW_HEIGHT stacking has no ceiling). syncPaneLayout clips each
  // pane's row rect against this before it ever reaches paneRects/Rust.
  const paneColumnRef = useRef<HTMLDivElement>(null)
  // CSS-pixel-space rects, viewport-relative -- the same numbers
  // syncPaneLayout divides down into the PaneRectFraction sent to Rust, fed
  // straight to PaneHitLayer for its invisible per-pane hit-divs' position
  // (items.id=257 Path B; see paneLayout.ts's module doc).
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
    const dock = paneDockRef.current
    const column = paneColumnRef.current
    if (!dock || !column || openPaneIds.length === 0) {
      setPaneRects({})
      return
    }
    const rawRects = computePaneRects(dock.getBoundingClientRect(), openPaneIds)
    const columnRect = column.getBoundingClientRect()
    const viewport = { top: columnRect.top, bottom: columnRect.bottom }
    const rects: Record<string, PanePixelRect> = {}
    for (const [id, rect] of Object.entries(rawRects)) {
      if (isRowFullyVisible(rect, viewport)) rects[id] = rect
    }
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
  }, [openPaneIds])

  useEffect(() => {
    const dock = paneDockRef.current
    const column = paneColumnRef.current
    if (!dock || !column) return
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
    observer.observe(dock)
    // items.id=257 (2026-08-28): ResizeObserver only fires when the dock's
    // own border-box SIZE changes -- confirmed live that maximizing the
    // window shifts the dock's on-screen X position (its own column grows)
    // while its width/height stay byte-identical, so ResizeObserver never
    // fires and this pane's rect goes stale (Rust keeps compositing against
    // an old fraction that no longer matches the dock's real position).
    // window's own 'resize' event fires on any window-size change
    // regardless of whether this specific element's size happened to
    // change, so it catches exactly the case ResizeObserver misses.
    window.addEventListener('resize', scheduleSync)
    // items.id=334: scrolling the column moves the dock's on-screen
    // position/visible portion without changing its own size or the
    // window's, so neither of the above fires -- a plain scroll listener
    // is the only thing that catches it.
    column.addEventListener('scroll', scheduleSync)
    return () => {
      observer.disconnect()
      window.removeEventListener('resize', scheduleSync)
      column.removeEventListener('scroll', scheduleSync)
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

  const handleConfirm = (selected: Provider[]) => {
    setConfirmedProviders(selected)
    setOpenError(null)
    commands.openTier3Panes(selected.map((p) => p.id)).then((result) => {
      if (result.status === 'ok') {
        setOpenPaneIds(selected.map((p) => p.id))
      } else {
        setOpenError(result.error)
      }
    })
  }

  const handleClose = (providerId: string) => {
    commands.closeTier3Pane(providerId).then((result) => {
      if (result.status === 'ok') {
        setOpenPaneIds((ids) => ids.filter((id) => id !== providerId))
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
  // the Tier3Selector/pane-open state by seeding a synthetic drafted message
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

  return (
    <div className="tier3-access-pane">
      <PaneHitLayer rects={paneRects} />
      {/* items.id=234: mounted AFTER PaneHitLayer -- DOM source order alone
          resolves popup-vs-parent-pane hit-test precedence in any
          overlapping region (see PopupHitLayer's own doc). */}
      <PopupHitLayer popups={popupRects} />
      <div className="tier3-access-pane__conversation">
        <MiddleZone
          contextKey={
            personaId ? `tier3-access-${personaId}` : 'tier3-access'
          }
          profile={DEFAULT_CONVERSATION_PROFILE}
          isGenerating={chatGenerating}
          contextPane={<p>{t('navShell.content.tier3ContextPlaceholder')}</p>}
          chatPane={
            personaId ? (
              <ChatPane
                contextKey={`tier3-access-${personaId}`}
                userId={requireCurrentUserId()}
                personaId={personaId}
                focusId="quick-ask"
                gate3Track={true}
                onGenerating={setChatGenerating}
                onDraftReady={handleDraftReady}
              />
            ) : (
              <p>{t('navShell.content.tier3ChatUnavailable')}</p>
            )
          }
        />
      </div>

      <div className="tier3-access-pane__dock-column" ref={paneColumnRef}>
        <div
          ref={paneDockRef}
          className="tier3-access-pane__dock"
          data-has-panes={openPaneIds.length > 0 ? '' : undefined}
          style={
            openPaneIds.length > 0
              ? { height: openPaneIds.length * PANE_ROW_HEIGHT }
              : undefined
          }
        >
          {/* items.id=334: invisible per-row anchors, one per open pane --
              not visual chrome (see the reverted per-row header attempt's
              own history), just scroll-snap-align targets so the column
              (scroll-snap-type: y, NavShell.css) can only rest with whole
              rows visible, never a partial one -- see isRowFullyVisible's
              own doc for why a partial reveal must never happen. */}
          {openPaneIds.map((id) => (
            <div
              key={id}
              className="tier3-access-pane__pane-row"
              style={{ height: PANE_ROW_HEIGHT }}
            />
          ))}
        </div>

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
        {providers.length > 0 && reviewOutcome === 'approved' && openPaneIds.length === 0 && (
          // items.id=329: gated on openPaneIds too, not just reviewOutcome --
          // reviewOutcome stays 'approved' for the rest of this component's
          // life once set, so without this the selector's checkboxes stayed
          // mounted (and clickable) underneath the dock the whole time a
          // pane was open, a stray target for any hit-testing gap to fall
          // through onto. Reappears if the user closes back down to zero
          // open panes, which is also the correct behavior for opening more.
          <Tier3Selector providers={providers} onConfirm={handleConfirm} />
        )}
        <PrivacyGuardianModal
          open={reviewOutcome === 'pending'}
          payload={consentPayload}
          onResolve={handleModalResolve}
          onCancel={handleModalCancel}
        />
        {confirmedProviders && (
          <p>
            {t('navShell.tier3AccessPane.confirmedLabel', {
              names: confirmedProviders.map((p) => p.name).join(', '),
            })}
          </p>
        )}
        {openError && (
          <p role="alert">
            {t('navShell.tier3AccessPane.openError', { message: openError })}
          </p>
        )}

        <h4>{t('navShell.tier3AccessPane.openPanesLabel')}</h4>
        {openPaneIds.length === 0 ? (
          <p>{t('navShell.tier3AccessPane.noPanesOpen')}</p>
        ) : (
          <ul>
            {openPaneIds.map((id) => (
              <li key={id}>
                {providers.find((p) => p.id === id)?.name ?? id}{' '}
                <button type="button" onClick={() => handleClose(id)}>
                  {t('navShell.tier3AccessPane.closeButton')}
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  )
}
