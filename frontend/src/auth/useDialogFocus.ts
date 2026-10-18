// Focus handling for the app's modal panels (items.id=769 stage 2): focus
// moves into the dialog on open, Tab and Shift+Tab cycle within it, Escape
// calls `onEscape`, and focus returns to whatever had it before on close.
//
// Not <dialog>.showModal(): a showModal dialog sits in the browser's top layer,
// above every z-index, which would hide the key-rotation overlay (a normal
// fixed layer) behind the panel that started the rotation. Pass `active=false`
// while that overlay is up so only one of the two handles the keyboard.
//
// `onEscape` is read through a ref, so the keyboard effect depends only on the
// element and `active`: a parent re-render (a new callback identity) never
// re-runs it and so never moves focus. Focus goes back to the opener only when
// the dialog unmounts, not each time `active` toggles.

import { useEffect, useRef, type RefObject } from 'react'

const FOCUSABLE =
  'button:not([disabled]), input:not([disabled]):not([type="hidden"]):not([tabindex="-1"]), select:not([disabled]), textarea:not([disabled]), a[href], [tabindex]:not([tabindex="-1"])'

export function useDialogFocus(
  ref: RefObject<HTMLElement | null>,
  active: boolean,
  onEscape: () => void,
): void {
  const onEscapeRef = useRef(onEscape)
  useEffect(() => {
    onEscapeRef.current = onEscape
  })

  // Give focus back to the opener when the dialog goes away.
  useEffect(() => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null
    return () => opener?.focus()
  }, [])

  useEffect(() => {
    const dialog = ref.current
    if (!dialog || !active) return
    const focusables = () => Array.from(dialog.querySelectorAll<HTMLElement>(FOCUSABLE))
    if (!dialog.contains(document.activeElement)) {
      const first = focusables()[0]
      if (first) first.focus()
      else dialog.focus()
    }
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault()
        onEscapeRef.current()
        return
      }
      if (event.key !== 'Tab') return
      const items = focusables()
      if (items.length === 0) {
        event.preventDefault()
        dialog.focus()
        return
      }
      const first = items[0]
      const last = items[items.length - 1]
      const current = document.activeElement
      if (event.shiftKey && (current === first || !dialog.contains(current))) {
        event.preventDefault()
        last.focus()
      } else if (!event.shiftKey && (current === last || !dialog.contains(current))) {
        event.preventDefault()
        first.focus()
      }
    }
    document.addEventListener('keydown', onKeyDown)
    return () => document.removeEventListener('keydown', onKeyDown)
  }, [ref, active])
}
