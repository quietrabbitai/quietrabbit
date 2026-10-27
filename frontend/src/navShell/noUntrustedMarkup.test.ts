// items.id=826 -- watcher for the property items.id=690 found: no untrusted
// content (model output, ingested documents, Library entries, third-party
// pane pages) can run script, navigate or call IPC in the main webview,
// because the frontend has no way to turn a string into live markup. Message
// and document text render as React text nodes; there are no anchors,
// window.open calls or markdown/sanitizer libraries.
//
// This test fails, naming file:line and the rule, when that stops being
// true. A failure here means items.id=666 (backend-opened file dialog
// returning an opaque token) must be reviewed BEFORE the failure is
// silenced: its trigger is "untrusted content can reach the main webview".
// Read items.id=666's trigger and items.id=690's verdict first; only then
// add an ALLOWED entry below, with a one-line reason.
//
//   node --experimental-strip-types src/navShell/noUntrustedMarkup.test.ts
//
// (also wired as part of `npm test`.) Scans frontend/src/**/*.ts(x) with the
// TypeScript AST (skipping bindings.ts and *.test.* files), so comments,
// string literals and JSX text never match; plus the direct dependency names
// in frontend/package.json.
//
// WHAT THIS CANNOT CATCH (read before trusting a green run):
// - Markup built from a string through a library function this test does
//   not name: a markdown/HTML library missing from FORBIDDEN_PACKAGES, the
//   react-i18next <Trans> components map, internals of @lobehub/icons.
// - Dependency changes in a transitive package. Only the direct
//   dependencies / devDependencies / optionalDependencies / peerDependencies
//   names in package.json are checked. package-lock.json is out of scope
//   (npm ci already fails on a package.json/lockfile mismatch).
// - Anything outside frontend/src: index.html, vite.config.ts, public/, the
//   CSP in tauri.conf.json, Rust-side webview and CEF pane code.
// - Dynamic property access it cannot resolve statically. el["inner"+"HTML"]
//   is caught when every part is a string literal; el[name] with a variable
//   `name`, Reflect.get(el, name), Object.assign(el, {...}) built from
//   computed keys, and keys assembled at runtime are not.
// - Aliased calls after the alias: `const f = Function` is flagged at the
//   alias (the identifier), later `f(...)` calls are not separate findings.
// - Non-TS sources are only tripwired by extension (any .js/.jsx/.mjs/.html/
//   .md/.mdx file under src/ fails the test); SVG imported with ?raw into a
//   string and then used as markup is not recognised.
// - Attacker-controlled data reaching the DOM through a React feature that
//   is not a sink named here (e.g. an <img src> or style string). That is
//   the CSP's job (img-src 'self', default-src 'self'), not this test's.
import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import ts from 'typescript'

const SRC_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const DEFAULT_PACKAGE_JSON = path.resolve(SRC_ROOT, '..', 'package.json')
const REVIEW_NOTE = "review items.id=666's trigger and items.id=690's verdict before adding an allowlist entry"

// ---------------------------------------------------------------------------
// Allowlist. Keyed by file (relative to frontend/src) + rule id, not by line
// number, so edits above a use do not invalidate it. Every entry needs a
// one-line reason. An entry that no longer matches any finding fails the
// test, so the list cannot rot. Empty on purpose: the first run against
// frontend/src (items.id=826) had zero hits.
// ---------------------------------------------------------------------------
type AllowedEntry = { file: string; rule: string; reason: string }
const ALLOWED: AllowedEntry[] = []

// ---------------------------------------------------------------------------
// Source rules
// ---------------------------------------------------------------------------
const SINK_IDENTIFIERS = new Set([
  'dangerouslySetInnerHTML',
  'innerHTML',
  'outerHTML',
  'insertAdjacentHTML',
  'srcDoc',
  'srcdoc',
  'createContextualFragment',
  'DOMParser',
  'location',
])
const GLOBAL_OBJECTS = new Set(['window', 'document', 'globalThis', 'self'])
const FORBIDDEN_TAGS = new Set(['a', 'iframe', 'object', 'embed', 'script'])
const SINK_TEXT_RE = /inner|outer|html|srcdoc|write|open|eval|function|fragment|href|location/i
const NON_TS_EXTENSIONS = ['.js', '.jsx', '.mjs', '.html', '.md', '.mdx']

type Finding = { file: string; line: number; rule: string; text: string }

function walk(dir: string, out: string[] = []): string[] {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name)
    if (entry.isDirectory()) walk(full, out)
    else out.push(full)
  }
  return out
}

function relPath(p: string): string {
  return path.relative(SRC_ROOT, p)
}

function isTsSource(file: string): boolean {
  const name = path.basename(file)
  return /\.tsx?$/.test(name) && !/\.test\./.test(name) && name !== 'bindings.ts'
}

// Static text of a string literal, a `+` concatenation of literals, or a
// template literal; null when the expression has no statically known part.
function literalText(expr: ts.Expression): string | null {
  if (ts.isStringLiteralLike(expr)) return expr.text
  if (ts.isParenthesizedExpression(expr)) return literalText(expr.expression)
  if (ts.isBinaryExpression(expr) && expr.operatorToken.kind === ts.SyntaxKind.PlusToken) {
    const left = literalText(expr.left)
    const right = literalText(expr.right)
    return left === null && right === null ? null : (left ?? '') + (right ?? '')
  }
  if (ts.isTemplateExpression(expr)) {
    return expr.head.text + expr.templateSpans.map((span) => span.literal.text).join('')
  }
  return null
}

function isGlobalMember(node: ts.PropertyAccessExpression, names: string[]): boolean {
  return (
    ts.isIdentifier(node.expression) && GLOBAL_OBJECTS.has(node.expression.text) && names.includes(node.name.text)
  )
}

function scanSource(file: string, content: string): Finding[] {
  const sf = ts.createSourceFile(
    file,
    content,
    ts.ScriptTarget.Latest,
    true,
    file.endsWith('x') ? ts.ScriptKind.TSX : ts.ScriptKind.TS,
  )
  const lines = content.split('\n')
  const findings: Finding[] = []
  const report = (node: ts.Node, rule: string) => {
    const line = sf.getLineAndCharacterOfPosition(node.getStart(sf)).line + 1
    findings.push({ file: relPath(file), line, rule, text: (lines[line - 1] ?? '').trim() })
  }

  const visit = (node: ts.Node): void => {
    if (ts.isIdentifier(node) && SINK_IDENTIFIERS.has(node.text)) report(node, `identifier ${node.text}`)

    if (ts.isPropertyAccessExpression(node)) {
      if (isGlobalMember(node, ['write', 'writeln'])) report(node, `document.${node.name.text}`)
      if (isGlobalMember(node, ['open'])) report(node, 'window.open')
      if (isGlobalMember(node, ['eval'])) report(node, 'eval')
      if (isGlobalMember(node, ['Function'])) report(node, 'Function constructor')
      if (node.name.text === 'href') report(node, 'href property')
    }

    if (ts.isCallExpression(node) && ts.isIdentifier(node.expression)) {
      const callee = node.expression.text
      if (callee === 'eval') report(node, 'eval')
      if (callee === 'Function') report(node, 'Function constructor')
      const first = node.arguments[0]
      if ((callee === 'setTimeout' || callee === 'setInterval') && first && ts.isStringLiteralLike(first)) {
        report(node, `${callee} with a string`)
      }
    }

    if (ts.isNewExpression(node) && ts.isIdentifier(node.expression) && node.expression.text === 'Function') {
      report(node, 'new Function')
    }

    if ((ts.isPropertyAssignment(node) || ts.isShorthandPropertyAssignment(node)) && node.name.getText(sf) === 'href') {
      report(node, 'href object key')
    }

    if (ts.isElementAccessExpression(node)) {
      const text = literalText(node.argumentExpression)
      if (text !== null && SINK_TEXT_RE.test(text)) report(node, `element access ["${text}"]`)
    }

    if (ts.isJsxOpeningElement(node) || ts.isJsxSelfClosingElement(node)) {
      const tag = node.tagName.getText(sf)
      if (FORBIDDEN_TAGS.has(tag.toLowerCase())) report(node, `<${tag}>`)
      for (const attr of node.attributes.properties) {
        if (ts.isJsxAttribute(attr) && attr.name.getText(sf) === 'href') report(attr, 'href attribute')
      }
    }

    ts.forEachChild(node, visit)
  }
  visit(sf)
  return findings
}

function checkSources(): Finding[] {
  const all = walk(SRC_ROOT)
  const findings: Finding[] = []
  for (const file of all) {
    if (NON_TS_EXTENSIONS.some((ext) => file.endsWith(ext))) {
      findings.push({ file: relPath(file), line: 1, rule: 'non-TypeScript source/markup file under src/', text: '' })
    }
  }
  for (const file of all.filter(isTsSource)) findings.push(...scanSource(file, fs.readFileSync(file, 'utf8')))
  return findings
}

// ---------------------------------------------------------------------------
// Dependency rule: direct package names only (see cannot-catch list).
// ---------------------------------------------------------------------------
const FORBIDDEN_PACKAGES: Array<{ pattern: RegExp; label: string }> = [
  { pattern: /^marked$/, label: 'marked' },
  { pattern: /^markdown-it/, label: 'markdown-it*' },
  { pattern: /^react-markdown$/, label: 'react-markdown' },
  { pattern: /^remark/, label: 'remark*' },
  { pattern: /^rehype/, label: 'rehype*' },
  { pattern: /^micromark/, label: 'micromark*' },
  { pattern: /^mdx/, label: 'mdx*' },
  { pattern: /^@mdx-js\//, label: '@mdx-js/*' },
  { pattern: /^showdown$/, label: 'showdown' },
  { pattern: /^(isomorphic-)?dompurify$/, label: 'dompurify' },
  { pattern: /^sanitize-html$/, label: 'sanitize-html' },
  { pattern: /^highlight\.js$/, label: 'highlight.js' },
  { pattern: /^prismjs$/, label: 'prismjs' },
  { pattern: /^katex$/, label: 'katex' },
  { pattern: /^mermaid$/, label: 'mermaid' },
  { pattern: /^(unified|mdast-|hast-|unist-)/, label: 'unified / mdast-* / hast-* / unist-*' },
  { pattern: /^(markdown-to-jsx|commonmark|snarkdown|turndown)$/, label: 'markdown converter' },
  {
    pattern: /^(xss|html-react-parser|react-html-parser|htmlparser2|parse5|cheerio|jsdom)$/,
    label: 'HTML parser/sanitizer',
  },
  { pattern: /^(react-syntax-highlighter|prism-react-renderer|shiki)$/, label: 'syntax highlighter' },
  { pattern: /^@tiptap\//, label: '@tiptap/*' },
  { pattern: /^prosemirror-/, label: 'prosemirror-*' },
  { pattern: /^(lexical|@lexical\/)/, label: 'lexical' },
  { pattern: /^slate/, label: 'slate*' },
  { pattern: /^(draft-js|quill|react-quill|trix|tinymce)$/, label: 'rich-text editor' },
  {
    pattern: /^(@codemirror\/|codemirror$|monaco-editor$|@monaco-editor\/|ckeditor|@editorjs\/)/,
    label: 'code/rich-text editor',
  },
]
const DEPENDENCY_FIELDS = ['dependencies', 'devDependencies', 'optionalDependencies', 'peerDependencies']

function checkDependencies(packageJsonPath: string): Finding[] {
  const pkg = JSON.parse(fs.readFileSync(packageJsonPath, 'utf8')) as Record<string, Record<string, string> | undefined>
  const findings: Finding[] = []
  for (const field of DEPENDENCY_FIELDS) {
    for (const name of Object.keys(pkg[field] ?? {})) {
      const hit = FORBIDDEN_PACKAGES.find((p) => p.pattern.test(name))
      if (hit) {
        findings.push({ file: 'package.json', line: 1, rule: `${field} "${name}" matches ${hit.label}`, text: '' })
      }
    }
  }
  return findings
}

// ---------------------------------------------------------------------------
// Allowlist application: drop allowed findings, fail on stale entries.
// ---------------------------------------------------------------------------
function applyAllowlist(findings: Finding[]): { remaining: Finding[]; stale: AllowedEntry[] } {
  const used = new Set<AllowedEntry>()
  const remaining = findings.filter((f) => {
    const entry = ALLOWED.find((a) => a.file === f.file && a.rule === f.rule)
    if (entry) used.add(entry)
    return !entry
  })
  return { remaining, stale: ALLOWED.filter((a) => !used.has(a)) }
}

// ---------------------------------------------------------------------------

for (const entry of ALLOWED) {
  assert.ok(entry.reason.trim().length > 0, `ALLOWED entry ${entry.file} / ${entry.rule} needs a one-line reason`)
}

// Optional argv[2]: a package.json to check instead of frontend/package.json
// (used to demonstrate the dependency rule against a scratch copy).
const packageJsonPath = process.argv[2] ? path.resolve(process.argv[2]) : DEFAULT_PACKAGE_JSON
const { remaining, stale } = applyAllowlist([...checkSources(), ...checkDependencies(packageJsonPath)])
const messages = [
  ...remaining.map((f) => `${f.file}:${f.line}: ${f.rule}${f.text ? ` -- ${f.text}` : ''}`),
  ...stale.map((a) => `stale ALLOWED entry (no longer matches anything, remove it): ${a.file} / ${a.rule}`),
]

assert.equal(
  messages.length,
  0,
  `\n${messages.length} untrusted-markup finding(s):\n` +
    messages.map((m) => `  - ${m}`).join('\n') +
    `\n\n${REVIEW_NOTE}`,
)

console.log('noUntrustedMarkup.test.ts: all assertions passed')
