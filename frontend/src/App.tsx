import { useCallback, useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { commands, type HealthResponse } from './bindings'
import { MiddleZone } from './middleZone/MiddleZone'
import { DEFAULT_BROWSING_PROFILE } from './middleZone/middleZoneConfig'
import { computePaneLayout } from './tier3Access/paneLayout'
import { Tier3Selector } from './tier3Access/Tier3Selector'
import {
  fetchActiveProviders,
  type Provider,
} from './tier3Access/tier3AccessConfig'

function App() {
  // First real IPC round-trip (items.id=3): calls the read-only, no-argument
  // get_health command to verify the whole pipe -- frontend -> Tauri IPC ->
  // Rust command -> typed response -- actually works, not just that
  // bindings.ts loads. Only verifiable under `tauri dev`, not plain
  // `vite dev`, since IPC does not exist outside a real Tauri window.
  const { t } = useTranslation()
  const [health, setHealth] = useState<HealthResponse | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    commands.getHealth().then((result) => {
      if (result.status === 'ok') {
        setHealth(result.data)
      } else {
        setError(result.error)
      }
    })
  }, [])

  // TEMPORARY HARNESS (items.id=3, 2026-07-31; provider/pane wiring added
  // items.id=202 piece 5 / items.id=223, 2026-08-04): a minimal, throwaway
  // host for MiddleZone and Tier3Selector -- NOT the real top strip /
  // navigation shell (IA spec Section 2) or the outbound Privacy
  // Guardian gate that must precede the selector in the real flow,
  // both separate, undispatched/existing-locked scope. This exists
  // only to prove each mechanism mounts and behaves correctly in
  // isolation; it should be deleted, not extended, once the real
  // navigation shell and gate flow land. Do not build on top of this
  // harness. Kept at the same throwaway level per Jason's explicit
  // 2026-08-04 direction: the actual split-screen container (MiddleZone
  // + 1-3 CEF panes) is still harness-hosted, not the real
  // nav-shell-integrated version -- there is no navigation shell for it
  // to live in yet.
  const [isGenerating, setIsGenerating] = useState(false)
  const [providers, setProviders] = useState<Provider[]>([])
  const [providerError, setProviderError] = useState<string | null>(null)
  const [confirmedProviders, setConfirmedProviders] = useState<
    Provider[] | null
  >(null)
  // Open panes composite into a shared gtk::GLArea overlaid on this same
  // window (pane_host.rs, items.id=202 real positioning fix, 2026-08-07) --
  // not a DOM element, so this harness still can't render them inline, just
  // show which ones are open. `openPaneIds` mirrors the pane manager's own
  // state via openTier3Panes'/closeTier3Pane's own success, not a live push
  // subscription -- adequate for this harness, not the real integration.
  const [openPaneIds, setOpenPaneIds] = useState<string[]>([])
  const [openError, setOpenError] = useState<string | null>(null)

  // items.id=202 piece 4: the actual split-screen container. `paneDock`
  // reserves the on-screen region open panes should sit alongside (this is
  // what makes MiddleZone's own on-screen region concrete, not incidental)
  // -- open CEF panes never render inside it directly (see the openPaneIds
  // comment above), this div only exists to be measured.
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

  // Recomputes on mount, whenever openPaneIds changes (a different pane
  // count changes the column split even at the same dock pixel size -- the
  // ResizeObserver alone would miss that), and on any actual resize of the
  // dock itself (window resize, or the dock's own width transition when
  // panes open/close). rAF-debounced so a main-window drag-resize doesn't
  // spam the IPC call once per intermediate frame.
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
    commands
      .openTier3Panes(selected.map((p) => p.id))
      .then((result) => {
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
    <main>
      <h1>{t('app.title')}</h1>
      {error && <p>{t('health.error', { message: error })}</p>}
      {health && (
        <dl>
          <dt>{t('health.ollamaStatus')}</dt>
          <dd>{health.ollama.status}</dd>
          <dt>{t('health.ollamaSource')}</dt>
          <dd>{health.ollama_source}</dd>
          <dt>{t('health.tier2Configured')}</dt>
          <dd>{String(health.tier2_configured)}</dd>
        </dl>
      )}
      {!health && !error && <p>{t('health.loading')}</p>}

      <button type="button" onClick={() => setIsGenerating((v) => !v)}>
        {isGenerating
          ? t('middleZoneHarness.generatingToggleOff')
          : t('middleZoneHarness.generatingToggleOn')}
      </button>

      {/* position: sticky pins this row's on-screen position regardless of
          how far the rest of this scrollable harness page is scrolled --
          required, not cosmetic: computePaneLayout reads the dock's
          getBoundingClientRect(), which is viewport-relative. Without this,
          scrolling down to reach the Tier3Selector/Continue button below
          (unavoidable at this window size) pushes the dock above the
          viewport, producing a negative top and sending every pane to a
          bogus position near the window's top-left -- found via manual
          verification, 2026-08-07. */}
      <div
        style={{
          display: 'flex',
          flexDirection: 'row',
          gap: 8,
          width: '100%',
          position: 'sticky',
          top: 0,
          zIndex: 1,
          background: 'var(--bg)',
        }}
      >
        <div
          style={{
            flex: openPaneIds.length > 0 ? '1 1 55%' : '1 1 100%',
            minWidth: 0,
          }}
        >
          <MiddleZone
            contextKey="harness-placeholder"
            profile={DEFAULT_BROWSING_PROFILE}
            isGenerating={isGenerating}
            contextPane={
              <div>
                <h2>{t('middleZoneHarness.contextPaneLabel')}</h2>
                <p>{t('middleZoneHarness.contextPaneBody')}</p>
              </div>
            }
            chatPane={
              <div>
                <h2>{t('middleZoneHarness.chatPaneLabel')}</h2>
                <p>{t('middleZoneHarness.chatPaneBody')}</p>
                <input type="text" aria-label={t('middleZoneHarness.chatPaneLabel')} />
              </div>
            }
          />
        </div>
        <div
          ref={paneDockRef}
          style={{
            // No CSS transition here (removed 2026-08-07): animating
            // flex-basis made ResizeObserver fire repeatedly on
            // intermediate, not-yet-final sizes during the 220ms animation.
            // Combined with sync_to's own guard against fighting an
            // externally-resized pane window, each pane could freeze at
            // whatever mid-animation size happened to land right before the
            // guard started rejecting further updates -- found via manual
            // verification (distinct pane sizes, none matching the final
            // layout). The width change is instant now.
            flex: openPaneIds.length > 0 ? '0 0 45%' : '0 0 0%',
            minWidth: 0,
            minHeight: 200,
            border: openPaneIds.length > 0 ? '1px dashed var(--border)' : 'none',
            borderRadius: 8,
            overflow: 'hidden',
          }}
        >
          {openPaneIds.length > 0 && (
            <p style={{ padding: 8, margin: 0, fontSize: 12 }}>
              {t('tier3PaneHarness.dockLabel', { count: openPaneIds.length })}
            </p>
          )}
        </div>
      </div>

      <h2>{t('tier3PaneHarness.heading')}</h2>
      {providerError && (
        <p>{t('tier3PaneHarness.providerError', { message: providerError })}</p>
      )}
      {providers.length === 0 && !providerError && (
        <p>{t('tier3PaneHarness.loadingProviders')}</p>
      )}
      {providers.length > 0 && (
        <Tier3Selector providers={providers} onConfirm={handleConfirm} />
      )}
      {confirmedProviders && (
        <p>
          Confirmed: {confirmedProviders.map((p) => p.name).join(', ')}
        </p>
      )}
      {openError && (
        <p>{t('tier3PaneHarness.openError', { message: openError })}</p>
      )}

      <h3>{t('tier3PaneHarness.openPanesLabel')}</h3>
      {openPaneIds.length === 0 ? (
        <p>{t('tier3PaneHarness.noPanesOpen')}</p>
      ) : (
        <ul>
          {openPaneIds.map((id) => (
            <li key={id}>
              {providers.find((p) => p.id === id)?.name ?? id}{' '}
              <button type="button" onClick={() => handleClose(id)}>
                {t('tier3PaneHarness.closeButton')}
              </button>
            </li>
          ))}
        </ul>
      )}
    </main>
  )
}

export default App
