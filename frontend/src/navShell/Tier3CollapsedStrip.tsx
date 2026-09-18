// Cloud Chat's own collapsed bar -- items.id=384 slice 4, decisions.id=738.
// Shown in place of the rail+content-pane whenever Cloud Chat is NOT the
// expanded region. Per the reference mockup's tier3-floor element:
// click-to-expand only, NO live entry field -- that's the resolved answer
// to decisions.id=738's flagged "does Tier 3's side get the same treatment
// as QR's collapsed floor" question (no, by design: QR's collapsed floor
// keeps a real entry field because it's QR's OWN conversation; a
// cloud_frontier provider's page has no equivalent QR-owned input to keep live).
//
// Deliberately does NOT show a text snippet of the provider's own last
// response (unlike the mockup's illustrative "Claude: '...still
// responding'" flavor text) -- this component has no access to that
// content. Tier3AccessPane never reads inside a provider's CEF-rendered
// page; showing a fabricated snippet here would be inventing data that
// doesn't exist, not reflecting real state.
//
// items.id=391 (Jason, 2026-09-02, tenth pass -- the "three bars, one
// expanded" redesign): this used to return null whenever no provider pane
// was open and no draft had been Gate3-approved yet, and only otherwise
// showed a generic "second opinion ready" bar with no way to actually
// START a review. Both gaps close here: this bar is now ALWAYS rendered
// whenever Cloud Chat isn't the expanded region (never null), one row among
// the three peer bars (Board / Chat / Second opinion) WorkspaceShell.tsx
// and Tier3AccessPane.tsx stack together -- Jason's own framing: "a second
// opinion bar always visible." Tier3AccessPane now derives which of three
// states applies whenever openProviderIds is empty (no provider pane open
// this round):
//   'approved'   -- a draft already cleared Gate3, waiting on the rail.
//                   onExpandRail (markTier3Ready) brings the rail back,
//                   same as before this pass.
//   'reviewable' -- there's a real last assistant response that hasn't
//                   been sent through Gate3 review yet. onReview starts
//                   that review -- this replaces the old chat-toolbar "2nd
//                   opinion" button (removed, Tier3AccessPane.tsx), which
//                   is now redundant with this always-visible bar doing
//                   the same job.
//   'none'       -- no assistant response at all yet. Genuinely nothing to
//                   act on -- rendered as a plain, non-interactive row
//                   rather than a dead-feeling button.

import { useTranslation } from 'react-i18next'
import type { Provider } from '../tier3Access/tier3AccessConfig'
import './Tier3CollapsedStrip.css'

export interface Tier3CollapsedStripProps {
  providers: Provider[]
  openProviderIds: string[]
  /** The provider to feature (its name shown on the strip) and the one
   *  `onExpand` reactivates -- the most recently active one, or the last
   *  loaded provider if none was ever activated this session. */
  activeProviderId: string | null
  onExpand: (providerId: string) => void
  /** Only consulted when openProviderIds is empty -- see this file's
   *  header comment for what each value means. */
  emptyState: 'approved' | 'reviewable' | 'none'
  /** Fires when clicked in the 'approved' empty state -- brings back the
   *  rail itself (markTier3Ready), not a specific provider. */
  onExpandRail: () => void
  /** Fires when clicked in the 'reviewable' empty state -- starts a real
   *  Gate3 review of the current last response (Tier3AccessPane's
   *  handleSecondOpinion). */
  onReview: () => void
  /** True while a Gate3 review is already in flight (reviewOutcome ===
   *  'pending') -- disables the 'reviewable' bar for the same reason the
   *  old toolbar button used to disable itself in that state. */
  reviewDisabled?: boolean
}

export function Tier3CollapsedStrip({
  providers,
  openProviderIds,
  activeProviderId,
  onExpand,
  emptyState,
  onExpandRail,
  onReview,
  reviewDisabled = false,
}: Tier3CollapsedStripProps) {
  const { t } = useTranslation()

  if (openProviderIds.length === 0) {
    if (emptyState === 'none') {
      return (
        <div className="tier3-collapsed-strip tier3-collapsed-strip--empty">
          <span className="tier3-collapsed-strip__name">
            {t('navShell.tier3CollapsedStrip.emptyLabel')}
          </span>
        </div>
      )
    }
    if (emptyState === 'approved') {
      return (
        <button
          type="button"
          className="tier3-collapsed-strip"
          onClick={() => onExpandRail()}
        >
          <span className="tier3-collapsed-strip__name">
            {t('navShell.tier3CollapsedStrip.readyLabel')}
          </span>
          <span className="tier3-collapsed-strip__expand">
            {t('navShell.tier3CollapsedStrip.expandLabel')}
          </span>
        </button>
      )
    }
    return (
      <button
        type="button"
        className="tier3-collapsed-strip"
        onClick={() => onReview()}
        disabled={reviewDisabled}
      >
        <span className="tier3-collapsed-strip__name">
          {t('navShell.tier3AccessPane.secondOpinionButton')}
        </span>
        <span className="tier3-collapsed-strip__expand">
          {t('navShell.tier3CollapsedStrip.expandLabel')}
        </span>
      </button>
    )
  }

  const featuredId = activeProviderId ?? openProviderIds[openProviderIds.length - 1]
  const featured = providers.find((p) => p.id === featuredId)
  const extraCount = openProviderIds.length - 1

  return (
    <button
      type="button"
      className="tier3-collapsed-strip"
      onClick={() => onExpand(featuredId)}
    >
      <span className="tier3-collapsed-strip__name">
        {featured?.name ?? null}
      </span>
      <span className="tier3-collapsed-strip__expand">
        {t('navShell.tier3CollapsedStrip.expandLabel')}
      </span>
      {extraCount > 0 && (
        <span className="tier3-collapsed-strip__extra">
          {t('navShell.tier3CollapsedStrip.extraCount', { count: extraCount })}
        </span>
      )}
    </button>
  )
}
