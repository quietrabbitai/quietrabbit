// Measured contrast and target size of the final-failure notice (items.id=831).
// The design doc says its pairs "were estimated by eye, not measured, so the
// build checks them": this test computes them from the real tokens in
// index.css, so a token change that breaks a pair fails here.
//
//   node --experimental-strip-types src/chat/finalFailureContrast.test.ts
//
// (also wired as part of `npm test`.)
//
// WCAG 2.x relative luminance. Text needs 4.5:1; a button's border, the clock
// icon and the focus ring (non-text) need 3:1. The notice sits inside an
// assistant message bubble, whose background is --surface (asserted below).
import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const INDEX_CSS = fs.readFileSync(path.join(HERE, '..', 'index.css'), 'utf8')
const NOTICE_CSS = fs.readFileSync(path.join(HERE, 'FinalFailureNotice.css'), 'utf8')
const PANE_CSS = fs.readFileSync(path.join(HERE, 'ChatPane.css'), 'utf8')

type Rgba = { r: number; g: number; b: number; a: number }

// The tokens of the first :root block.
const rootBlock = /:root\s*\{([\s\S]*?)\n\}/.exec(INDEX_CSS)
assert.ok(rootBlock, ':root block found in index.css')
const tokens = new Map<string, string>()
for (const m of rootBlock[1].replace(/\/\*[\s\S]*?\*\//g, '').matchAll(/(--[\w-]+)\s*:\s*([^;]+);/g)) {
  tokens.set(m[1], m[2].trim())
}

function parse(value: string, seen = new Set<string>()): Rgba {
  const v = value.trim()
  const ref = /^var\((--[\w-]+)\)$/.exec(v)
  if (ref) {
    assert.ok(!seen.has(ref[1]), `token cycle at ${ref[1]}`)
    const next = tokens.get(ref[1])
    assert.ok(next !== undefined, `token ${ref[1]} is defined`)
    return parse(next, new Set(seen).add(ref[1]))
  }
  const hex = /^#([0-9a-f]{6})$/i.exec(v)
  if (hex) {
    const n = parseInt(hex[1], 16)
    return { r: (n >> 16) & 255, g: (n >> 8) & 255, b: n & 255, a: 1 }
  }
  const rgba = /^rgba?\(\s*(\d+),\s*(\d+),\s*(\d+)(?:,\s*([\d.]+))?\s*\)$/.exec(v)
  if (rgba) {
    return { r: +rgba[1], g: +rgba[2], b: +rgba[3], a: rgba[4] === undefined ? 1 : +rgba[4] }
  }
  throw new Error(`cannot parse colour: ${value}`)
}

function over(fg: Rgba, bg: Rgba): Rgba {
  return {
    r: fg.r * fg.a + bg.r * (1 - fg.a),
    g: fg.g * fg.a + bg.g * (1 - fg.a),
    b: fg.b * fg.a + bg.b * (1 - fg.a),
    a: 1,
  }
}

function luminance(c: Rgba): number {
  const f = (v: number) => {
    const s = v / 255
    return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4
  }
  return 0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b)
}

function ratio(fg: string, bg: string): number {
  const back = over(parse(bg), { r: 255, g: 255, b: 255, a: 1 })
  const front = over(parse(fg), back)
  const [hi, lo] = [luminance(front), luminance(back)].sort((a, b) => b - a)
  return (hi + 0.05) / (lo + 0.05)
}

// The bubble the notice sits in is --surface.
assert.match(
  PANE_CSS,
  /\.chat-pane__message--assistant\s*\{[^}]*background:\s*var\(--surface\)/,
  'the assistant bubble is --surface',
)
const BUBBLE = 'var(--surface)'

// The notice uses exactly these tokens (asserted against its CSS so a change
// of colour in the stylesheet cannot slip past the numbers below).
assert.match(NOTICE_CSS, /\.final-failure__text\s*\{[^}]*color:\s*var\(--text\)/)
assert.match(NOTICE_CSS, /\.final-failure__icon\s*\{[^}]*color:\s*var\(--slate-blue\)/)
assert.match(NOTICE_CSS, /\.final-failure__try-again\s*\{[^}]*color:\s*var\(--ink\)/)
assert.match(NOTICE_CSS, /\.final-failure__try-again\s*\{[^}]*border:\s*1\.5px solid var\(--ink\)/)
assert.match(NOTICE_CSS, /outline:\s*var\(--focus-ring\)/)
assert.equal(tokens.get('--focus-ring'), '2px solid var(--ink)')

const measured: Array<[string, string, string, number]> = [
  ['sentence (--text on the bubble)', 'var(--text)', BUBBLE, 4.5],
  ['Try again label (--ink on the bubble)', 'var(--ink)', BUBBLE, 4.5],
  ['Try again border (--ink on the bubble)', 'var(--ink)', BUBBLE, 3],
  ['clock icon (--slate-blue on the bubble)', 'var(--slate-blue)', BUBBLE, 3],
  ['focus ring (--ink on the bubble)', 'var(--ink)', BUBBLE, 3],
]
for (const [name, fg, bg, min] of measured) {
  const r = ratio(fg, bg)
  assert.ok(r >= min, `${name}: ${r.toFixed(2)}:1 is below ${min}:1`)
}

// The pair the design mockup used for its quiet text, so the number is on
// record: ink at 62% on Sage mist is 3.89:1, below 4.5. The notice does not use
// it.
assert.ok(ratio('rgba(36, 44, 42, 0.62)', '#e6e9e2') < 4.5)

// Target size: the button is at least 44 px tall.
const rule = /\.final-failure__try-again\s*\{([^}]*)\}/.exec(NOTICE_CSS)
assert.ok(rule, 'the Try again rule exists')
const minHeight = /min-height:\s*(\d+)px/.exec(rule[1])
assert.ok(minHeight, 'the Try again button sets min-height')
assert.ok(Number(minHeight[1]) >= 44, `min-height ${minHeight[1]}px is below 44px`)

console.log('finalFailureContrast.test.ts: ok')
