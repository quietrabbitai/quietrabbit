import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { commands, type HealthResponse } from './bindings'
import { MiddleZone } from './middleZone/MiddleZone'
import { DEFAULT_BROWSING_PROFILE } from './middleZone/middleZoneConfig'
import { Tier3Selector } from './tier3Access/Tier3Selector'
import {
  PLACEHOLDER_PROVIDERS,
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

  // TEMPORARY HARNESS (items.id=3, 2026-07-31): a minimal, throwaway host
  // for MiddleZone and Tier3Selector -- NOT the real top strip /
  // navigation shell (IA spec Section 2) or the outbound Privacy
  // Guardian gate that must precede the selector in the real flow,
  // both separate, undispatched/existing-locked scope. This exists
  // only to prove each mechanism mounts and behaves correctly in
  // isolation; it should be deleted, not extended, once the real
  // navigation shell and gate flow land. Do not build on top of this
  // harness.
  const [isGenerating, setIsGenerating] = useState(false)
  const [confirmedProviders, setConfirmedProviders] = useState<
    Provider[] | null
  >(null)

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

      <h2>Tier3Selector harness</h2>
      <Tier3Selector
        providers={PLACEHOLDER_PROVIDERS}
        onConfirm={(selected) => setConfirmedProviders(selected)}
      />
      {confirmedProviders && (
        <p>
          Confirmed: {confirmedProviders.map((p) => p.name).join(', ')}
        </p>
      )}
    </main>
  )
}

export default App
