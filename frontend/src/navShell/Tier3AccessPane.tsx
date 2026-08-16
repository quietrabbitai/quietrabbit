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
import { commands } from '../bindings'
import { ChatPane } from '../chat/ChatPane'
import { MiddleZone } from '../middleZone/MiddleZone'
import { DEFAULT_CONVERSATION_PROFILE } from '../middleZone/middleZoneConfig'
import { requireCurrentUserId } from './navShellConfig'
import { computePaneLayout } from '../tier3Access/paneLayout'
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

  const [reviewOutcome, setReviewOutcome] = useState<ReviewOutcome | null>(null)
  const [reviewMessage, setReviewMessage] = useState<string | null>(null)
  const [consentPayload, setConsentPayload] = useState<ConsentRequestPayload | null>(null)
  // ConsentRequestPayload carries focus_run_id, not message_id -- gate3()
  // only knows about content_key/step_id/focus_run_id, never the
  // messages.db row that triggered it. Stashed here from handleDraftReady
  // so handleModalResolve has the right id to pass to resolveTier3Gate3Review.
  const [pendingMessageId, setPendingMessageId] = useState<string | null>(null)

  const syncPaneLayout = useCallback(() => {
    const dock = paneDockRef.current
    if (!dock || openPaneIds.length === 0) return
    const layout = computePaneLayout(
      dock.getBoundingClientRect(),
      window.innerWidth,
      window.innerHeight,
      openPaneIds,
    )
    const entries = Object.entries(layout).map(([providerId, rect]) => ({
      provider_id: providerId,
      rect,
    }))
    commands.setPaneLayout(entries).then((result) => {
      if (result.status !== 'ok') {
        setOpenError(result.error)
      }
    })
  }, [openPaneIds])

  useEffect(() => {
    const dock = paneDockRef.current
    if (!dock) return
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
    return () => {
      observer.disconnect()
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
              t('navShell.content.gate3ReviewError', { message: result.error }),
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

  const handleModalCancel = () => {
    setConsentPayload(null)
    setPendingMessageId(null)
    setReviewOutcome(null)
  }

  return (
    <div className="tier3-access-pane">
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

      <div className="tier3-access-pane__dock-column">
        <div
          ref={paneDockRef}
          className="tier3-access-pane__dock"
          data-has-panes={openPaneIds.length > 0 ? '' : undefined}
        >
          {openPaneIds.length > 0 && (
            <p>
              {t('navShell.tier3AccessPane.dockLabel', {
                count: openPaneIds.length,
              })}
            </p>
          )}
        </div>

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
        {reviewOutcome === 'blocked' && (
          <p role="alert">{reviewMessage ?? t('navShell.content.gate3BlockedFallback')}</p>
        )}
        {reviewOutcome === 'withheld' && (
          <p>{t('navShell.content.gate3Withheld')}</p>
        )}
        {providers.length > 0 && reviewOutcome === 'approved' && (
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
