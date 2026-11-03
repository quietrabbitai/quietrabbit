// Plain-assertion test for popupCloseKey.ts and the wiring that makes a
// popup always closable (items.id=853), matching privacyModelProgress.test.ts's
// convention: run directly on Node's built-in TypeScript support, no framework.
//
//   node --experimental-strip-types src/cloudChatAccess/popupCloseKey.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { isPopupCloseKey } from './popupCloseKey.ts'

const ev = (key: string, mods: Partial<Record<'ctrlKey' | 'shiftKey' | 'altKey' | 'metaKey', boolean>> = {}) => ({
  key,
  ctrlKey: false,
  shiftKey: false,
  altKey: false,
  metaKey: false,
  ...mods,
})

// Ctrl+W closes, whatever the case the layout reports (caps lock).
assert.equal(isPopupCloseKey(ev('w', { ctrlKey: true })), true)
assert.equal(isPopupCloseKey(ev('W', { ctrlKey: true })), true)

// No other modifier is allowed alongside Ctrl.
assert.equal(isPopupCloseKey(ev('w', { ctrlKey: true, shiftKey: true })), false)
assert.equal(isPopupCloseKey(ev('w', { ctrlKey: true, altKey: true })), false)
assert.equal(isPopupCloseKey(ev('w', { ctrlKey: true, metaKey: true })), false)

// Not Ctrl+W: plain w, other Ctrl chords, and Escape (pages use it themselves).
assert.equal(isPopupCloseKey(ev('w')), false)
assert.equal(isPopupCloseKey(ev('q', { ctrlKey: true })), false)
assert.equal(isPopupCloseKey(ev('Escape')), false)
assert.equal(isPopupCloseKey(ev('Escape', { ctrlKey: true })), false)
assert.equal(isPopupCloseKey(ev('Control', { ctrlKey: true })), false)

// Wiring (source scan, like noUntrustedMarkup.test.ts): the listener lives in
// PopupHitLayer at window level in the capture phase and uses the helper, the
// head button calls the close command, and neither adds a bare-Escape close.
const SRC = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const layer = fs.readFileSync(path.join(SRC, 'cloudChatAccess/PopupHitLayer.tsx'), 'utf8')
const pane = fs.readFileSync(path.join(SRC, 'navShell/CloudChatAccessPane.tsx'), 'utf8')
assert.match(layer, /isPopupCloseKey\(/)
assert.match(layer, /window\.addEventListener\('keydown', \w+, true\)/)
assert.match(layer, /window\.removeEventListener\('keydown', \w+, true\)/)
assert.match(layer, /commands\.closeCloudChatPopup\(/)
assert.match(pane, /commands\.closeCloudChatPopup\(/)
assert.match(pane, /navShell\.cloudChatAccessPane\.popupCloseButton/)
assert.doesNotMatch(layer, /['"]Escape['"]/)
assert.doesNotMatch(pane, /['"]Escape['"]/)

console.log('popupCloseKey: all assertions passed')
