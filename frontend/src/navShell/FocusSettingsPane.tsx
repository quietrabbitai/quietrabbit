// items.id=321 -- Focus settings screen. Thin wrapper: heading, read-only
// context for the fields this pass doesn't expose controls for
// (context_flow/library_visibility/focus_profile -- out of scope, see
// decisions.id=729), and the real editable controls via
// FocusSettingsControls (mode="full", shared with Tier3AccessPane's inline
// "raise it now" affordance so the friction-gate-confirm logic exists once).

import { useTranslation } from 'react-i18next'
import { FocusSettingsControls } from './FocusSettingsControls'

export interface FocusSettingsPaneProps {
  userId: string
  personaId: string
  focusId: string
}

export function FocusSettingsPane({ userId, personaId, focusId }: FocusSettingsPaneProps) {
  const { t } = useTranslation()

  return (
    <div className="focus-settings-pane">
      <h2>{t('navShell.focusSettings.heading', { focusId })}</h2>
      <FocusSettingsControls
        userId={userId}
        personaId={personaId}
        focusId={focusId}
        mode="full"
      />
    </div>
  )
}
