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
// this screen in the real flow (items.id=233 -- explicitly out of scope
// for items.id=232, blocked on it). Same caller responsibility
// Tier3Selector's own header already documents; this pane is the
// caller, and still doesn't do it.

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
