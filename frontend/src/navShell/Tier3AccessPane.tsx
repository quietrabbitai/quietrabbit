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
// item's harness predecessor used. MiddleZone's own content is a
// placeholder (no real Persona-chat wiring exists yet); the structural
// side-by-side relationship is what this pane is responsible for.
//
// Does NOT mount the outbound Privacy Guardian gate that must precede
// this screen in the real flow. items.id=233 investigated this (its
// former blocker, items.id=232, is now resolved) and confirmed which
// gate applies, then deliberately descoped to this stub rather than
// build the real flow -- see the loud marker at the Tier3Selector
// render site below for what's still missing and why.

import { useCallback, useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { commands } from '../bindings'
import { MiddleZone } from '../middleZone/MiddleZone'
import { DEFAULT_CONVERSATION_PROFILE } from '../middleZone/middleZoneConfig'
import { computePaneLayout } from '../tier3Access/paneLayout'
import { Tier3Selector } from '../tier3Access/Tier3Selector'
import {
  fetchActiveProviders,
  type Provider,
} from '../tier3Access/tier3AccessConfig'

export function Tier3AccessPane() {
  const { t } = useTranslation()
  const [providers, setProviders] = useState<Provider[]>([])
  const [providerError, setProviderError] = useState<string | null>(null)
  const [confirmedProviders, setConfirmedProviders] = useState<
    Provider[] | null
  >(null)
  const [openPaneIds, setOpenPaneIds] = useState<string[]>([])
  const [openError, setOpenError] = useState<string | null>(null)
  const paneDockRef = useRef<HTMLDivElement>(null)

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

  return (
    <div className="tier3-access-pane">
      <div className="tier3-access-pane__conversation">
        <MiddleZone
          contextKey="tier3-access"
          profile={DEFAULT_CONVERSATION_PROFILE}
          isGenerating={false}
          contextPane={<p>{t('navShell.content.tier3ContextPlaceholder')}</p>}
          chatPane={<p>{t('navShell.content.tier3ChatPlaceholder')}</p>}
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
        {/*
         * NO OUTBOUND PRIVACY GUARDIAN REVIEW HAPPENS BEFORE THIS SCREEN.
         * items.id=233 -- confirmed (2026-08-09) which gate belongs here,
         * then deliberately descoped to this stub rather than build the
         * real flow this session. Per TIER3_ACCESS_MODEL.md's locked
         * design, the Selector screen below must not be reachable until
         * a Privacy Guardian review has resolved on the drafted content
         * headed to Tier 3 -- today it renders unconditionally instead.
         *
         * Confirmed gate: PG_GATE_3 (conductor/privacy/gate3.rs), NOT
         * Gate 1 and NOT Gate 4. TIER3_ACCESS_MODEL.md states plainly
         * that Tier 3's outbound review "uses the locked per-span modal
         * directly, with Tier 3 destination forcing the High tier" --
         * i.e. PRIVACY_GUARDIAN_GATE_SPEC.md's Easy/Medium/High modal,
         * which gate3.rs's assign_review_tier() already implements
         * (target_tier >= 3 forces ReviewTier::High). Gate 1 is wrong
         * for this trigger point regardless of naming: it needs an
         * executing Focus step's PersonalTrack/step_id/focus_run_id,
         * none of which exist here -- commands.openTier3Panes takes
         * only provider IDs. Gate 4 is uncited in any spec doc.
         *
         * gate3() DOES have a real call site -- conductor/executor.rs:632,
         * inside the Conductor's own Focus-execution step loop -- but it
         * is not exposed as an IPC command, and it needs ctx.step /
         * focus_run_id / a prior step's response content, none of which
         * exist at this trigger point either. Useful precedent for the
         * data shape the real wiring will need, not something reusable
         * as-is.
         *
         * Three pieces are missing before this can be real, none built:
         *   1. A starter-drafting step -- TIER3_ACCESS_MODEL.md's
         *      "QR-only pre-conversation" state, where QR assembles the
         *      content that will actually cross to Tier 3. MiddleZone
         *      here is still a placeholder with no real chat wiring, so
         *      there is no drafted content to gate in the first place.
         *   2. An IPC path that runs gate3() against that drafted
         *      content and returns/streams the result to this screen.
         *   3. The Easy/Medium/High consent modal UI itself
         *      (PRIVACY_GUARDIAN_GATE_SPEC.md) -- no such component
         *      exists anywhere in frontend/src today.
         */}
        {providers.length > 0 && (
          <Tier3Selector providers={providers} onConfirm={handleConfirm} />
        )}
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
