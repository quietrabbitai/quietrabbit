// Tier 3's own collapsed floor -- items.id=384 slice 4, decisions.id=738.
// Shown in place of the rail+content-pane when Chat is dominant and at
// least one provider is still loaded. Per the reference mockup's
// tier3-floor element: click-to-expand only, NO live entry field --
// that's the resolved answer to decisions.id=738's flagged "does Tier 3's
// side get the same treatment as QR's collapsed floor" question (no, by
// design: QR's collapsed floor keeps a real entry field because it's
// QR's OWN conversation; a Tier 3 provider's page has no equivalent
// QR-owned input to keep live).
//
// Deliberately does NOT show a text snippet of the provider's own last
// response (unlike the mockup's illustrative "Claude: '...still
// responding'" flavor text) -- this component has no access to that
// content. Tier3AccessPane never reads inside a provider's CEF-rendered
// page; showing a fabricated snippet here would be inventing data that
// doesn't exist, not reflecting real state.

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
}

export function Tier3CollapsedStrip({
  providers,
  openProviderIds,
  activeProviderId,
  onExpand,
}: Tier3CollapsedStripProps) {
  const { t } = useTranslation()

  if (openProviderIds.length === 0) return null

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
        {featured?.name ?? featuredId}
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
