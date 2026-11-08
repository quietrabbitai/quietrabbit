// State for the Cloud Chat navigation prompts (items.id=873, decisions.id=934):
// the "open this link in your browser?" confirm and the blocked-link notice that
// the Rust pane host raises through three events. A plain reducer so the rules
// are testable without a DOM (navPromptState.test.ts).
//
// The frontend never holds or supplies a URL: the confirm carries a request id
// and the host to show. The URL stays in Rust (answer_cloud_chat_nav_confirm).

export interface NavConfirm {
  requestId: string
  providerId: string
  host: string
}

export interface NavNotice {
  providerId: string
  /** The blocked scheme, or null for a malformed link. */
  scheme: string | null
}

export interface NavPromptState {
  confirm: NavConfirm | null
  notice: NavNotice | null
}

export const initialNavPromptState: NavPromptState = { confirm: null, notice: null }

export type NavPromptEvent =
  | { type: 'confirm'; confirm: NavConfirm }
  | { type: 'confirm-cancelled'; providerId: string }
  | { type: 'answered'; requestId: string }
  | { type: 'notice'; notice: NavNotice }
  | { type: 'notice-dismissed' }

export function reduceNavPrompt(state: NavPromptState, event: NavPromptEvent): NavPromptState {
  switch (event.type) {
    case 'confirm':
      // A newer request replaces an older one (Rust keeps one per pane and
      // reports the replaced one as cancelled).
      return { ...state, confirm: event.confirm }
    case 'confirm-cancelled':
      // The pane closed or its request was replaced: dismiss only that pane's.
      return state.confirm?.providerId === event.providerId ? { ...state, confirm: null } : state
    case 'answered':
      return state.confirm?.requestId === event.requestId ? { ...state, confirm: null } : state
    case 'notice':
      return { ...state, notice: event.notice }
    case 'notice-dismissed':
      return state.notice === null ? state : { ...state, notice: null }
  }
}
