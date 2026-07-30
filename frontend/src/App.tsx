import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { commands, type HealthResponse } from './bindings'

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
    </main>
  )
}

export default App
