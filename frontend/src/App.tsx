import { commands } from './bindings'

function App() {
  // Smoke-test only: confirms the generated bindings module resolves and
  // loads at runtime, not just under tsc. No real IPC call yet -- that's
  // the next step, once this renders correctly.
  const bindingsLoaded = typeof commands === 'object'

  return (
    <main>
      <h1>Quiet Rabbit</h1>
      <p>Frontend scaffold running. Bindings loaded: {String(bindingsLoaded)}</p>
    </main>
  )
}

export default App
