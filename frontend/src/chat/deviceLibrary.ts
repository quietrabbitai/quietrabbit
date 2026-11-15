// The Tech Support device library screen (items.id=891 stage 7, decisions.id=509,
// 520): the pure logic the components use. No React, no Tauri, so a plain node test
// covers it (deviceLibrary.test.ts).
//
// DEVICES ARE NAMED BY THEIR HANDLE. Every command takes the E-XXXXXXXX handle the
// backend returns in the list; the UI never sees or sends an internal id. The
// field choices come from the backend (DeviceSheet.addable_facts), so this screen
// holds no copy of the device catalogue.
import type { DevicePurgeSummary } from '../bindings.ts'

/** The backend's short error codes (commands/device_library.rs), grouped by what
 *  the user should be told. The components pick the copy. */
export type DeviceErrorKind =
  | 'not_found'
  | 'name_in_use'
  | 'invalid_name'
  | 'unknown_field'
  | 'invalid_value'
  | 'runs_running'
  | 'unavailable'
  | 'other'

export function deviceErrorKind(code: string): DeviceErrorKind {
  switch (code) {
    case 'device_not_found':
      return 'not_found'
    case 'name_in_use':
      return 'name_in_use'
    case 'invalid_device_name':
      return 'invalid_name'
    case 'unknown_field':
      return 'unknown_field'
    case 'invalid_value':
      return 'invalid_value'
    case 'runs_still_running':
      return 'runs_running'
    case 'persona_not_found':
    case 'invalid_focus_id':
    case 'catalogue_unavailable':
      return 'unavailable'
    default:
      return 'other'
  }
}

/** A deletion the user can simply try again: a Tech Support chat was still going
 *  and nothing was deleted. */
export function isRetryable(kind: DeviceErrorKind): boolean {
  return kind === 'runs_running'
}

/** The three numbers a purge confirmation shows, as the backend counted them. */
export interface PurgeCounts {
  facts: number
  cases: number
  changes: number
}

export function purgeCounts(summary: DevicePurgeSummary): PurgeCounts {
  return { facts: summary.facts, cases: summary.cases, changes: summary.changes }
}

/** Whether a purge would remove anything beyond the device record itself. */
export function removesRecordedData(summary: DevicePurgeSummary): boolean {
  return summary.facts + summary.cases + summary.changes > 0
}
