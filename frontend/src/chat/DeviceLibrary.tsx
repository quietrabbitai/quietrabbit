// The Tech Support device library screen (items.id=891 stage 7, decisions.id=509,
// 520, 944): the devices Tech Support keeps about the user's equipment, what it
// recorded about each, and the ways the user can correct or delete it. Opened from
// the small "Devices" entry in the Tech Support chat pane
// (CollectionConsentSection), next to the memory entry.
//
//   list          every device (name, kind, how many facts and open tickets)
//   device sheet  READ-ONLY view of what is recorded: facts in the backend's own
//                 order with their labels, the tickets and the configuration
//                 changes. Facts can be edited, cleared or added; the tickets and
//                 changes are shown, not edited.
//   rename        one name field
//   delete        a two-step purge: the backend counts what would go, the user
//                 confirms, and only then is it deleted
//
// Devices are named by their E- handle everywhere; no internal id is shown or
// sent. NOT here: archiving, adding a device by hand, any visibility control.
//
// THE DELETE CONFIRMATION SAYS PLAINLY what it deletes: what QuietRabbit recorded
// about the device (its facts, tickets and configuration changes). It does NOT say
// or imply that chat transcripts, earlier replies or the consent record are
// deleted, and it says they are not (deviceLibrary.test.ts pins both). The copy is
// PLACEHOLDER, owned by Chat-BRAND (items.id=839).
import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import type { TFunction } from 'i18next'
import { isRetryable } from './deviceLibrary'
import { useDeviceLibrary, type DeviceActionError } from './useDeviceLibrary'
import './DeviceLibrary.css'

export interface DeviceLibraryProps {
  personaId: string
  focusId: string
  onClose: () => void
}

/** Plain-language copy for each error kind. A retryable one says to try again. */
function errorText(t: TFunction, error: DeviceActionError): string {
  switch (error.kind) {
    case 'not_found':
      return t('navShell.chat.devices.error.notFound')
    case 'name_in_use':
      return t('navShell.chat.devices.error.nameInUse')
    case 'invalid_name':
      return t('navShell.chat.devices.error.invalidName')
    case 'unknown_field':
      return t('navShell.chat.devices.error.unknownField')
    case 'invalid_value':
      return t('navShell.chat.devices.error.invalidValue')
    case 'runs_running':
      return t('navShell.chat.devices.error.runsRunning')
    case 'unavailable':
      return t('navShell.chat.devices.error.unavailable')
    case 'other':
      return t('navShell.chat.devices.error.other', { message: error.message })
  }
}

export function DeviceLibrary({ personaId, focusId, onClose }: DeviceLibraryProps) {
  const { t } = useTranslation()
  const lib = useDeviceLibrary(personaId, focusId)
  const [renaming, setRenaming] = useState(false)
  const [renameDraft, setRenameDraft] = useState('')
  const [editingField, setEditingField] = useState<string | null>(null)
  const [editDraft, setEditDraft] = useState('')
  const [addField, setAddField] = useState('')
  const [addValue, setAddValue] = useState('')

  const sheet = lib.sheet
  const error = lib.actionError

  return (
    <section className="device-library" aria-labelledby="device-library-heading">
      <h3 id="device-library-heading" className="device-library__heading">
        {t('navShell.chat.devices.heading')}
      </h3>

      {lib.purged && (
        <output className="device-library__purged">
          {t('navShell.chat.devices.purged', {
            facts: lib.purged.facts,
            cases: lib.purged.cases,
            changes: lib.purged.changes,
          })}
          <button type="button" onClick={lib.dismissPurged}>
            {t('navShell.chat.devices.dismiss')}
          </button>
        </output>
      )}

      {error && (
        <p role="alert" className="device-library__error">
          {errorText(t, error)}
        </p>
      )}

      {lib.selected === null && (
        <>
          {lib.listStatus === 'loading' && (
            <p className="device-library__note">{t('navShell.chat.devices.loading')}</p>
          )}
          {lib.listStatus === 'error' && (
            <p role="alert" className="device-library__error">
              {t('navShell.chat.devices.loadError', { message: lib.listError ?? '' })}
            </p>
          )}
          {lib.listStatus === 'ready' && lib.devices.length === 0 && (
            <p className="device-library__note">{t('navShell.chat.devices.empty')}</p>
          )}
          {lib.listStatus === 'ready' && lib.devices.length > 0 && (
            <ul className="device-library__list">
              {lib.devices.map((d) => (
                <li key={d.handle} className="device-library__item">
                  <button
                    type="button"
                    className="device-library__open"
                    onClick={() => lib.open(d.handle)}
                  >
                    <span className="device-library__name">{d.display_name}</span>
                    {d.subject_class && (
                      <span className="device-library__kind">{d.subject_class}</span>
                    )}
                    <span className="device-library__counts">
                      {t('navShell.chat.devices.counts', {
                        facts: d.fact_count,
                        open: d.open_case_count,
                      })}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </>
      )}

      {lib.selected !== null && (
        <>
          {lib.sheetStatus === 'loading' && (
            <p className="device-library__note">{t('navShell.chat.devices.loading')}</p>
          )}
          {lib.sheetStatus === 'error' && (
            <p role="alert" className="device-library__error">
              {t('navShell.chat.devices.sheetError', { message: lib.sheetError ?? '' })}
            </p>
          )}
          {sheet && (
            <div className="device-library__sheet">
              <div className="device-library__title-row">
                <h4 className="device-library__device-name">{sheet.device.display_name}</h4>
                {!renaming && (
                  <button
                    type="button"
                    disabled={lib.busy}
                    onClick={() => {
                      setRenameDraft(sheet.device.display_name)
                      setRenaming(true)
                    }}
                  >
                    {t('navShell.chat.devices.rename')}
                  </button>
                )}
              </div>
              {renaming && (
                <form
                  className="device-library__form"
                  onSubmit={(e) => {
                    e.preventDefault()
                    void lib.rename(renameDraft).then((done) => {
                      if (done) setRenaming(false)
                    })
                  }}
                >
                  <label className="device-library__label" htmlFor="device-library-rename">
                    {t('navShell.chat.devices.renameLabel')}
                  </label>
                  <input
                    id="device-library-rename"
                    className="device-library__input"
                    value={renameDraft}
                    maxLength={120}
                    onChange={(e) => setRenameDraft(e.target.value)}
                  />
                  <button type="submit" disabled={lib.busy || renameDraft.trim() === ''}>
                    {t('navShell.chat.devices.save')}
                  </button>
                  <button type="button" onClick={() => setRenaming(false)}>
                    {t('navShell.chat.devices.cancel')}
                  </button>
                </form>
              )}

              <h5 className="device-library__section">{t('navShell.chat.devices.factsHeading')}</h5>
              {sheet.facts.length === 0 && (
                <p className="device-library__note">{t('navShell.chat.devices.noFacts')}</p>
              )}
              <ul className="device-library__facts">
                {sheet.facts.map((f) => (
                  <li key={f.field_name} className="device-library__fact">
                    <span className="device-library__fact-label">{f.label}</span>
                    {editingField === f.field_name ? (
                      <form
                        className="device-library__form"
                        onSubmit={(e) => {
                          e.preventDefault()
                          void lib.saveFact(f.field_name, editDraft).then((done) => {
                            if (done) setEditingField(null)
                          })
                        }}
                      >
                        <input
                          className="device-library__input"
                          aria-label={f.label}
                          value={editDraft}
                          onChange={(e) => setEditDraft(e.target.value)}
                        />
                        <button type="submit" disabled={lib.busy || editDraft.trim() === ''}>
                          {t('navShell.chat.devices.save')}
                        </button>
                        <button type="button" onClick={() => setEditingField(null)}>
                          {t('navShell.chat.devices.cancel')}
                        </button>
                      </form>
                    ) : (
                      <>
                        <span className="device-library__fact-value">{f.value}</span>
                        <button
                          type="button"
                          disabled={lib.busy}
                          onClick={() => {
                            setEditDraft(f.value)
                            setEditingField(f.field_name)
                          }}
                        >
                          {t('navShell.chat.devices.edit')}
                        </button>
                        <button
                          type="button"
                          disabled={lib.busy}
                          onClick={() => void lib.clearFact(f.field_name)}
                        >
                          {t('navShell.chat.devices.clear')}
                        </button>
                      </>
                    )}
                  </li>
                ))}
              </ul>

              {sheet.addable_facts.length > 0 && (
                <form
                  className="device-library__form"
                  onSubmit={(e) => {
                    e.preventDefault()
                    void lib.saveFact(addField, addValue).then((done) => {
                      if (done) {
                        setAddField('')
                        setAddValue('')
                      }
                    })
                  }}
                >
                  <label className="device-library__label" htmlFor="device-library-add-field">
                    {t('navShell.chat.devices.addLabel')}
                  </label>
                  <select
                    id="device-library-add-field"
                    className="device-library__input"
                    value={addField}
                    onChange={(e) => setAddField(e.target.value)}
                  >
                    <option value="">{t('navShell.chat.devices.addChoose')}</option>
                    {sheet.addable_facts.map((a) => (
                      <option key={a.field_name} value={a.field_name}>
                        {a.label}
                      </option>
                    ))}
                  </select>
                  <input
                    className="device-library__input"
                    aria-label={t('navShell.chat.devices.addValueLabel')}
                    value={addValue}
                    onChange={(e) => setAddValue(e.target.value)}
                  />
                  <button
                    type="submit"
                    disabled={lib.busy || addField === '' || addValue.trim() === ''}
                  >
                    {t('navShell.chat.devices.add')}
                  </button>
                </form>
              )}

              <h5 className="device-library__section">{t('navShell.chat.devices.casesHeading')}</h5>
              {sheet.cases.length === 0 && (
                <p className="device-library__note">{t('navShell.chat.devices.noCases')}</p>
              )}
              <ul className="device-library__records">
                {sheet.cases.map((c, i) => (
                  <li key={`${i}:${c.opened_at}`} className="device-library__record">
                    {t('navShell.chat.devices.caseLine', {
                      state: c.state,
                      symptoms: c.symptoms,
                      opened: c.opened_at,
                    })}
                  </li>
                ))}
              </ul>

              <h5 className="device-library__section">{t('navShell.chat.devices.changesHeading')}</h5>
              {sheet.changes.length === 0 && (
                <p className="device-library__note">{t('navShell.chat.devices.noChanges')}</p>
              )}
              <ul className="device-library__records">
                {sheet.changes.map((c, i) => (
                  <li key={`${i}:${c.changed_at}`} className="device-library__record">
                    {t('navShell.chat.devices.changeLine', {
                      description: c.description,
                      status: c.status,
                      changed: c.changed_at,
                    })}
                  </li>
                ))}
              </ul>

              {lib.purge.phase === 'idle' && (
                <div className="device-library__footer">
                  <button type="button" disabled={lib.busy} onClick={() => void lib.beginPurge()}>
                    {t('navShell.chat.devices.delete')}
                  </button>
                </div>
              )}
              {lib.purge.phase === 'previewing' && (
                <p className="device-library__note">{t('navShell.chat.devices.checking')}</p>
              )}
              {(lib.purge.phase === 'confirming' || lib.purge.phase === 'purging') && (
                <div
                  className="device-library__confirm"
                  role="alertdialog"
                  aria-modal="true"
                  aria-labelledby="device-library-confirm-title"
                >
                  <p id="device-library-confirm-title" className="device-library__confirm-title">
                    {t('navShell.chat.devices.confirmTitle', { name: sheet.device.display_name })}
                  </p>
                  <p className="device-library__confirm-body">
                    {t('navShell.chat.devices.confirmBody', {
                      name: sheet.device.display_name,
                      facts: lib.purge.counts.facts,
                      cases: lib.purge.counts.cases,
                      changes: lib.purge.counts.changes,
                    })}
                  </p>
                  <p className="device-library__confirm-body">
                    {t('navShell.chat.devices.confirmNotDeleted')}
                  </p>
                  <p className="device-library__confirm-body">
                    {t('navShell.chat.devices.confirmPermanent')}
                  </p>
                  <div className="device-library__footer">
                    <button
                      type="button"
                      disabled={lib.purge.phase === 'purging'}
                      onClick={lib.cancelPurge}
                    >
                      {t('navShell.chat.devices.cancel')}
                    </button>
                    <button
                      type="button"
                      className="device-library__danger"
                      disabled={lib.purge.phase === 'purging'}
                      onClick={() => void lib.confirmPurge()}
                    >
                      {lib.purge.phase === 'purging'
                        ? t('navShell.chat.devices.deleting')
                        : error && isRetryableError(error)
                          ? t('navShell.chat.devices.tryAgain')
                          : t('navShell.chat.devices.confirmDelete')}
                    </button>
                  </div>
                </div>
              )}
            </div>
          )}
          <div className="device-library__footer">
            <button
              type="button"
              onClick={() => {
                setRenaming(false)
                setEditingField(null)
                lib.back()
              }}
            >
              {t('navShell.chat.devices.back')}
            </button>
          </div>
        </>
      )}

      {lib.selected === null && (
        <div className="device-library__footer">
          <button type="button" onClick={onClose}>
            {t('navShell.chat.devices.close')}
          </button>
        </div>
      )}
    </section>
  )
}

function isRetryableError(error: DeviceActionError): boolean {
  return isRetryable(error.kind)
}
