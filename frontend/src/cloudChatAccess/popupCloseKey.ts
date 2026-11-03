// items.id=853: the keyboard close for a Cloud Chat popup. PopupHitLayer
// listens for this at window level, in the capture phase, while a popup is
// open, and closes the popup instead of forwarding the key to the page.
//
// Ctrl+W only, no other modifier. Bare Escape is deliberately NOT a close
// key: provider pages use it themselves (dismissing a menu or dialog inside
// the popup). Matched on `key`, not `code`, so it follows the user's layout;
// a layout whose Ctrl+W key is not "w" uses the content-head button instead.

export interface PopupCloseKeyEvent {
  key: string
  ctrlKey: boolean
  shiftKey: boolean
  altKey: boolean
  metaKey: boolean
}

export function isPopupCloseKey(e: PopupCloseKeyEvent): boolean {
  return e.ctrlKey && !e.shiftKey && !e.altKey && !e.metaKey && e.key.toLowerCase() === 'w'
}
