// Plain-assertion test for devSimulatedDeviceLibrary.ts (items.id=891 stage 7).
//
//   node --experimental-strip-types src/chat/devSimulatedDeviceLibrary.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import {
  DEV_SIMULATE_DEVICE_LIBRARY_KEY,
  SIMULATED_MODES,
  SimulatedDeviceLibrary,
  devSimulatedDeviceLibrary,
} from './devSimulatedDeviceLibrary.ts'

// Guard: under Node there is no Vite env, so the guarded entry point is inert,
// the same inertness a production build gets (even with the switch set).
;(globalThis as { localStorage?: unknown }).localStorage = {
  getItem: (key: string) => (key === DEV_SIMULATE_DEVICE_LIBRARY_KEY ? 'devices' : null),
}
assert.equal(devSimulatedDeviceLibrary(), null)
delete (globalThis as { localStorage?: unknown }).localStorage

assert.deepEqual([...SIMULATED_MODES], ['empty', 'devices', 'runs_blocked'])

const must = <T>(r: { status: 'ok'; data: T } | { status: 'error'; error: string }): T => {
  assert.equal(r.status, 'ok', JSON.stringify(r))
  return (r as { status: 'ok'; data: T }).data
}

// The modes start where they say, and only handles name a device.
{
  assert.deepEqual(must(new SimulatedDeviceLibrary('empty').list()), [])
  const sim = new SimulatedDeviceLibrary('devices')
  const list = must(sim.list())
  assert.equal(list.length, 3)
  for (const d of list) assert.match(d.handle, /^E-[0-9A-Z]{8}$/)
  assert.equal(list.filter((d) => d.fact_count === 0).length, 1, 'one device has no facts')
}

// The same codes the backend returns.
{
  const sim = new SimulatedDeviceLibrary('devices')
  const [tablet, laptop] = must(sim.list())
  assert.deepEqual(sim.sheet('E-00000000'), { status: 'error', error: 'device_not_found' })
  assert.deepEqual(sim.rename(laptop.handle, '  '), { status: 'error', error: 'invalid_device_name' })
  assert.deepEqual(sim.rename(laptop.handle, ' SPARE   tablet '), { status: 'error', error: 'name_in_use' })
  assert.equal(must(sim.rename(laptop.handle, ' Work  laptop ')).display_name, 'Work laptop')
  assert.deepEqual(sim.setFact(tablet.handle, 'wifi_password', 'x'), { status: 'error', error: 'unknown_field' })
  assert.deepEqual(sim.setFact(tablet.handle, 'make', '   '), { status: 'error', error: 'invalid_value' })
  assert.deepEqual(sim.setFact(tablet.handle, 'make', 'a\nb'), { status: 'error', error: 'invalid_value' })
  assert.equal(must(sim.setFact(tablet.handle, 'make', ' Apple ')).value, 'Apple')
  assert.deepEqual(sim.clearFact(tablet.handle, 'wifi_password'), { status: 'error', error: 'unknown_field' })
  must(sim.clearFact(tablet.handle, 'make'))
  assert.equal(must(sim.sheet(tablet.handle)).facts.length, 0)
}

// The sheet lists facts in the catalogue's order and offers the rest to add.
{
  const sim = new SimulatedDeviceLibrary('devices')
  const router = must(sim.list()).find((d) => d.display_name === 'Living room router')!
  const sheet = must(sim.sheet(router.handle))
  assert.deepEqual(
    sheet.facts.map((f) => f.field_name),
    ['subject_class', 'make', 'model', 'firmware_version', 'serial_number', 'known_issues'],
  )
  assert.deepEqual(sheet.addable_facts.map((f) => f.field_name), ['os_version'])
}

// Preview equals the purge, a purge removes the device, purge all removes the rest.
{
  const sim = new SimulatedDeviceLibrary('devices')
  const router = must(sim.list()).find((d) => d.display_name === 'Living room router')!
  const preview = must(sim.preview(router.handle))
  assert.deepEqual(must(sim.purge(router.handle)), preview)
  assert.equal(must(sim.list()).length, 2)
  assert.deepEqual(sim.preview(router.handle), { status: 'error', error: 'device_not_found' })
  must(sim.purgeAll())
  assert.deepEqual(must(sim.list()), [])
}

// runs_blocked refuses a purge with the retryable code and deletes nothing.
{
  const sim = new SimulatedDeviceLibrary('runs_blocked')
  const first = must(sim.list())[0]
  assert.deepEqual(sim.purge(first.handle), { status: 'error', error: 'runs_still_running' })
  assert.deepEqual(sim.purgeAll(), { status: 'error', error: 'runs_still_running' })
  assert.equal(must(sim.list()).length, 3)
}

console.log('devSimulatedDeviceLibrary.test.ts: all assertions passed')
