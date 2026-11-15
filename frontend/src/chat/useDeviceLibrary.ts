// State for the Tech Support device library screen (items.id=891 stage 7), one
// instance per (Persona, Focus): the list, the open device's sheet, the user's
// edits and the two-step purge (preview, then confirm). Every call goes through
// deviceLibraryApi (the real commands or the dev fake) and every device is named
// by its handle.
import { useCallback, useEffect, useRef, useState } from 'react'
import type { DevicePurgeSummary, DeviceSheet, DeviceSummary } from '../bindings'
import {
  listDevices,
  previewPurge,
  purgeOneDevice,
  readDeviceSheet,
  removeDeviceFact,
  renameOneDevice,
  saveDeviceFact,
} from './deviceLibraryApi'
import { deviceErrorKind, type DeviceErrorKind } from './deviceLibrary'

export interface DeviceActionError {
  kind: DeviceErrorKind
  message: string
}

export type PurgeStage =
  | { phase: 'idle' }
  | { phase: 'previewing' }
  | { phase: 'confirming'; counts: DevicePurgeSummary }
  | { phase: 'purging'; counts: DevicePurgeSummary }

export interface DeviceLibraryController {
  listStatus: 'loading' | 'ready' | 'error'
  devices: DeviceSummary[]
  listError: string | null
  /** The open device's handle, or null on the list. */
  selected: string | null
  sheet: DeviceSheet | null
  sheetStatus: 'idle' | 'loading' | 'ready' | 'error'
  sheetError: string | null
  busy: boolean
  actionError: DeviceActionError | null
  purge: PurgeStage
  /** Set after a purge that worked: the counts the screen shows once. */
  purged: DevicePurgeSummary | null
  open: (handle: string) => void
  back: () => void
  rename: (newName: string) => Promise<boolean>
  saveFact: (field: string, value: string) => Promise<boolean>
  clearFact: (field: string) => Promise<void>
  beginPurge: () => Promise<void>
  cancelPurge: () => void
  confirmPurge: () => Promise<void>
  dismissPurged: () => void
}

export function useDeviceLibrary(personaId: string, focusId: string): DeviceLibraryController {
  const [listStatus, setListStatus] = useState<DeviceLibraryController['listStatus']>('loading')
  const [devices, setDevices] = useState<DeviceSummary[]>([])
  const [listError, setListError] = useState<string | null>(null)
  const [selected, setSelected] = useState<string | null>(null)
  const [sheet, setSheet] = useState<DeviceSheet | null>(null)
  const [sheetStatus, setSheetStatus] = useState<DeviceLibraryController['sheetStatus']>('idle')
  const [sheetError, setSheetError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [actionError, setActionError] = useState<DeviceActionError | null>(null)
  const [purge, setPurge] = useState<PurgeStage>({ phase: 'idle' })
  const [purged, setPurged] = useState<DevicePurgeSummary | null>(null)
  // Request tokens: an answer for a Persona, Focus or device that is no longer
  // shown is dropped.
  const listToken = useRef(0)
  const sheetToken = useRef(0)

  const loadList = useCallback(() => {
    const token = ++listToken.current
    return listDevices(personaId, focusId).then((r) => {
      if (token !== listToken.current) return
      if (r.status === 'ok') {
        setDevices(r.data)
        setListError(null)
        setListStatus('ready')
      } else {
        setListError(r.error)
        setListStatus('error')
      }
    })
  }, [personaId, focusId])

  const loadSheet = useCallback(
    (handle: string) => {
      const token = ++sheetToken.current
      setSheetStatus('loading')
      return readDeviceSheet(personaId, focusId, handle).then((r) => {
        if (token !== sheetToken.current) return
        if (r.status === 'ok') {
          setSheet(r.data)
          setSheetError(null)
          setSheetStatus('ready')
        } else {
          setSheet(null)
          setSheetError(r.error)
          setSheetStatus('error')
        }
      })
    },
    [personaId, focusId],
  )

  // The caller remounts this hook's component (a `key`) when the Persona or the
  // Focus changes, so state never carries over and this only has to fetch.
  useEffect(() => {
    void loadList()
  }, [loadList])

  const fail = useCallback((code: string) => {
    setActionError({ kind: deviceErrorKind(code), message: code })
  }, [])

  const open = useCallback(
    (handle: string) => {
      setSelected(handle)
      setSheet(null)
      setActionError(null)
      setPurge({ phase: 'idle' })
      setPurged(null)
      void loadSheet(handle)
    },
    [loadSheet],
  )

  const back = useCallback(() => {
    sheetToken.current++
    setSelected(null)
    setSheet(null)
    setSheetStatus('idle')
    setActionError(null)
    setPurge({ phase: 'idle' })
    void loadList()
  }, [loadList])

  const rename = useCallback(
    async (newName: string) => {
      if (!selected) return false
      setBusy(true)
      setActionError(null)
      const r = await renameOneDevice(personaId, focusId, selected, newName)
      setBusy(false)
      if (r.status === 'error') {
        fail(r.error)
        return false
      }
      await loadSheet(selected)
      return true
    },
    [selected, personaId, focusId, loadSheet, fail],
  )

  const saveFact = useCallback(
    async (field: string, value: string) => {
      if (!selected) return false
      setBusy(true)
      setActionError(null)
      const r = await saveDeviceFact(personaId, focusId, selected, field, value)
      setBusy(false)
      if (r.status === 'error') {
        fail(r.error)
        return false
      }
      await loadSheet(selected)
      return true
    },
    [selected, personaId, focusId, loadSheet, fail],
  )

  const clearFact = useCallback(
    async (field: string) => {
      if (!selected) return
      setBusy(true)
      setActionError(null)
      const r = await removeDeviceFact(personaId, focusId, selected, field)
      setBusy(false)
      if (r.status === 'error') {
        fail(r.error)
        return
      }
      await loadSheet(selected)
    },
    [selected, personaId, focusId, loadSheet, fail],
  )

  // Step one of a purge: ask the backend what it would remove, then show it.
  const beginPurge = useCallback(async () => {
    if (!selected) return
    setActionError(null)
    setPurge({ phase: 'previewing' })
    const r = await previewPurge(personaId, focusId, selected)
    if (r.status === 'error') {
      setPurge({ phase: 'idle' })
      fail(r.error)
      return
    }
    setPurge({ phase: 'confirming', counts: r.data })
  }, [selected, personaId, focusId, fail])

  const cancelPurge = useCallback(() => setPurge({ phase: 'idle' }), [])

  // Step two: the user confirmed. A refusal because a chat is still going leaves
  // the confirmation showing so the user can press Delete again.
  const confirmPurge = useCallback(async () => {
    if (!selected || purge.phase !== 'confirming') return
    const counts = purge.counts
    setActionError(null)
    setPurge({ phase: 'purging', counts })
    const r = await purgeOneDevice(personaId, focusId, selected)
    if (r.status === 'error') {
      setPurge({ phase: 'confirming', counts })
      fail(r.error)
      return
    }
    setPurged(r.data)
    setPurge({ phase: 'idle' })
    sheetToken.current++
    setSelected(null)
    setSheet(null)
    setSheetStatus('idle')
    await loadList()
  }, [selected, purge, personaId, focusId, loadList, fail])

  const dismissPurged = useCallback(() => setPurged(null), [])

  return {
    listStatus,
    devices,
    listError,
    selected,
    sheet,
    sheetStatus,
    sheetError,
    busy,
    actionError,
    purge,
    purged,
    open,
    back,
    rename,
    saveFact,
    clearFact,
    beginPurge,
    cancelPurge,
    confirmPurge,
    dismissPurged,
  }
}
