import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { commands, type HealthResponse } from './bindings'
import { MiddleZone } from './middleZone/MiddleZone'
import { DEFAULT_BROWSING_PROFILE } from './middleZone/middleZoneConfig'
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
  // Open panes are separate, OS-level windows (sync_window.rs) synced
  // beside the Tauri window -- there is no DOM element to host them in,
  // so this harness can only show which ones are open, not render them
  // inline. `openPaneIds` mirrors the pane manager's own state via
  // openTier3Panes'/closeTier3Pane's own success, not a live push
  // subscription -- adequate for this harness, not the real integration.
  const [openPaneIds, setOpenPaneIds] = useState<string[]>([])
  const [openError, setOpenError] = useState<string | null>(null)

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
