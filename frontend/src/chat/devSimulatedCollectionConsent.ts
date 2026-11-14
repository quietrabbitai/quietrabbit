// DEV-ONLY simulated collection-consent backend (items.id=920 stage 3).
//
// Why it exists: get_collection_consent_state answers focus_settings_not_found
// for a Focus with no focus_settings row, and Tech Support is not built, so a dev
// build has no row for tech-support (and no UI creates one). Without this the
// confirm and the withdrawal screen could never be seen. This FAKES the row and
// the four states: the three commands the UI calls are answered here, in memory,
// by the same rules the backend applies, and nothing reaches the real commands
// or any database. Only the backend is simulated; the components are the real
// ones.
//
// Guards (all must hold, otherwise this returns null and does nothing), the
// devSimulatedCopyFlag.ts precedent:
//   1. The call site wraps the call in the literal `import.meta.env.DEV`, which
//      Vite replaces with `false` in a production bundle, so the call and this
//      module are dead code and tree-shaken away. It cannot ship.
//   1b. The same env check again inside, as defense in depth.
//   2. localStorage['qr.devSimulateCollectionConsent'] holds one of the MODES
//      below: off by default, set by hand, never by app code.
// The `?.` on env is because Node's test runner (no Vite) has no
// import.meta.env at all.
//
// Removal: delete this file, its test, and the call in collectionConsentApi.ts
// once Tech Support exists with a real focus_settings row.
import type { CollectionConsent, CollectionConsentState } from '../bindings.ts'

export const DEV_SIMULATE_COLLECTION_CONSENT_KEY = 'qr.devSimulateCollectionConsent'

export const SIMULATED_PROMISES = [
  'collects_listed_categories_only',
  'never_shared_outside_qr_without_review',
  'per_session_opt_out',
  'withdraw_any_time',
  'delete_device_and_linked_data',
]

/** not_asked        the first open: the confirm shows
 *  stale_granted    granted for text v1, the text is now v2: asked again
 *  stale_declined   declined for text v1, the text is now v2: asked again
 *  declined         declined at the current text: not asked, memory settings
 *  granted          granted at the current text: not asked, can withdraw
 *  withdrawn        withdrawn: not asked again */
export const SIMULATED_MODES = [
  'not_asked',
  'stale_granted',
  'stale_declined',
  'declined',
  'granted',
  'withdrawn',
] as const
export type SimulatedMode = (typeof SIMULATED_MODES)[number]

type Result<T> = { status: 'ok'; data: T } | { status: 'error'; error: string }

function view(consent: CollectionConsent, stored: number, current: number): CollectionConsentState {
  // The fake backend's own copy of the rule the real one has. The UI never has
  // this: it reads needs_decision.
  const needs =
    consent === 'not_asked' ||
    ((consent === 'granted' || consent === 'declined') && stored !== current)
  return {
    consent,
    stored_version: stored,
    current_text_version: current,
    needs_decision: needs,
    promises: [...SIMULATED_PROMISES],
  }
}

export function initialSimulatedState(mode: SimulatedMode): CollectionConsentState {
  switch (mode) {
    case 'not_asked':
      return view('not_asked', 0, 1)
    case 'stale_granted':
      return view('granted', 1, 2)
    case 'stale_declined':
      return view('declined', 1, 2)
    case 'declined':
      return view('declined', 1, 1)
    case 'granted':
      return view('granted', 1, 1)
    case 'withdrawn':
      return view('withdrawn', 0, 1)
  }
}

/** The in-memory fake. One instance per dev session. */
export class SimulatedCollectionConsent {
  private current: CollectionConsentState

  constructor(mode: SimulatedMode) {
    this.current = initialSimulatedState(mode)
  }

  get(): Result<CollectionConsentState> {
    return { status: 'ok', data: this.current }
  }

  submit(decision: 'granted' | 'declined', version: number): Result<CollectionConsentState> {
    if (version !== this.current.current_text_version) {
      return { status: 'error', error: 'consent_text_stale' }
    }
    if (decision === 'declined' && this.current.consent === 'granted' && !this.current.needs_decision) {
      return { status: 'error', error: 'consent_granted_use_withdraw' }
    }
    this.current = view(decision, version, this.current.current_text_version)
    return { status: 'ok', data: this.current }
  }

  withdraw(): Result<CollectionConsentState> {
    if (this.current.consent !== 'granted' && this.current.consent !== 'withdrawn') {
      return { status: 'error', error: 'no_consent_to_withdraw' }
    }
    this.current = view('withdrawn', 0, this.current.current_text_version)
    return { status: 'ok', data: this.current }
  }
}

let instance: { mode: SimulatedMode; sim: SimulatedCollectionConsent } | null = null

/** Guarded entry point: null unless this is a Vite dev build AND the
 *  localStorage switch holds a mode. The same fake is returned on every call
 *  while the mode is unchanged, so a decision made in one place shows in the
 *  other. */
export function devSimulatedCollectionConsent(): SimulatedCollectionConsent | null {
  // Typed loosely on purpose: this file is also compiled by the Node-typed test
  // project, which has neither Vite's ImportMeta.env nor DOM types.
  const env = (import.meta as { env?: { DEV?: boolean } }).env
  if (!env?.DEV) return null
  try {
    const storage = (globalThis as { localStorage?: { getItem(key: string): string | null } })
      .localStorage
    const mode = storage?.getItem(DEV_SIMULATE_COLLECTION_CONSENT_KEY)
    if (!mode || !(SIMULATED_MODES as readonly string[]).includes(mode)) return null
    if (!instance || instance.mode !== mode) {
      instance = { mode: mode as SimulatedMode, sim: new SimulatedCollectionConsent(mode as SimulatedMode) }
    }
    return instance.sim
  } catch {
    return null
  }
}
