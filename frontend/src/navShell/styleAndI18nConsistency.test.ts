// Mechanical consistency check between CSS class selectors <-> their TSX
// consumers, and en.json i18n keys <-> their t() call sites (items.id=534).
// CSS/i18n renames fail silently -- no compiler catches a dangling
// selector or an orphaned translation key -- so this is a permanent
// regression test, not a one-off script, following the same no-framework
// convention as consentDecisions.test.ts / reviewSections.test.ts /
// frictionGateDetail.test.ts:
//
//   node --experimental-strip-types src/navShell/styleAndI18nConsistency.test.ts
//
// (also wired as part of `npm test`.)
//
// Scope note: this scans the whole frontend/src tree, not just the files
// items.id=534 touches -- it's a general-purpose regression test. Anything
// it flags outside this item's own rename must NOT be "fixed" here; add it
// to PRE_EXISTING_ORPHAN_CLASSES / PRE_EXISTING_ORPHAN_KEYS below instead,
// each with a one-line reason. Do NOT add a name there because the parser
// merely failed to resolve it -- extend resolveLiteralAlternatives()
// instead; the allowlists are only for genuinely dead code this item isn't
// scoped to remove.
import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const SRC_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const SENTINEL_RE = /\u0000(\d+)\u0000/

// ---------------------------------------------------------------------------
// Pre-existing gaps found the first time this test ran against source that
// already existed before items.id=534 touched anything (verified: each name
// really has no consumer/definition/call-site anywhere in frontend/src, not
// just a case this parser can't follow). Not this item's job to fix -- it's
// a CSS/i18n RENAME item for the retired tier vocabulary, not a dead-code
// sweep. Allowlisted so this test asserts "no NEW orphans", not "zero
// orphans ever".
// ---------------------------------------------------------------------------
const PRE_EXISTING_ORPHAN_CLASSES: string[] = [
  'chat-history-list__empty', // ChatHistoryList.tsx never renders an empty state with this class
  'chat-pane__unsent-badge', // unsent-message UI (see unsentBadge/notSentNotice below) not wired up yet
  'chat-pane__not-sent-notice', // same unwired unsent-message feature
  'button-icon', // Vite/React template leftover in index.css, unused
  'counter', // Vite/React template leftover in index.css, unused
  'login-form', // LoginForm.tsx has no LoginForm.css; relies on inherited/global styles only
  'chat-pane__content-timeout', // ChatPane.tsx references a class with no CSS definition anywhere
  'cross-persona-confirm-modal__field', // same: TSX class with no matching selector
  'pg-modal__section', // PrivacyGuardianModal.tsx: no matching selector in PrivacyGuardianModal.css
  'pg-modal__section-label', // same
  'pg-modal__remember-for-persona', // same
  'focus-settings-controls', // FocusSettingsControls.tsx has no FocusSettingsControls.css
  'focus-settings-controls__gate-confirm', // same
  'focus-settings-pane', // FocusSettingsPane.tsx has no FocusSettingsPane.css
  'history-screen__row', // HistoryScreen.css only defines __row-header/__row-actions, not __row
  'history-screen__row-label', // same mismatch
  'history-screen__chat-history', // same mismatch
  'persona-hub', // PersonaHub.tsx has no PersonaHub.css
  'persona-hub__focus-list', // same
  'cloud-chat-rail__row--loaded', // CloudChatSelector.tsx's rowState can be 'loaded', but only
  // --active/--idle have a CSS rule -- pre-existing missing style (was tier3-rail__row--loaded
  // before items.id=534's rename), not introduced by this item.
]
const PRE_EXISTING_ORPHAN_KEYS: string[] = [
  'navShell.content.chatPlaceholder', // defined, no t() call site anywhere
  'navShell.chat.unsentBadge', // same unwired unsent-message feature as chat-pane__unsent-badge above
  'navShell.chat.notSentNotice', // same
  'navShell.focusSettings.crumbLabel', // defined, no t() call site anywhere
  'privacyGuardianModal.selectedCount', // defined, no t() call site anywhere
]

function walk(dir: string, exts: string[], out: string[] = []): string[] {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name)
    if (entry.isDirectory()) {
      walk(full, exts, out)
    } else if (exts.some((ext) => entry.name.endsWith(ext))) {
      out.push(full)
    }
  }
  return out
}

function relPath(p: string): string {
  return path.relative(SRC_ROOT, p)
}

function lineAt(content: string, index: number): number {
  let line = 1
  for (let i = 0; i < index; i++) if (content[i] === '\n') line++
  return line
}

// ---------------------------------------------------------------------------
// Small hand-rolled JS/TS expression resolver. Not a real parser -- just
// enough balanced-bracket/string tracking to pull apart the handful of
// shapes this codebase actually uses for className/t() arguments: plain
// strings, template literals, ternaries between two resolvable branches,
// a local `const NAME = <expr>` reference, and a call to a same-file
// `function NAME(...) { ... return <expr> ... }` helper (switch-per-case
// key/class lookups, e.g. FocusSettingsControls.tsx's
// maxPermittedTierLabelKey). Anything else throws -- see file header.
// ---------------------------------------------------------------------------

// Blanks out // and /* */ comments (preserving line numbers and string
// contents) so a stray "t()" or "className=" mentioned in prose -- e.g.
// PrivacyGuardianModal.tsx's own header comment about "this codebase's
// plain-global-CSS / useState / t() conventions" -- never gets scanned as
// real code.
function stripJsComments(content: string): string {
  let out = ''
  let i = 0
  let inString: false | string = false
  while (i < content.length) {
    const c = content[i]
    if (inString) {
      out += c
      if (c === '\\') {
        out += content[i + 1] ?? ''
        i += 2
        continue
      }
      if (c === inString) inString = false
      i++
      continue
    }
    if (c === '"' || c === "'" || c === '`') {
      inString = c
      out += c
      i++
      continue
    }
    if (c === '/' && content[i + 1] === '/') {
      while (i < content.length && content[i] !== '\n') {
        out += ' '
        i++
      }
      continue
    }
    if (c === '/' && content[i + 1] === '*') {
      out += '  '
      i += 2
      while (i < content.length && !(content[i] === '*' && content[i + 1] === '/')) {
        out += content[i] === '\n' ? '\n' : ' '
        i++
      }
      out += '  '
      i += 2
      continue
    }
    out += c
    i++
  }
  return out
}

function findMatchingBracket(content: string, openIndex: number): number {
  const open = content[openIndex]
  const close = open === '(' ? ')' : open === '{' ? '}' : ']'
  let depth = 1
  let i = openIndex + 1
  let inString: false | string = false
  while (i < content.length) {
    const c = content[i]
    if (inString) {
      if (c === '\\') {
        i += 2
        continue
      }
      if (c === inString) inString = false
      i++
      continue
    }
    if (c === '"' || c === "'" || c === '`') {
      inString = c
      i++
      continue
    }
    if (c === open) {
      depth++
      i++
      continue
    }
    if (c === close) {
      depth--
      if (depth === 0) return i
      i++
      continue
    }
    i++
  }
  throw new Error(`unbalanced ${open}...${close} starting at index ${openIndex}`)
}

// First-argument extraction for a call whose '(' is at openIndex: stops at
// a top-level comma (second argument, e.g. t() interpolation params) or the
// call's own matching ')'.
function extractFirstCallArg(content: string, openIndex: number): string {
  let depth = 1
  let i = openIndex + 1
  let inString: false | string = false
  const start = i
  while (i < content.length) {
    const c = content[i]
    if (inString) {
      if (c === '\\') {
        i += 2
        continue
      }
      if (c === inString) inString = false
      i++
      continue
    }
    if (c === '"' || c === "'" || c === '`') {
      inString = c
      i++
      continue
    }
    if (c === '(' || c === '{' || c === '[') {
      depth++
      i++
      continue
    }
    if (c === ')' || c === '}' || c === ']') {
      depth--
      if (depth === 0) return content.slice(start, i)
      i++
      continue
    }
    if (c === ',' && depth === 1) return content.slice(start, i)
    i++
  }
  throw new Error(`unterminated call starting at index ${openIndex}`)
}

function findTopLevelChar(expr: string, target: string, from: number): number {
  let depth = 0
  let inString: false | string = false
  for (let i = from; i < expr.length; i++) {
    const c = expr[i]
    if (inString) {
      if (c === '\\') {
        i++
        continue
      }
      if (c === inString) inString = false
      continue
    }
    if (c === '"' || c === "'" || c === '`') {
      inString = c
      continue
    }
    if (c === '(' || c === '{' || c === '[') {
      depth++
      continue
    }
    if (c === ')' || c === '}' || c === ']') {
      depth--
      continue
    }
    if (depth === 0 && c === target) {
      if (target === '?' && (expr[i + 1] === '.' || expr[i + 1] === '?')) continue
      return i
    }
  }
  return -1
}

function splitTopLevelTernary(expr: string): { a: string; b: string } | null {
  const qIndex = findTopLevelChar(expr, '?', 0)
  if (qIndex === -1) return null
  const colonIndex = findTopLevelChar(expr, ':', qIndex + 1)
  if (colonIndex === -1) return null
  return { a: expr.slice(qIndex + 1, colonIndex).trim(), b: expr.slice(colonIndex + 1).trim() }
}

// Extracts the right-hand side of `const NAME = <here>`, stopping at a
// top-level `;` or a newline whose next non-blank line does not continue
// the expression (this codebase's Prettier style has no semicolons and
// leads continuation lines with the operator, e.g. `? '...' \n : '...'`).
function extractStatementRhs(content: string, fromIndex: number): string {
  let i = fromIndex
  let depth = 0
  let inString: false | string = false
  let end = content.length
  while (i < content.length) {
    const c = content[i]
    if (inString) {
      if (c === '\\') {
        i += 2
        continue
      }
      if (c === inString) inString = false
      i++
      continue
    }
    if (c === '"' || c === "'" || c === '`') {
      inString = c
      i++
      continue
    }
    if (c === '(' || c === '{' || c === '[') {
      depth++
      i++
      continue
    }
    if (c === ')' || c === '}' || c === ']') {
      depth--
      i++
      continue
    }
    if (c === ';' && depth === 0) {
      end = i
      break
    }
    if (c === '\n' && depth === 0) {
      let j = i + 1
      while (j < content.length && /[ \t]/.test(content[j])) j++
      const next2 = content.slice(j, j + 2)
      const isContinuation = /^[?:.+]/.test(content[j] ?? '') || next2 === '&&' || next2 === '||'
      if (!isContinuation) {
        end = i
        break
      }
    }
    i++
  }
  return content.slice(fromIndex, end).trim()
}

let sentinelCounter = 0

function tryResolveConst(name: string, content: string, file: string, line: number): string[] | null {
  const declRe = new RegExp(`const\\s+${name}\\s*(?::[^=]+)?=\\s*`)
  const m = declRe.exec(content)
  if (!m) return null
  const rhs = extractStatementRhs(content, m.index + m[0].length)
  return resolveLiteralAlternatives(rhs, content, file, line)
}

function resolveFunctionReturns(name: string, content: string, file: string, line: number): string[] {
  const fnRe = new RegExp(`function\\s+${name}\\s*\\(`)
  const m = fnRe.exec(content)
  if (!m) {
    throw new Error(
      `${relPath(file)}:${line}: cannot resolve call \`${name}(...)\` -- no \`function ${name}(\` ` +
        `found in this file. extend resolveFunctionReturns() (e.g. to follow arrow functions or ` +
        `cross-file lookups) before trusting this file's consistency result`,
    )
  }
  const paramOpen = content.indexOf('(', m.index)
  const paramClose = findMatchingBracket(content, paramOpen)
  const bodyOpen = content.indexOf('{', paramClose)
  const bodyClose = findMatchingBracket(content, bodyOpen)
  const body = content.slice(bodyOpen + 1, bodyClose)
  const returnRe = /return\s+([^\n;]+)/g
  const alternatives: string[] = []
  let rm: RegExpExecArray | null
  while ((rm = returnRe.exec(body))) {
    alternatives.push(...resolveLiteralAlternatives(rm[1], content, file, line))
  }
  if (alternatives.length === 0) {
    throw new Error(
      `${relPath(file)}:${line}: \`function ${name}\` has no \`return <literal>\` statements ` +
        `resolveFunctionReturns() can find -- extend it before trusting this file's consistency result`,
    )
  }
  return alternatives
}

// Splits template-literal contents into literal/`${expr}` segments,
// tracking brace depth inside each `${...}`.
type Segment = { kind: 'lit'; value: string } | { kind: 'expr'; value: string }
function splitTemplateLiteral(raw: string): Segment[] {
  const segments: Segment[] = []
  let buf = ''
  let i = 0
  while (i < raw.length) {
    if (raw[i] === '$' && raw[i + 1] === '{') {
      if (buf) {
        segments.push({ kind: 'lit', value: buf })
        buf = ''
      }
      let depth = 1
      let j = i + 2
      const exprStart = j
      while (j < raw.length && depth > 0) {
        if (raw[j] === '{') depth++
        else if (raw[j] === '}') depth--
        if (depth > 0) j++
      }
      segments.push({ kind: 'expr', value: raw.slice(exprStart, j) })
      i = j + 1
    } else {
      buf += raw[i]
      i++
    }
  }
  if (buf) segments.push({ kind: 'lit', value: buf })
  return segments
}

// The core resolver: given a raw JS/TS expression's source text, returns
// every concrete string literal it could evaluate to. A truly opaque
// runtime value (a function parameter, loop variable, or prop -- anything
// with no local `const`/`function` definition to inline) becomes a single
// unique sentinel placeholder standing in for "any value"; downstream
// consumers turn that into a prefix/suffix or wildcard-segment match
// instead of an exact-string match. Anything this can't classify throws,
// per this file's "fail loudly, never skip silently" rule.
function resolveLiteralAlternatives(exprRaw: string, content: string, file: string, line: number): string[] {
  const expr = exprRaw.trim()

  const singleQuoted = expr.match(/^'([^'\\]*(?:\\.[^'\\]*)*)'$/)
  if (singleQuoted) return [singleQuoted[1]]
  const doubleQuoted = expr.match(/^"([^"\\]*(?:\\.[^"\\]*)*)"$/)
  if (doubleQuoted) return [doubleQuoted[1]]

  if (expr.length >= 2 && expr[0] === '`' && expr[expr.length - 1] === '`') {
    const segments = splitTemplateLiteral(expr.slice(1, -1))
    let versions = ['']
    for (const seg of segments) {
      if (seg.kind === 'lit') {
        versions = versions.map((v) => v + seg.value)
      } else {
        const choices = resolveLiteralAlternatives(seg.value, content, file, line)
        versions = versions.flatMap((v) => choices.map((c) => v + c))
      }
    }
    return versions
  }

  const ternary = splitTopLevelTernary(expr)
  if (ternary) {
    return [
      ...resolveLiteralAlternatives(ternary.a, content, file, line),
      ...resolveLiteralAlternatives(ternary.b, content, file, line),
    ]
  }

  const callMatch = expr.match(/^([A-Za-z_$][\w$]*)\(([\s\S]*)\)$/)
  if (callMatch) {
    return resolveFunctionReturns(callMatch[1], content, file, line)
  }

  if (/^[A-Za-z_$][\w$]*$/.test(expr)) {
    const resolved = tryResolveConst(expr, content, file, line)
    if (resolved) return resolved
    const idBare = sentinelCounter++
    return [`\u0000${idBare}\u0000`]
  }

  // A member-access chain (`m.sender`, `provider.name`) is always a runtime
  // property read, never a local const this file could inline -- opaque.
  if (/^[A-Za-z_$][\w$]*(\.[A-Za-z_$][\w$]*)+$/.test(expr)) {
    const id = sentinelCounter++
    return [`\u0000${id}\u0000`]
  }

  throw new Error(
    `${relPath(file)}:${line}: cannot resolve expression \`${expr}\` to a literal string -- ` +
      `extend resolveLiteralAlternatives() in styleAndI18nConsistency.test.ts before trusting this ` +
      `file's consistency result (refusing to silently skip it)`,
  )
}

// ---------------------------------------------------------------------------
// CSS class selectors <-> TSX consumers
// ---------------------------------------------------------------------------

function extractCssSelectors(): Map<string, { file: string; line: number }> {
  const selectors = new Map<string, { file: string; line: number }>()
  for (const file of walk(SRC_ROOT, ['.css'])) {
    const raw = fs.readFileSync(file, 'utf8')
    const noComments = raw.replace(/\/\*[\s\S]*?\*\//g, (m) => m.replace(/[^\n]/g, ' '))
    const re = /\.([A-Za-z_][\w-]*)/g
    let m: RegExpExecArray | null
    while ((m = re.exec(noComments))) {
      const cls = m[1]
      if (!selectors.has(cls)) {
        selectors.set(cls, { file, line: lineAt(noComments, m.index) })
      }
    }
  }
  return selectors
}

type Use = { file: string; line: number; alternatives: string[] }

function extractClassNameUses(files: string[]): Use[] {
  const uses: Use[] = []
  for (const file of files) {
    const content = stripJsComments(fs.readFileSync(file, 'utf8'))
    const re = /className=(\{|")/g
    let m: RegExpExecArray | null
    while ((m = re.exec(content))) {
      const line = lineAt(content, m.index)
      if (m[1] === '"') {
        const endQuote = content.indexOf('"', m.index + m[0].length)
        uses.push({ file, line, alternatives: [content.slice(m.index + m[0].length, endQuote)] })
        re.lastIndex = endQuote + 1
      } else {
        const openBrace = m.index + m[0].length - 1
        const closeBrace = findMatchingBracket(content, openBrace)
        const expr = content.slice(openBrace + 1, closeBrace)
        uses.push({ file, line, alternatives: resolveLiteralAlternatives(expr, content, file, line) })
        re.lastIndex = closeBrace + 1
      }
    }
  }
  return uses
}

type ResolvedToken = { kind: 'concrete'; value: string } | { kind: 'pattern'; prefix: string; suffix: string }
function tokensFromVersion(version: string): ResolvedToken[] {
  return version
    .split(/\s+/)
    .filter((t) => t.length > 0)
    .map((tok): ResolvedToken => {
      const sentinelMatch = tok.match(SENTINEL_RE)
      if (!sentinelMatch) return { kind: 'concrete', value: tok }
      const idx = tok.indexOf(sentinelMatch[0])
      return { kind: 'pattern', prefix: tok.slice(0, idx), suffix: tok.slice(idx + sentinelMatch[0].length) }
    })
}

function checkCssConsistency(): string[] {
  const failures: string[] = []
  const cssSelectors = extractCssSelectors()
  const tsxFiles = walk(SRC_ROOT, ['.tsx', '.ts']).filter(
    (f) => !f.endsWith('.test.ts') && !f.endsWith('bindings.ts'),
  )
  const uses = extractClassNameUses(tsxFiles)

  const consumed = new Set<string>()
  const patternRules: Array<{ prefix: string; suffix: string; file: string; line: number }> = []

  for (const use of uses) {
    for (const version of use.alternatives) {
      for (const token of tokensFromVersion(version)) {
        if (token.kind === 'concrete') consumed.add(token.value)
        else patternRules.push({ ...token, file: use.file, line: use.line })
      }
    }
  }

  for (const rule of patternRules) {
    const matches = [...cssSelectors.keys()].filter(
      (c) =>
        c.startsWith(rule.prefix) && c.endsWith(rule.suffix) && c.length >= rule.prefix.length + rule.suffix.length,
    )
    if (matches.length === 0) {
      failures.push(
        `${relPath(rule.file)}:${rule.line}: dynamic className pattern "${rule.prefix}*${rule.suffix}" matches no CSS class`,
      )
    } else {
      for (const m of matches) consumed.add(m)
    }
  }

  for (const [cls, loc] of cssSelectors) {
    if (!consumed.has(cls) && !PRE_EXISTING_ORPHAN_CLASSES.includes(cls)) {
      failures.push(`${relPath(loc.file)}:${loc.line}: CSS class ".${cls}" has no TSX consumer`)
    }
  }

  for (const use of uses) {
    for (const version of use.alternatives) {
      for (const token of tokensFromVersion(version)) {
        if (
          token.kind === 'concrete' &&
          !cssSelectors.has(token.value) &&
          !PRE_EXISTING_ORPHAN_CLASSES.includes(token.value)
        ) {
          failures.push(`${relPath(use.file)}:${use.line}: className "${token.value}" has no matching CSS selector`)
        }
      }
    }
  }

  return failures
}

// ---------------------------------------------------------------------------
// en.json i18n keys <-> t() call sites
// ---------------------------------------------------------------------------

type JsonNode = string | { [k: string]: JsonNode }

function flattenLeafPaths(node: JsonNode, prefix: string[] = [], out: string[][] = []): string[][] {
  if (typeof node === 'string') {
    out.push(prefix)
    return out
  }
  for (const [k, v] of Object.entries(node)) flattenLeafPaths(v, [...prefix, k], out)
  return out
}

type PatternSegment = { kind: 'lit'; value: string } | { kind: 'wild' }
function pathToPattern(dotted: string): PatternSegment[] {
  return dotted.split('.').map((seg) => (SENTINEL_RE.test(seg) ? { kind: 'wild' } : { kind: 'lit', value: seg }))
}

function patternMatchesAnyLeaf(pattern: PatternSegment[], leafPaths: string[][]): boolean {
  return leafPaths.some(
    (leaf) => leaf.length === pattern.length && pattern.every((p, i) => p.kind === 'wild' || p.value === leaf[i]),
  )
}

function markMatchingLeavesUsed(pattern: PatternSegment[], leafPaths: string[][], used: Set<string>): void {
  for (const leaf of leafPaths) {
    if (leaf.length !== pattern.length) continue
    if (pattern.every((p, i) => p.kind === 'wild' || p.value === leaf[i])) used.add(leaf.join('.'))
  }
}

function extractTCallUses(files: string[]): Use[] {
  const uses: Use[] = []
  for (const file of files) {
    const content = stripJsComments(fs.readFileSync(file, 'utf8'))
    const re = /\bt\(/g
    let m: RegExpExecArray | null
    while ((m = re.exec(content))) {
      const openParen = m.index + m[0].length - 1
      const line = lineAt(content, m.index)
      const argText = extractFirstCallArg(content, openParen)
      uses.push({ file, line, alternatives: resolveLiteralAlternatives(argText, content, file, line) })
      re.lastIndex = openParen + 1
    }
  }
  return uses
}

function checkI18nConsistency(): string[] {
  const failures: string[] = []
  const en = JSON.parse(fs.readFileSync(path.join(SRC_ROOT, 'i18n', 'en.json'), 'utf8')) as JsonNode
  const leafPaths = flattenLeafPaths(en)
  const tsxFiles = walk(SRC_ROOT, ['.tsx', '.ts']).filter(
    (f) => !f.endsWith('.test.ts') && !f.endsWith('bindings.ts'),
  )
  const uses = extractTCallUses(tsxFiles)
  const used = new Set<string>()

  for (const use of uses) {
    for (const version of use.alternatives) {
      const pattern = pathToPattern(version)
      if (!patternMatchesAnyLeaf(pattern, leafPaths)) {
        failures.push(
          `${relPath(use.file)}:${use.line}: t() key pattern "${version.replace(SENTINEL_RE, '*')}" matches no en.json key`,
        )
      } else {
        markMatchingLeavesUsed(pattern, leafPaths, used)
      }
    }
  }

  for (const leaf of leafPaths) {
    const dotted = leaf.join('.')
    if (!used.has(dotted) && !PRE_EXISTING_ORPHAN_KEYS.includes(dotted)) {
      failures.push(`i18n/en.json: key "${dotted}" has no t() call site`)
    }
  }

  return failures
}

// ---------------------------------------------------------------------------

const cssFailures = checkCssConsistency()
const i18nFailures = checkI18nConsistency()
const allFailures = [...cssFailures, ...i18nFailures]

assert.equal(
  allFailures.length,
  0,
  `\n${allFailures.length} CSS/i18n consistency failure(s):\n` + allFailures.map((f) => `  - ${f}`).join('\n'),
)

console.log('styleAndI18nConsistency.test.ts: all assertions passed')
