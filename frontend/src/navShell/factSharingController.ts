// items.id=960 unit 2 (GATE-CRITICAL: it writes the disclosure policy): the
// write-then-re-read controller of the Sharing blocks, pure and React-free so it is
// tested with fake commands (the repo has no DOM test runner). MyFactsView owns one
// per Persona and passes it the two commands.
//
// RULES:
// - NO OPTIMISTIC VALUE. What is shown only ever comes from a read. A write changes
//   nothing on screen until the re-read answers.
// - ALWAYS RE-READ after a write, whatever the write answered: set_fact_policy can
//   fail AFTER it stored the choice (its own read-back is part of it). The entry the
//   write returns is not used.
// - A RE-READ THAT FAILS drops the old values: the state is failed, with no control.
//   Nothing stale is left looking current.
// - ONE WRITE AT A TIME, for every field of the Persona: a second choice while one is
//   in flight does nothing, so two re-reads can never cross.
// - Only the newest read is applied (a retry or a reload may overlap a write).
// - After dispose() nothing is emitted: a late answer never reaches another view.
// - A rejected call reads as fact_policy_unavailable, never as its own text.

// '../bindings.ts' with the extension: see frictionGateDetail.ts on TS2835.
import type { FactPolicyInfo } from '../bindings.ts'
import type { PoliciesLoad, SharingWord } from './factSharing.ts'

type Answer<T> = { status: 'ok'; data: T } | { status: 'error'; error: string }

/** The two commands, already bound to one Persona. */
export interface SharingCommands {
  get: () => Promise<Answer<FactPolicyInfo[]>>
  set: (fieldName: string, tier: 2 | 3, policy: SharingWord) => Promise<Answer<unknown>>
}

export interface SharingState {
  load: PoliciesLoad
  /** A write is in flight: every control is disabled. */
  busy: boolean
  /** The last write that was refused or failed, for its field. */
  writeError: { field: string; code: string } | null
}

export const INITIAL_SHARING: SharingState = {
  load: { kind: 'loading' },
  busy: false,
  writeError: null,
}

const UNAVAILABLE = 'fact_policy_unavailable'

export interface SharingController {
  /** Read everything again (first load, retry, and after the facts changed). */
  load: () => Promise<void>
  /** Store one route's choice, then read again. */
  choose: (fieldName: string, tier: 2 | 3, policy: SharingWord) => Promise<void>
  clearError: () => void
  dispose: () => void
}

export function createSharingController(
  commands: SharingCommands,
  emit: (state: SharingState) => void,
): SharingController {
  let state = INITIAL_SHARING
  let latestRead = 0
  let disposed = false

  function update(next: Partial<SharingState>) {
    if (disposed) return
    state = { ...state, ...next }
    emit(state)
  }

  /** One read. Its result is applied only if no later read has started. */
  async function read(): Promise<void> {
    const mine = ++latestRead
    let result: PoliciesLoad
    try {
      const answer = await commands.get()
      result =
        answer.status === 'ok'
          ? { kind: 'ready', policies: answer.data }
          : { kind: 'failed', code: answer.error }
    } catch {
      result = { kind: 'failed', code: UNAVAILABLE }
    }
    if (mine === latestRead) update({ load: result })
  }

  return {
    async load() {
      update({ load: { kind: 'loading' }, writeError: null })
      await read()
    },
    async choose(fieldName, tier, policy) {
      // Controls exist only over a ready read.
      if (disposed || state.busy || state.load.kind !== 'ready') return
      update({ busy: true, writeError: null })
      let code: string | null
      try {
        const answer = await commands.set(fieldName, tier, policy)
        code = answer.status === 'ok' ? null : answer.error
      } catch {
        code = UNAVAILABLE
      }
      await read()
      update({
        busy: false,
        writeError: code === null ? null : { field: fieldName, code },
      })
    },
    clearError() {
      if (state.writeError !== null) update({ writeError: null })
    },
    dispose() {
      disposed = true
    },
  }
}
