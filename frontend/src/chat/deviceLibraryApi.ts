// The device-library calls the UI makes (items.id=891 stage 7), each answered by
// the real command or, in a dev build with the simulation switch on, by the
// in-memory fake (devSimulatedDeviceLibrary.ts). The `import.meta.env.DEV` literal
// lets a production bundle drop the fake entirely.
import {
  commands,
  type DeviceFact,
  type DevicePurgeSummary,
  type DeviceSheet,
  type DeviceSummary,
} from '../bindings'
import { devSimulatedDeviceLibrary } from './devSimulatedDeviceLibrary'

export type DeviceResult<T> = { status: 'ok'; data: T } | { status: 'error'; error: string }

export function listDevices(personaId: string, focusId: string): Promise<DeviceResult<DeviceSummary[]>> {
  if (import.meta.env.DEV) {
    const sim = devSimulatedDeviceLibrary()
    if (sim) return Promise.resolve(sim.list())
  }
  return commands.listDevices(personaId, focusId)
}

export function readDeviceSheet(
  personaId: string,
  focusId: string,
  device: string,
): Promise<DeviceResult<DeviceSheet>> {
  if (import.meta.env.DEV) {
    const sim = devSimulatedDeviceLibrary()
    if (sim) return Promise.resolve(sim.sheet(device))
  }
  return commands.getDeviceSheet(personaId, focusId, device)
}

export function previewPurge(
  personaId: string,
  focusId: string,
  device: string,
): Promise<DeviceResult<DevicePurgeSummary>> {
  if (import.meta.env.DEV) {
    const sim = devSimulatedDeviceLibrary()
    if (sim) return Promise.resolve(sim.preview(device))
  }
  return commands.previewDevicePurge(personaId, focusId, device)
}

export function purgeOneDevice(
  personaId: string,
  focusId: string,
  device: string,
): Promise<DeviceResult<DevicePurgeSummary>> {
  if (import.meta.env.DEV) {
    const sim = devSimulatedDeviceLibrary()
    if (sim) return Promise.resolve(sim.purge(device))
  }
  return commands.purgeDevice(personaId, focusId, device)
}

export function purgeEveryDevice(personaId: string, focusId: string): Promise<DeviceResult<DevicePurgeSummary>> {
  if (import.meta.env.DEV) {
    const sim = devSimulatedDeviceLibrary()
    if (sim) return Promise.resolve(sim.purgeAll())
  }
  return commands.purgeAllDevices(personaId, focusId)
}

export function renameOneDevice(
  personaId: string,
  focusId: string,
  device: string,
  newName: string,
): Promise<DeviceResult<DeviceSummary>> {
  if (import.meta.env.DEV) {
    const sim = devSimulatedDeviceLibrary()
    if (sim) return Promise.resolve(sim.rename(device, newName))
  }
  return commands.renameDevice(personaId, focusId, device, newName)
}

export function saveDeviceFact(
  personaId: string,
  focusId: string,
  device: string,
  field: string,
  value: string,
): Promise<DeviceResult<DeviceFact>> {
  if (import.meta.env.DEV) {
    const sim = devSimulatedDeviceLibrary()
    if (sim) return Promise.resolve(sim.setFact(device, field, value))
  }
  return commands.setDeviceFact(personaId, focusId, device, field, value)
}

export function removeDeviceFact(
  personaId: string,
  focusId: string,
  device: string,
  field: string,
): Promise<DeviceResult<null>> {
  if (import.meta.env.DEV) {
    const sim = devSimulatedDeviceLibrary()
    if (sim) return Promise.resolve(sim.clearFact(device, field))
  }
  return commands.clearDeviceFact(personaId, focusId, device, field)
}
