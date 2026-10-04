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
// content. CloudChatAccessPane never reads inside a provider's CEF-rendered
// page; showing a fabricated snippet here would be inventing data that
// doesn't exist, not reflecting real state.
//
// items.id=391 (Jason, 2026-09-02, tenth pass -- the "three bars, one
// expanded" redesign): this bar is ALWAYS rendered whenever Cloud Chat isn't
// the expanded region (never null), one row among the three peer bars
// (Board / Chat / Cloud Chat) WorkspaceShell.tsx and CloudChatAccessPane.tsx
// stack together.
//
// items.id=501 slice 1 (decisions.id=846, 811): it is also ALWAYS CLICKABLE
// and named "Cloud Chat". With no provider pane open, a click enters Cloud
// Chat directly (onExpandRail: the provider list, nothing sent) -- no
// message, persona or review is needed. The old "Second opinion ready" /
// "No second opinion yet" states and the review-on-click branch are gone.

import { useTranslation } from 'react-i18next'
import type { Provider } from '../cloudChatAccess/cloudChatAccessConfig'
import './CloudChatCollapsedStrip.css'

export interface CloudChatCollapsedStripProps {
  providers: Provider[]
  openProviderIds: string[]
  /** The provider to feature (its name shown on the strip) and the one
   *  `onExpand` reactivates -- the most recently active one, or the last
   *  loaded provider if none was ever activated this session. */
  activeProviderId: string | null
  onExpand: (providerId: string) => void
  /** Fires on click when no provider pane is open -- enters Cloud Chat
   *  (CloudChatAccessPane's enterCloudChat), not a specific provider. */
  onExpandRail: () => void
}

export function CloudChatCollapsedStrip({
  providers,
  openProviderIds,
  activeProviderId,
  onExpand,
  onExpandRail,
}: CloudChatCollapsedStripProps) {
  const { t } = useTranslation()

  if (openProviderIds.length === 0) {
    return (
      <button type="button" className="cloud-chat-collapsed-strip" onClick={() => onExpandRail()}>
        <span className="cloud-chat-collapsed-strip__name">
          {t('navShell.cloudChatAccessPane.heading')}
        </span>
        <span className="cloud-chat-collapsed-strip__expand">
          {t('navShell.cloudChatCollapsedStrip.expandLabel')}
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
      className="cloud-chat-collapsed-strip"
      onClick={() => onExpand(featuredId)}
    >
      <span className="cloud-chat-collapsed-strip__name">
        {featured?.name ?? null}
      </span>
      <span className="cloud-chat-collapsed-strip__expand">
        {t('navShell.cloudChatCollapsedStrip.expandLabel')}
      </span>
      {extraCount > 0 && (
        <span className="cloud-chat-collapsed-strip__extra">
          {t('navShell.cloudChatCollapsedStrip.extraCount', { count: extraCount })}
        </span>
      )}
    </button>
  )
}
