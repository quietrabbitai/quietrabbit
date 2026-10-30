// "Something couldn't be saved" (items.id=778, decisions.id=896 point 8). The
// backend emits `data-write-failed` with `{ kind }` when a write of the user's
// own content failed after its retries (src-tauri/src/write_failure.rs): never
// any text, id or path. The notice says what could not be saved and what the
// person can still do. Pure functions, so they run under
// `node --experimental-strip-types` (writeFailed.test.ts).

export const WRITE_FAILED_EVENT = 'data-write-failed'

/** Leaf under `auth.writeFailed.` for a kind. An unknown kind still gets the
 *  generic wording: a failure is never hidden because the app is older than
 *  the backend. */
export function writeFailedSuffix(kind: string): string {
  switch (kind) {
    case 'reply':
      return 'reply'
    case 'run':
      return 'run'
    case 'history':
      return 'history'
    case 'reply_status':
      return 'replyStatus'
    case 'personal_flag':
      return 'personalFlag'
    case 'ocr_text':
      return 'ocrText'
    case 'extracted_text':
      return 'extractedText'
    default:
      return 'generic'
  }
}

/** Every suffix writeFailedSuffix can return, for the en.json check. */
export function allWriteFailedSuffixes(): string[] {
  return [
    'extractedText',
    'generic',
    'history',
    'ocrText',
    'personalFlag',
    'reply',
    'replyStatus',
    'run',
  ]
}

/** Narrowing guard for the untyped event payload `{ kind: string }`. Returns
 *  the kind, or null for anything else (a malformed event is ignored). */
export function parseWriteFailed(payload: unknown): string | null {
  if (typeof payload !== 'object' || payload === null) return null
  const kind = (payload as Record<string, unknown>).kind
  return typeof kind === 'string' && kind.length > 0 ? kind : null
}

/** The kinds on screen after a new failure: each kind once, newest last. */
export function addFailure(current: string[], kind: string): string[] {
  return current.includes(kind) ? current : [...current, kind]
}
