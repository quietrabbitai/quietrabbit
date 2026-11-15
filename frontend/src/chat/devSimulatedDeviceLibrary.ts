// DEV-ONLY simulated device-library backend (items.id=891 stage 7).
//
// Why it exists: Tech Support is not built yet (items.id=915), so no real Persona
// has an owned device, and no UI creates one. Without this the device screen, its
// purge confirmation and the delete-everything control could never be seen in a
// dev build. This FAKES the eight commands the screen calls, in memory, with the
// same error codes the backend returns, and nothing reaches the real commands or
// any database. Only the backend is simulated; the components are the real ones.
//
// Guards (all must hold, otherwise this returns null and does nothing), the
// devSimulatedCollectionConsent.ts precedent:
//   1. The call site wraps the call in the literal `import.meta.env.DEV`, which
//      Vite replaces with `false` in a production bundle, so the call and this
//      module are dead code and tree-shaken away. It cannot ship.
//   1b. The same env check again inside, as defense in depth.
//   2. localStorage['qr.devSimulateDeviceLibrary'] holds one of the MODES below:
//      off by default, set by hand, never by app code.
// The `?.` on env is because Node's test runner (no Vite) has no import.meta.env.
//
// Removal: delete this file, its test, and the calls in deviceLibraryApi.ts once
// Tech Support exists (items.id=915) and a device can be made through a real chat.
import type {
  DeviceFact,
  DeviceFieldChoice,
  DevicePurgeSummary,
  DeviceSheet,
  DeviceSummary,
} from '../bindings.ts'

export const DEV_SIMULATE_DEVICE_LIBRARY_KEY = 'qr.devSimulateDeviceLibrary'

/** empty           no devices
 *  devices         three devices (no facts, partial, full) with cases and changes
 *  runs_blocked    the same devices, but a purge is refused with
 *                  runs_still_running (a chat is still going) */
export const SIMULATED_MODES = ['empty', 'devices', 'runs_blocked'] as const
export type SimulatedMode = (typeof SIMULATED_MODES)[number]

type Result<T> = { status: 'ok'; data: T } | { status: 'error'; error: string }
const ok = <T>(data: T): Result<T> => ({ status: 'ok', data })
const err = <T>(error: string): Result<T> => ({ status: 'error', error })

// A small stand-in for the backend's field catalogue (name, label, category).
const FIELDS: DeviceFieldChoice[] = [
  { field_name: 'subject_class', label: 'Kind', category: 'identity' },
  { field_name: 'make', label: 'Make', category: 'identity' },
  { field_name: 'model', label: 'Model', category: 'identity' },
  { field_name: 'os_version', label: 'Operating system', category: 'version_firmware' },
  { field_name: 'firmware_version', label: 'Firmware version', category: 'version_firmware' },
  { field_name: 'serial_number', label: 'Serial number', category: 'purchase_warranty' },
  { field_name: 'known_issues', label: 'Known issues', category: 'known_issues' },
]

interface SimDevice {
  handle: string
  name: string
  facts: Map<string, string>
  cases: number
  changes: number
}

function initialDevices(mode: SimulatedMode): SimDevice[] {
  if (mode === 'empty') return []
  return [
    { handle: 'E-7K3M9XQA', name: 'Spare tablet', facts: new Map(), cases: 0, changes: 0 },
    {
      handle: 'E-2B8VNC4D',
      name: 'Study laptop',
      facts: new Map([
        ['subject_class', 'device:endpoint:laptop'],
        ['os_version', 'Windows 11'],
      ]),
      cases: 1,
      changes: 0,
    },
    {
      handle: 'E-QH5T0WRE',
      name: 'Living room router',
      facts: new Map([
        ['subject_class', 'device:network:router'],
        ['make', 'Netgear'],
        ['model', 'R7000'],
        ['firmware_version', '1.0.4.82'],
        ['serial_number', '4XF1234567890'],
        ['known_issues', 'drops the connection at night'],
      ]),
      cases: 2,
      changes: 2,
    },
  ]
}

/** The in-memory fake. One instance per dev session and mode. */
export class SimulatedDeviceLibrary {
  private devices: SimDevice[]
  private readonly blocked: boolean

  constructor(mode: SimulatedMode) {
    this.devices = initialDevices(mode)
    this.blocked = mode === 'runs_blocked'
  }

  private find(handle: string): SimDevice | undefined {
    return this.devices.find((d) => d.handle === handle)
  }

  private summary(d: SimDevice): DeviceSummary {
    return {
      handle: d.handle,
      display_name: d.name,
      subject_class: d.facts.get('subject_class') ?? null,
      fact_count: d.facts.size,
      open_case_count: d.cases,
      created_at: '2026-01-01T00:00:00Z',
    }
  }

  list(): Result<DeviceSummary[]> {
    return ok(this.devices.map((d) => this.summary(d)))
  }

  sheet(handle: string): Result<DeviceSheet> {
    const d = this.find(handle)
    if (!d) return err('device_not_found')
    const facts: DeviceFact[] = FIELDS.filter((f) => d.facts.has(f.field_name)).map((f) => ({
      field_name: f.field_name,
      label: f.label,
      category: f.category,
      value: d.facts.get(f.field_name) ?? '',
      recorded_at: '2026-01-02T00:00:00Z',
    }))
    return ok({
      device: this.summary(d),
      facts,
      addable_facts: FIELDS.filter((f) => !d.facts.has(f.field_name)),
      cases: Array.from({ length: d.cases }, (_, i) => ({
        state: i === 0 ? 'open' : 'resolved',
        subject_label: d.name,
        symptoms: 'drops the connection at night',
        current_rung: 2,
        outcome: i === 0 ? null : 'restarting fixed it',
        opened_at: `2026-02-0${i + 1}T00:00:00Z`,
        closed_at: i === 0 ? null : '2026-02-03T00:00:00Z',
      })),
      changes: Array.from({ length: d.changes }, (_, i) => ({
        description: i === 0 ? 'changed the DNS servers' : 'turned off remote management',
        status: i === 0 ? 'believed-active' : 'confirmed-active',
        changed_at: `2026-03-0${i + 1}T00:00:00Z`,
      })),
    })
  }

  private counts(d: SimDevice): DevicePurgeSummary {
    return { cases: d.cases, strategies: d.cases, changes: d.changes, facts: d.facts.size }
  }

  preview(handle: string): Result<DevicePurgeSummary> {
    const d = this.find(handle)
    return d ? ok(this.counts(d)) : err('device_not_found')
  }

  purge(handle: string): Result<DevicePurgeSummary> {
    const d = this.find(handle)
    if (!d) return err('device_not_found')
    if (this.blocked) return err('runs_still_running')
    this.devices = this.devices.filter((x) => x !== d)
    return ok(this.counts(d))
  }

  purgeAll(): Result<DevicePurgeSummary> {
    if (this.blocked) return err('runs_still_running')
    const total: DevicePurgeSummary = { cases: 0, strategies: 0, changes: 0, facts: 0 }
    for (const d of this.devices) {
      const c = this.counts(d)
      total.cases += c.cases
      total.strategies += c.strategies
      total.changes += c.changes
      total.facts += c.facts
    }
    this.devices = []
    return ok(total)
  }

  rename(handle: string, newName: string): Result<DeviceSummary> {
    const d = this.find(handle)
    if (!d) return err('device_not_found')
    const name = newName.trim().replace(/\s+/g, ' ')
    if (name === '' || name.length > 120) return err('invalid_device_name')
    const taken = this.devices.some((x) => x !== d && x.name.toLowerCase() === name.toLowerCase())
    if (taken) return err('name_in_use')
    d.name = name
    return ok(this.summary(d))
  }

  setFact(handle: string, field: string, value: string): Result<DeviceFact> {
    const d = this.find(handle)
    if (!d) return err('device_not_found')
    const def = FIELDS.find((f) => f.field_name === field.trim())
    if (!def) return err('unknown_field')
    const v = value.trim()
    if (v === '' || v.length > 80 || [...v].some((c) => c.charCodeAt(0) < 32)) {
      return err('invalid_value')
    }
    d.facts.set(def.field_name, v)
    return ok({
      field_name: def.field_name,
      label: def.label,
      category: def.category,
      value: v,
      recorded_at: '2026-04-01T00:00:00Z',
    })
  }

  clearFact(handle: string, field: string): Result<null> {
    const d = this.find(handle)
    if (!d) return err('device_not_found')
    if (!FIELDS.some((f) => f.field_name === field.trim())) return err('unknown_field')
    d.facts.delete(field.trim())
    return ok(null)
  }
}

let instance: { mode: SimulatedMode; sim: SimulatedDeviceLibrary } | null = null

/** Guarded entry point: null unless this is a Vite dev build AND the
 *  localStorage switch holds a mode. The same fake is returned on every call
 *  while the mode is unchanged, so a deletion made in one place shows in the
 *  other. */
export function devSimulatedDeviceLibrary(): SimulatedDeviceLibrary | null {
  // Typed loosely on purpose: this file is also compiled by the Node-typed test
  // project, which has neither Vite's ImportMeta.env nor DOM types.
  const env = (import.meta as { env?: { DEV?: boolean } }).env
  if (!env?.DEV) return null
  try {
    const storage = (globalThis as { localStorage?: { getItem(key: string): string | null } })
      .localStorage
    const mode = storage?.getItem(DEV_SIMULATE_DEVICE_LIBRARY_KEY)
    if (!mode || !(SIMULATED_MODES as readonly string[]).includes(mode)) return null
    if (!instance || instance.mode !== mode) {
      instance = { mode: mode as SimulatedMode, sim: new SimulatedDeviceLibrary(mode as SimulatedMode) }
    }
    return instance.sim
  } catch {
    return null
  }
}
