// State for the collection-consent section of ChatPane (items.id=920 stage 3),
// one instance per (Persona, Focus) -- the section is keyed on both:
// reads the state when a consent Focus opens, sends the user's decision and
// withdrawal, and keeps whatever the backend answers. The backend's
// `needs_decision` is the only "ask?" signal (collectionConsent.ts).
import { useCallback, useEffect, useRef, useState } from 'react'
import type { CollectionConsentDecision, CollectionConsentState } from '../bindings'
import { readConsentState, sendConsentDecision, sendConsentWithdrawal } from './collectionConsentApi'
import { consentErrorKind, displayedTextVersion, usesCollectionConsent, type ConsentErrorKind } from './collectionConsent'

export type ConsentActionError =
  | { kind: ConsentErrorKind; message: string }
  /** The withdrawal itself took effect (collection is already off) but the
   *  record of it could not be written: the user presses Stop again to finish. */
  | { kind: 'withdraw-pending'; message: string }

export interface CollectionConsentController {
  /** idle = this Focus does not ask. */
  status: 'idle' | 'loading' | 'ready' | 'error'
  state: CollectionConsentState | null
  loadError: string | null
  busy: boolean
  actionError: ConsentActionError | null
  decide: (decision: CollectionConsentDecision) => Promise<void>
  withdraw: () => Promise<void>
}

export function useCollectionConsent(personaId: string, focusId: string): CollectionConsentController {
  const asks = usesCollectionConsent(focusId)
  const [status, setStatus] = useState<CollectionConsentController['status']>(asks ? 'loading' : 'idle')
  const [state, setState] = useState<CollectionConsentState | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [actionError, setActionError] = useState<ConsentActionError | null>(null)
  // A request token: an answer for a Persona or Focus that is no longer shown
  // is dropped.
  const tokenRef = useRef(0)

  const load = useCallback(() => {
    const token = ++tokenRef.current
    return readConsentState(personaId, focusId).then((result) => {
      if (token !== tokenRef.current) return
      if (result.status === 'ok') {
        setState(result.data)
        setLoadError(null)
        setStatus('ready')
      } else {
        setState(null)
        setLoadError(result.error)
        setStatus('error')
      }
    })
  }, [personaId, focusId])

  // The caller remounts this hook's component (a `key`) when the Persona or the
  // Focus changes, so state never carries over and this only has to fetch.
  useEffect(() => {
    if (usesCollectionConsent(focusId)) void load()
  }, [focusId, load])

  const decide = useCallback(
    async (decision: CollectionConsentDecision) => {
      if (!state) return
      const token = tokenRef.current
      setBusy(true)
      setActionError(null)
      // The version the user was SHOWN, never a freshly read one.
      const result = await sendConsentDecision(personaId, focusId, decision, displayedTextVersion(state))
      if (token !== tokenRef.current) return
      setBusy(false)
      if (result.status === 'ok') {
        setState(result.data)
        return
      }
      const kind = consentErrorKind(result.error)
      setActionError({ kind, message: result.error })
      // The text changed under the user: show the new one and ask again.
      if (kind === 'stale') await load()
    },
    [state, personaId, focusId, load],
  )

  const withdraw = useCallback(async () => {
    const token = tokenRef.current
    setBusy(true)
    setActionError(null)
    const result = await sendConsentWithdrawal(personaId, focusId)
    if (token !== tokenRef.current) return
    setBusy(false)
    if (result.status === 'ok') {
      setState(result.data)
      return
    }
    // An error does not say whether collection already stopped (the flag is
    // cleared BEFORE the record is written). Read the state back: if it now says
    // withdrawn, collection is off and only the record is missing, so the user
    // is told to press Stop again to finish; otherwise nothing changed.
    const after = await readConsentState(personaId, focusId)
    if (token !== tokenRef.current) return
    if (after.status === 'ok') {
      setState(after.data)
      setStatus('ready')
    }
    const pending = after.status === 'ok' && after.data.consent === 'withdrawn'
    setActionError(
      pending
        ? { kind: 'withdraw-pending', message: result.error }
        : { kind: consentErrorKind(result.error), message: result.error },
    )
  }, [personaId, focusId])

  return { status, state, loadError, busy, actionError, decide, withdraw }
}
