// The QR Chat <-> Tier 3 dominance pair's state and activate/close/reclaim
// actions -- items.id=384 slice 4 (decisions.id=735). Extracted from
// Tier3AccessPane.tsx's own local useState so the pair survives
// Tier3AccessPane unmounting: WorkspaceShell.tsx (slice 3) unmounts it
// entirely when Board is 'full', by design (its own header comment
// explains why a real unmount, not CSS-hiding, is required for the CEF
// pane lifecycle below) -- so the pair's state can no longer live in that
// component's own useState the way it used to.
//
// This is a CONTROLLED hook, not a self-contained one: `pair` and
// `setPair` come from NavState.workspace.pair (navShellConfig.ts), owned
// one level up in NavShell.tsx, which never unmounts. `setPair` takes a
// functional updater (not a plain value) for the same reason the
// pre-extraction code's setOpenPaneIds/setActiveProviderId calls did:
// openTier3Panes/setActivePane/closeTier3Pane are async Rust commands,
// and computing the next state against whatever `pair` was at CALL time
// (rather than at RESOLUTION time) would silently reintroduce exactly the
// stale-closure race the original functional-setState calls were already
// guarding against.
//
// `dominant` is an EXPLICITLY set field, not derived from
// activeProviderId !== null -- an earlier version of this hook derived it
// that way and shipped a real regression: pre-extraction, the rail
// (Tier3Selector) was visible whenever reviewOutcome === 'approved', full
// stop, independent of whether any specific provider had been activated
// yet or since closed ("the rail is persistent once the gate clears...
// it does not disappear once a pane opens," items.id=359's own comment,
// carried into Tier3AccessPane.tsx unchanged). Deriving dominant purely
// from activeProviderId made the rail vanish the instant activeProviderId
// went back to null (right after Gate3 first approves, before the user
// has clicked anything; or after closing whichever pane was active) --
// exactly the cases the pre-merge design deliberately kept the rail
// visible for. The four real triggers for a dominant change are:
//   - Gate3 approves a draft for the first time this "round" -> markTier3Ready()
//   - activate(providerId) -> 'tier3' (redundant with the above in
//     practice, set directly anyway rather than relying on ordering)
//   - reclaimChat() -> 'chat' (the only user-driven path back to chat)
//   - close(providerId) does NOT touch dominant at all -- matches "the
//     rail persists" exactly: closing the active pane clears
//     activeProviderId (content-pane goes back to its empty prompt) but
//     the rail itself stays visible.
//
// Gate3 review state (reviewOutcome/consentPayload/pendingMessageId/etc.)
// stays local to Tier3AccessPane, entirely untouched by this extraction --
// it's per-in-flight-message state, not part of "which side is dominant."
// markTier3Ready() is the one narrow exception: Tier3AccessPane calls it
// from an effect watching reviewOutcome, but this hook still has no idea
// what Gate3 review even is -- it just exposes a plain "make tier3
// dominant" action, same shape as reclaimChat's "make chat dominant."

import { useCallback, useState } from 'react'
import { commands } from '../bindings'
import type { DominancePairState } from './navShellConfig'

export interface DominancePairHandle {
  dominant: 'chat' | 'tier3'
  openProviderIds: string[]
  activeProviderId: string | null
  openError: string | null
  /** Exposed (not just openError itself) so Tier3AccessPane can surface
   *  its OWN failures -- syncPaneLayout's setPaneLayout call, the
   *  dev-only force-escalation scaffolding -- through this same shared
   *  error slot, exactly as the pre-extraction code's single local
   *  openError state already did for all of these together. Splitting
   *  them into separate error surfaces per source would be a visible UX
   *  change (multiple error paragraphs where there used to be one) this
   *  refactor isn't meant to make. */
  setOpenError: (error: string | null) => void
  /** Idle row -> load then activate in one step; loaded row -> just
   *  switch the content pane (no reload) -- same behavior as the
   *  pre-extraction handleActivate. */
  activate: (providerId: string) => void
  /** onClosed fires after a successful close, before this function
   *  returns control -- Tier3AccessPane uses it to clear its own local
   *  popupRects entry for the closed provider (items.id=234's popup
   *  bookkeeping stays local to that component, not this hook's
   *  concern; see this hook's header comment on scope). Does NOT change
   *  `dominant` -- see this file's header comment on why the rail must
   *  stay visible after closing the active pane. */
  close: (providerId: string, onClosed?: () => void) => void
  /** decisions.id=735's symmetric transition: reclaims Chat dominance
   *  without closing anything -- same behavior as the pre-extraction
   *  handleExpandQR. */
  reclaimChat: () => void
  /** Makes Tier 3 dominant without activating any specific provider --
   *  the "Gate3 just approved a draft" trigger. A no-op (same object
   *  reference, no re-render) if already dominant, so Tier3AccessPane's
   *  effect can call this every time reviewOutcome is 'approved' without
   *  worrying about redundant updates. */
  markTier3Ready: () => void
}

export function useDominancePair(
  pair: DominancePairState,
  setPair: (updater: (prev: DominancePairState) => DominancePairState) => void,
): DominancePairHandle {
  const [openError, setOpenError] = useState<string | null>(null)

  const activate = useCallback(
    (providerId: string) => {
      setOpenError(null)
      if (pair.openProviderIds.includes(providerId)) {
        setPair((prev) => ({ ...prev, dominant: 'tier3', activeProviderId: providerId }))
        commands.setActivePane(providerId).then((result) => {
          if (result.status !== 'ok') setOpenError(result.error)
        })
        return
      }
      commands.openTier3Panes([providerId]).then((result) => {
        if (result.status !== 'ok') {
          setOpenError(result.error)
          return
        }
        setPair((prev) => ({
          ...prev,
          dominant: 'tier3',
          openProviderIds: prev.openProviderIds.includes(providerId)
            ? prev.openProviderIds
            : [...prev.openProviderIds, providerId],
          activeProviderId: providerId,
        }))
        commands.setActivePane(providerId).then((setResult) => {
          if (setResult.status !== 'ok') setOpenError(setResult.error)
        })
      })
    },
    [pair.openProviderIds, setPair],
  )

  const close = useCallback(
    (providerId: string, onClosed?: () => void) => {
      commands.closeTier3Pane(providerId).then((result) => {
        if (result.status !== 'ok') {
          setOpenError(result.error)
          return
        }
        setPair((prev) => ({
          ...prev,
          openProviderIds: prev.openProviderIds.filter((id) => id !== providerId),
          activeProviderId: prev.activeProviderId === providerId ? null : prev.activeProviderId,
        }))
        onClosed?.()
      })
    },
    [setPair],
  )

  const reclaimChat = useCallback(() => {
    setPair((prev) => ({ ...prev, dominant: 'chat', activeProviderId: null }))
    commands.setActivePane(null).then((result) => {
      if (result.status !== 'ok') setOpenError(result.error)
    })
  }, [setPair])

  const markTier3Ready = useCallback(() => {
    setPair((prev) => (prev.dominant === 'tier3' ? prev : { ...prev, dominant: 'tier3' }))
  }, [setPair])

  return {
    dominant: pair.dominant,
    openProviderIds: pair.openProviderIds,
    activeProviderId: pair.activeProviderId,
    openError,
    setOpenError,
    activate,
    close,
    reclaimChat,
    markTier3Ready,
  }
}
