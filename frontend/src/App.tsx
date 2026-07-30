import { useEffect, useState } from 'react'
import { commands, type HealthResponse } from './bindings'

function App() {
  // First real IPC round-trip (items.id=3): calls the read-only, no-argument
  // get_health command to verify the whole pipe -- frontend -> Tauri IPC ->
  // Rust command -> typed response -- actually works, not just that
  // bindings.ts loads. Only verifiable under `tauri dev`, not plain
  // `vite dev`, since IPC does not exist outside a real Tauri window.
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
      <h1>Quiet Rabbit</h1>
      {error && <p>IPC error: {error}</p>}
      {health && (
        <dl>
          <dt>Ollama status</dt>
          <dd>{health.ollama.status}</dd>
          <dt>Ollama source</dt>
          <dd>{health.ollama_source}</dd>
          <dt>Tier 2 configured</dt>
          <dd>{String(health.tier2_configured)}</dd>
        </dl>
      )}
      {!health && !error && <p>Loading health check…</p>}
    </main>
  )
}

export default App
