// items.id=960 unit 2 (GATE-CRITICAL: its controls write the disclosure policy): the
// Sharing block of a fact's panel. A controlled component with no backend call and no
// rule of its own: WHAT shows comes from factSharing.ts (sharingBlock, sharingNotes),
// and a choice goes to MyFactsView's controller (factSharingController.ts) with the
// route's own tier number. A select shows only a value that was read: choosing changes
// nothing on screen until the re-read answers.
//
// A stored ask is not a select value: it is shown as itself, with a button for each
// word that may replace it.

import { useTranslation } from 'react-i18next'
import {
  sharingErrorText,
  sharingNotes,
  sharingWordText,
  type SharingBlock,
  type SharingRoute,
  type SharingWord,
} from './factSharing'
import type { T } from './myFactsFormat'
import './MyFactsView.css'

export interface FactSharingProps {
  block: SharingBlock
  /** The fact's shown label, for the controls' accessible names. */
  fieldText: string
  busy: boolean
  /** The code of this field's last failed write, or null. */
  errorCode: string | null
  onChoose: (tier: SharingRoute['tier'], word: SharingWord) => void
  onRetry: () => void
}

export function FactSharing({
  block,
  fieldText,
  busy,
  errorCode,
  onChoose,
  onRetry,
}: FactSharingProps) {
  const { t } = useTranslation()
  const tt = t as unknown as T
  if (block.kind === 'none') return null
  const notes = sharingNotes(block)

  function routeNames(route: SharingRoute['route']): { name: string; gloss: string } {
    return route === 'anonymous'
      ? {
          name: t('navShell.facts.sharing.route.anonymous'),
          gloss: t('navShell.facts.sharing.route.anonymousGloss'),
        }
      : {
          name: t('navShell.facts.sharing.route.signedIn'),
          gloss: t('navShell.facts.sharing.route.signedInGloss'),
        }
  }

  function routeRow(route: SharingRoute) {
    const { name, gloss } = routeNames(route.route)
    const label = t('navShell.facts.sharing.routeAria', { field: fieldText, route: name })
    const control = route.control
    return (
      <div key={route.route} className="my-facts__route">
        <div className="my-facts__route-name">
          <span>{name}</span>
          <span className="my-facts__muted">{gloss}</span>
        </div>
        {control.kind === 'choose' ? (
          <select
            className="my-facts__route-select"
            aria-label={label}
            value={control.value}
            disabled={busy}
            onChange={(e) => {
              const word = control.options.find((w) => w === e.target.value)
              if (word !== undefined) onChoose(route.tier, word)
            }}
          >
            {control.options.map((word) => (
              <option key={word} value={word}>
                {sharingWordText(tt, word)}
              </option>
            ))}
          </select>
        ) : (
          <fieldset className="my-facts__route-ask" aria-label={label}>
            <p className="my-facts__route-ask-value">{t('navShell.facts.sharing.ask.value')}</p>
            <div className="my-facts__actions">
              <span className="my-facts__muted">
                {t('navShell.facts.sharing.ask.replaceLabel')}
              </span>
              {control.options.map((word) => (
                <button
                  key={word}
                  type="button"
                  className="my-facts__button"
                  disabled={busy}
                  onClick={() => onChoose(route.tier, word)}
                >
                  {sharingWordText(tt, word)}
                </button>
              ))}
            </div>
          </fieldset>
        )}
        {route.route === 'anonymous' && notes.anonymousScope && (
          <p className="my-facts__route-note">{t('navShell.facts.sharing.anonymousScope')}</p>
        )}
      </div>
    )
  }

  return (
    <div className="my-facts__sharing">
      <h4 className="my-facts__sharing-title">{t('navShell.facts.sharing.title')}</h4>
      {block.kind === 'loading' && (
        <p className="my-facts__muted">{t('navShell.facts.sharing.loading')}</p>
      )}
      {block.kind === 'failed' && (
        <>
          <p role="alert" className="my-facts__notice">
            {t('navShell.facts.sharing.failed')}
          </p>
          <div className="my-facts__actions">
            <button type="button" className="my-facts__button" onClick={onRetry}>
              {t('navShell.facts.retry')}
            </button>
          </div>
        </>
      )}
      {block.kind === 'fixed' && (
        <p className="my-facts__route-ask-value">{t('navShell.facts.sharing.fixed')}</p>
      )}
      {block.kind === 'choices' && block.routes.map(routeRow)}
      {block.kind === 'choices' && errorCode !== null && (
        <p role="alert" className="my-facts__notice">
          {sharingErrorText(tt, errorCode)}
        </p>
      )}
      {notes.defaultNote && (
        <p className="my-facts__muted">{t('navShell.facts.sharing.default')}</p>
      )}
      {notes.describeNote && (
        <p className="my-facts__muted">{t('navShell.facts.sharing.describeNote')}</p>
      )}
      {notes.next && <p className="my-facts__muted">{t('navShell.facts.sharing.next')}</p>}
      {notes.limits && <p className="my-facts__muted">{t('navShell.facts.sharing.limits')}</p>}
    </div>
  )
}
