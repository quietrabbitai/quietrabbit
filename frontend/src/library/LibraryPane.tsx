// Real Library output-viewer -- the thing behind NavShell's 'library'
// content branch, replacing the placeholder <p> previously returned by
// NavShell.tsx's describePlaceholder. See that file's former stub comment
// (removed once this landed) for the original wiring intent.

import { useCallback, useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { commands, type OutputInfo } from '../bindings'
import './LibraryPane.css'

export interface LibraryPaneProps {
  userId: string
  /** null when Library was opened via the bare nav button with no Persona
   *  context (content.personaFilter absent) -- see navShellConfig.ts's
   *  ContentDescriptor. Outputs cannot be scoped without one. */
  personaId: string | null
}

export function LibraryPane({ userId, personaId }: LibraryPaneProps) {
  const { t } = useTranslation()
  const [outputs, setOutputs] = useState<OutputInfo[]>([])
  const [listError, setListError] = useState<string | null>(null)
  const [selectedOutputId, setSelectedOutputId] = useState<string | null>(
    null,
  )
  const [selectedOutput, setSelectedOutput] = useState<OutputInfo | null>(
    null,
  )
  const [detailError, setDetailError] = useState<string | null>(null)
  const [copyError, setCopyError] = useState<string | null>(null)
  const [copySucceeded, setCopySucceeded] = useState(false)

  // Re-fetch on mount / identity change, mirrors ChatPane.tsx's own
  // identity-keyed re-fetch effect.
  useEffect(() => {
    setOutputs([])
    setListError(null)
    setSelectedOutputId(null)
    setSelectedOutput(null)
    setDetailError(null)
    setCopyError(null)
    setCopySucceeded(false)

    if (personaId === null) {
      // Nothing was ever really listable for this identity -- honest
      // empty state, not a fetch failure.
      return
    }

    commands.listOutputs(userId, personaId, null, null, null).then(
      (result) => {
        if (result.status === 'ok') {
          setOutputs(result.data)
        } else {
          setListError(result.error)
        }
      },
    )
  }, [userId, personaId])

  // Detail load, keyed on selection -- re-fetches via getOutput rather than
  // trusting the list row's own cached content, matching ChatPane's
  // reconcile-over-cache discipline and giving Privacy Guardian a fresh
  // per-access check point.
  useEffect(() => {
    setSelectedOutput(null)
    setDetailError(null)
    setCopyError(null)
    setCopySucceeded(false)

    if (selectedOutputId === null) return
    if (personaId === null) return // defensive, same guard as the list effect

    commands.getOutput(selectedOutputId, userId, personaId).then(
      (result) => {
        if (result.status === 'ok') {
          setSelectedOutput(result.data)
        } else {
          setDetailError(result.error)
        }
      },
    )
  }, [selectedOutputId, userId, personaId])

  const copyGateBlocked = personaId === null

  const handleCopy = useCallback(() => {
    if (copyGateBlocked || selectedOutput === null || personaId === null) {
      return
    }
    setCopyError(null)
    setCopySucceeded(false)
    commands
      .copyOutputToClipboard(selectedOutput.id, userId, personaId)
      .then((result) => {
        if (result.status === 'ok') {
          setCopySucceeded(true)
        } else {
          // Already the actionable, clipboard-specific message -- rendered
          // verbatim, not wrapped in a generic error template.
          setCopyError(result.error)
        }
      })
  }, [copyGateBlocked, selectedOutput, userId, personaId])

  if (personaId === null) {
    return (
      <div className="library-pane">
        <p className="library-pane__notice">
          {t('navShell.libraryPane.noPersonaContext')}
        </p>
      </div>
    )
  }

  if (selectedOutputId !== null) {
    return (
      <LibraryOutputDetail
        output={selectedOutput}
        detailError={detailError}
        copyError={copyError}
        copySucceeded={copySucceeded}
        copyGateBlocked={copyGateBlocked}
        onCopy={handleCopy}
        onBack={() => setSelectedOutputId(null)}
      />
    )
  }

  return (
    <div className="library-pane">
      <h2 className="library-pane__heading">
        {t('navShell.libraryPane.listHeading')}
      </h2>
      {listError && (
        <p role="alert">
          {t('navShell.libraryPane.listLoadError', { message: listError })}
        </p>
      )}
      {outputs.length === 0 && !listError && (
        <p>{t('navShell.libraryPane.emptyList')}</p>
      )}
      {outputs.length > 0 && (
        <ul className="library-pane__list">
          {outputs.map((output) => (
            <li key={output.id} className="library-pane__list-item">
              <button
                type="button"
                onClick={() => setSelectedOutputId(output.id)}
              >
                {t('navShell.libraryPane.itemLabel', {
                  type: output.output_type,
                  date: output.created_at,
                })}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  )
}

interface LibraryOutputDetailProps {
  output: OutputInfo | null
  detailError: string | null
  copyError: string | null
  copySucceeded: boolean
  copyGateBlocked: boolean
  onCopy: () => void
  onBack: () => void
}

function LibraryOutputDetail({
  output,
  detailError,
  copyError,
  copySucceeded,
  copyGateBlocked,
  onCopy,
  onBack,
}: LibraryOutputDetailProps) {
  const { t } = useTranslation()
  return (
    <div className="library-pane library-pane__detail">
      <button type="button" onClick={onBack}>
        {t('navShell.libraryPane.backButton')}
      </button>
      {detailError && (
        <p role="alert">
          {t('navShell.libraryPane.detailLoadError', {
            message: detailError,
          })}
        </p>
      )}
      {output && (
        <>
          <h2>
            {t('navShell.libraryPane.itemLabel', {
              type: output.output_type,
              date: output.created_at,
            })}
          </h2>
          <pre className="library-pane__content">{output.content}</pre>
          <button type="button" onClick={onCopy} disabled={copyGateBlocked}>
            {t('navShell.libraryPane.copyButton')}
          </button>
          {copyGateBlocked && (
            <p className="library-pane__notice">
              {t('navShell.libraryPane.notAvailable')}
            </p>
          )}
          {copySucceeded && <p>{t('navShell.libraryPane.copySuccess')}</p>}
          {copyError && (
            <p role="alert" className="library-pane__copy-error">
              {copyError}
            </p>
          )}
        </>
      )}
    </div>
  )
}
