// items.id=257 Path B -- invisible per-pane pointer hit-layer, replacing the
// GDK input-shape click-routing mechanism (see pane_host.rs's module doc
// for why that mechanism froze the whole client's Wayland pointer input and
// had to go). One absolutely-positioned, invisible <div> per open pane,
// positioned from the exact same PanePixelRect CloudChatAccessPane already
// computes for the PaneRectFraction it sends to Rust via set_pane_layout --
// see paneLayout.ts's own module doc for why that's one computation, not
// two independently-maintained ones. The browser's own native hit-testing
// (a real DOM element, real pointer events, no synthesis) decides which
// pane a given event belongs to; this component only forwards what already
// landed on it over the forward_pane_mouse_click/_move/_wheel/_key IPC
// commands.
//
// Keyboard forwarding (items.id=332): the tabIndex/.focus() below, already
// in place before keyboard support existed (see onPointerDown), is what
// gives this div real DOM focus to receive onKeyDown/onKeyUp from -- no
// separate focus wiring needed. See forward_pane_key's own doc
// (commands/tier3_pane.rs) for the windows_key_code/native_key_code
// scoping.

import { useCallback, useEffect, useRef } from 'react'
import {
  commands,
  type PaneEventModifiers,
  type PaneKeyEventType,
  type PaneMouseButton,
  type ZoomDirection,
} from '../bindings'
import type { PanePixelRect } from './paneLayout'

export interface PaneHitLayerProps {
  rects: Record<string, PanePixelRect>
}

/** Matches pane_host.rs's own `PIXELS_PER_SCROLL_UNIT` (the GDK-path
 *  constant this mirrors) for `DOM_DELTA_LINE`-mode wheel events. Sign/scale
 *  not independently re-verified against a real scroll gesture this
 *  session -- same caveat the GDK path this replaces already carried. */
const PIXELS_PER_LINE = 40

function wheelDeltaPixels(e: WheelEvent): { dx: number; dy: number } {
  const scale = e.deltaMode === 1 ? PIXELS_PER_LINE : e.deltaMode === 2 ? window.innerHeight : 1
  return { dx: e.deltaX * scale, dy: e.deltaY * scale }
}

function domModifiers(e: {
  shiftKey: boolean
  ctrlKey: boolean
  altKey: boolean
  metaKey: boolean
}): PaneEventModifiers {
  return { shift: e.shiftKey, ctrl: e.ctrlKey, alt: e.altKey, meta: e.metaKey }
}

/** DOM's `PointerEvent.button` (0/1/2 = left/middle/right) -> the enum
 *  `forward_pane_mouse_click` expects. `null` for buttons with no CEF
 *  equivalent (DOM also reports 3/4 for back/forward) -- same policy
 *  `cef_mouse_button_from_gdk` already applied to GDK's own out-of-range
 *  button numbers, just made on this side of the IPC boundary instead. */
function domButton(button: number): PaneMouseButton | null {
  switch (button) {
    case 0:
      return 'Left'
    case 1:
      return 'Middle'
    case 2:
      return 'Right'
    default:
      return null
  }
}

const PROVIDER_ID_ATTR = 'data-provider-id'

/** items.id=332: DOM `KeyboardEvent.keyCode` is deprecated but still
 *  populated by every engine following the long-standing web-platform
 *  convention of matching Windows virtual-key codes -- exactly what CEF's
 *  `KeyEvent::windows_key_code` expects regardless of platform. See
 *  `forward_pane_key`'s own doc (commands/tier3_pane.rs) for why there's no
 *  real native/hardware code to send alongside it. */
function forwardKey(providerId: string, e: KeyboardEvent, type: PaneKeyEventType) {
  const character = e.key.length === 1 ? e.key.charCodeAt(0) : 0
  void commands.forwardPaneKey(providerId, type, e.keyCode, character, domModifiers(e))
}

/** items.id=364: Ctrl+=/Ctrl+-/Ctrl+0 are intercepted here rather than
 *  forwarded to CEF as ordinary keys -- a Chrome-runtime CEF browser has no
 *  window chrome of its own to bind these as a zoom accelerator (that's
 *  normally a browser-UI concern), so this app applies the zoom itself via
 *  `adjustPaneZoom` (see `ZoomDirection`'s own doc, commands/tier3_pane.rs).
 *  `'='`/`'+'` both map to zoom-in since the physical key that types '+' on
 *  a US layout requires Shift, and browsers report that combo's `e.key` as
 *  `'+'`, not `'='`. */
function zoomDirectionForKey(e: KeyboardEvent): ZoomDirection | null {
  if (!e.ctrlKey) return null
  switch (e.key) {
    case '=':
    case '+':
      return 'In'
    case '-':
      return 'Out'
    case '0':
      return 'Reset'
    default:
      return null
  }
}

export function PaneHitLayer({ rects }: PaneHitLayerProps) {
  const wrapperRef = useRef<HTMLDivElement | null>(null)

  // Coalesces pointermove to at most one forwarded event per animation
  // frame, per pane (keyed by provider id so one pane's drag doesn't stall
  // another's) -- see forward_pane_mouse_move's own doc for why: the native
  // GDK path this replaces ran in-process at whatever rate the OS reported,
  // this path crosses an IPC boundary per call, so batching to the frame
  // rate the compositor can actually show avoids flooding it without a
  // perceptible behavior change. pointerdown/pointerup/wheel are NOT
  // throttled -- dropping or coalescing those would be a real correctness
  // change (click count, drag start/end, scroll-momentum fidelity), not
  // just a perf one.
  //
  // The queued move's args live in this ref (not just inside the RAF
  // closure) so a same-pane click/release/cancel can flush it out of order
  // -- otherwise a still-queued move could reach Rust after a click that
  // happened later in real time, inverting the true event sequence on the
  // wire. Each call updates the pending entry in place so the frame, when
  // it fires, always sends the latest known position rather than the first.
  const pendingMove = useRef<
    Record<
      string,
      {
        rafId: number
        x: number
        y: number
        leaving: boolean
        buttons: number
        modifiers: PaneEventModifiers
      }
    >
  >({})

  // Last button pressed per pane that hasn't seen its matching pointerup
  // yet -- used to synthesize a balanced release if the pointer is instead
  // cancelled (see onPointerCancel).
  const lastButtonDown = useRef<Record<string, { button: PaneMouseButton; detail: number }>>({})

  const scheduleMove = useCallback(
    (
      providerId: string,
      x: number,
      y: number,
      leaving: boolean,
      buttons: number,
      modifiers: PaneEventModifiers,
    ) => {
      const pending = pendingMove.current[providerId]
      if (pending) {
        pending.x = x
        pending.y = y
        pending.leaving = leaving
        pending.buttons = buttons
        pending.modifiers = modifiers
        return
      }
      const entry = { rafId: 0, x, y, leaving, buttons, modifiers }
      entry.rafId = requestAnimationFrame(() => {
        delete pendingMove.current[providerId]
        void commands.forwardPaneMouseMove(providerId, entry.x, entry.y, entry.leaving, entry.buttons, entry.modifiers)
      })
      pendingMove.current[providerId] = entry
    },
    [],
  )

  // Forwards a still-queued move for this pane immediately, ahead of a
  // click/release/cancel about to be sent for the same pane, so the two
  // reach Rust in the order they actually happened.
  const flushPendingMove = useCallback((providerId: string) => {
    const pending = pendingMove.current[providerId]
    if (!pending) return
    cancelAnimationFrame(pending.rafId)
    delete pendingMove.current[providerId]
    void commands.forwardPaneMouseMove(
      providerId,
      pending.x,
      pending.y,
      pending.leaving,
      pending.buttons,
      pending.modifiers,
    )
  }, [])

  // Wheel needs a real (non-passive) native listener, not React's onWheel:
  // React registers wheel (and touchstart/touchmove) handlers as passive by
  // default, silently making preventDefault() a no-op -- and preventDefault
  // is required here to stop the webview's own page from scrolling in
  // response to what's meant to be a pane-local scroll. Attached once on
  // the stable wrapper (delegation, not one listener per pane div) and
  // resolved back to a pane via the target's own data-provider-id, read
  // fresh on every event -- so this effect doesn't need to re-run, or even
  // know about, individual pane add/remove.
  useEffect(() => {
    const wrapper = wrapperRef.current
    if (!wrapper) return
    const handler = (e: WheelEvent) => {
      const providerId = (e.target as HTMLElement | null)?.getAttribute(PROVIDER_ID_ATTR)
      if (!providerId) return
      e.preventDefault()
      const { dx, dy } = wheelDeltaPixels(e)
      void commands.forwardPaneMouseWheel(providerId, e.offsetX, e.offsetY, dx, dy, domModifiers(e))
    }
    wrapper.addEventListener('wheel', handler, { passive: false })
    return () => wrapper.removeEventListener('wheel', handler)
  }, [])

  return (
    <div ref={wrapperRef} style={{ position: 'fixed', inset: 0, pointerEvents: 'none' }}>
      {Object.entries(rects).map(([providerId, rect]) => (
        <div
          key={providerId}
          {...{ [PROVIDER_ID_ATTR]: providerId }}
          tabIndex={-1}
          style={{
            position: 'absolute',
            left: rect.left,
            top: rect.top,
            width: rect.width,
            height: rect.height,
            pointerEvents: 'auto',
            touchAction: 'none',
          }}
          onPointerDown={(e) => {
            const button = domButton(e.nativeEvent.button)
            if (button === null) return
            e.currentTarget.setPointerCapture(e.pointerId)
            e.currentTarget.focus()
            const detail = e.nativeEvent.detail || 1
            flushPendingMove(providerId)
            lastButtonDown.current[providerId] = { button, detail }
            void commands.forwardPaneMouseClick(
              providerId,
              e.nativeEvent.offsetX,
              e.nativeEvent.offsetY,
              button,
              false,
              detail,
              e.nativeEvent.buttons,
              domModifiers(e.nativeEvent),
            )
          }}
          onContextMenu={(e) => {
            // items.id=379: without this, a right-click still reaches CEF
            // fine (forwarded via onPointerDown/onPointerUp below, same as
            // any other button) and its own context menu pipeline runs
            // correctly -- but the browser's native `contextmenu` DOM event
            // for this same click also fires, unprevented, on the *outer*
            // Tauri webview (WebKitGTK, a completely different browser than
            // CEF). Confirmed live: WebKitGTK's own default menu (Back/
            // Forward/Reload/Inspect Element) was what actually appeared on
            // screen, not the new custom gtk::Menu -- diagnostic logging
            // confirmed run_context_menu/show_context_menu were reached and
            // building the right menu the whole time.
            e.preventDefault()
          }}
          onPointerUp={(e) => {
            const button = domButton(e.nativeEvent.button)
            if (button === null) return
            flushPendingMove(providerId)
            delete lastButtonDown.current[providerId]
            void commands.forwardPaneMouseClick(
              providerId,
              e.nativeEvent.offsetX,
              e.nativeEvent.offsetY,
              button,
              true,
              e.nativeEvent.detail || 1,
              e.nativeEvent.buttons,
              domModifiers(e.nativeEvent),
            )
          }}
          onPointerCancel={(e) => {
            flushPendingMove(providerId)
            const down = lastButtonDown.current[providerId]
            if (!down) return
            delete lastButtonDown.current[providerId]
            void commands.forwardPaneMouseClick(
              providerId,
              e.nativeEvent.offsetX,
              e.nativeEvent.offsetY,
              down.button,
              true,
              down.detail,
              e.nativeEvent.buttons,
              domModifiers(e.nativeEvent),
            )
          }}
          onPointerMove={(e) => {
            scheduleMove(
              providerId,
              e.nativeEvent.offsetX,
              e.nativeEvent.offsetY,
              false,
              e.nativeEvent.buttons,
              domModifiers(e.nativeEvent),
            )
          }}
          onPointerLeave={(e) => {
            scheduleMove(
              providerId,
              e.nativeEvent.offsetX,
              e.nativeEvent.offsetY,
              true,
              e.nativeEvent.buttons,
              domModifiers(e.nativeEvent),
            )
          }}
          onKeyDown={(e) => {
            e.preventDefault()
            const zoomDirection = zoomDirectionForKey(e.nativeEvent)
            if (zoomDirection) {
              void commands.adjustPaneZoom(providerId, zoomDirection)
              return
            }
            forwardKey(providerId, e.nativeEvent, 'RawKeyDown')
            if (e.key.length === 1) {
              forwardKey(providerId, e.nativeEvent, 'Char')
            }
          }}
          onKeyUp={(e) => {
            e.preventDefault()
            forwardKey(providerId, e.nativeEvent, 'KeyUp')
          }}
        />
      ))}
    </div>
  )
}
