// items.id=234 -- host-owned popup subsystem, invisible per-popup pointer
// hit-layer. Near-copy of PaneHitLayer.tsx (see that file's own doc for the
// pointer/wheel forwarding mechanism this reuses verbatim); the two
// differences are the geometry source and the IPC commands called.
//
// Unlike PaneHitLayer's `rects` (frontend-measured `PanePixelRect`s, derived
// from real DOM `getBoundingClientRect()` calls against the dock region), a
// popup has no corresponding DOM node in this app's own page to measure --
// `window.open()` has nothing here to observe. `popups` is therefore
// backend-authoritative: the exact `PaneRectFraction` pane_host.rs's
// `resolve_popup_rect` computed and reported via the `tier3-popup-opened`
// event, trusted verbatim and rendered directly as inset percentages (no
// pixel-conversion helper needed, since there's nothing in CSS-pixel space
// to convert from).
//
// Mounted as a sibling AFTER PaneHitLayer in Tier3AccessPane's JSX -- DOM
// source order alone resolves "this click is for the popup, not its parent
// pane" in any overlapping region (later siblings hit-test on top), no
// coordinate-exclusion math needed. The parent pane's own hit-div stays
// live outside the popup's rect (see this item's plan, Judgment call 6.3).

import { useCallback, useEffect, useRef } from 'react'
import {
  commands,
  type PaneEventModifiers,
  type PaneKeyEventType,
  type PaneMouseButton,
  type PaneRectFraction,
} from '../bindings'

export interface PopupHitLayerProps {
  popups: Record<string, PaneRectFraction>
}

/** Matches PaneHitLayer's own constant/doc -- see that file. */
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

/** Same policy as PaneHitLayer's `domButton` -- see that file's own doc. */
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

const PROVIDER_ID_ATTR = 'data-popup-provider-id'

/** items.id=367: popup counterpart to PaneHitLayer's own `forwardKey` --
 *  see that file's doc for why `windows_key_code`/no-native-code. Missing
 *  entirely until now (see `forward_popup_key`'s own doc,
 *  commands/tier3_pane.rs, for the confirmed symptom this fixes). */
function forwardKey(providerId: string, e: KeyboardEvent, type: PaneKeyEventType) {
  const character = e.key.length === 1 ? e.key.charCodeAt(0) : 0
  void commands.forwardPopupKey(providerId, type, e.keyCode, character, domModifiers(e))
}

export function PopupHitLayer({ popups }: PopupHitLayerProps) {
  const wrapperRef = useRef<HTMLDivElement | null>(null)

  // Same coalesced-move/flush-on-click-or-cancel scheme as PaneHitLayer --
  // see that file's own doc for why.
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
        void commands.forwardPopupMouseMove(providerId, entry.x, entry.y, entry.leaving, entry.buttons, entry.modifiers)
      })
      pendingMove.current[providerId] = entry
    },
    [],
  )

  const flushPendingMove = useCallback((providerId: string) => {
    const pending = pendingMove.current[providerId]
    if (!pending) return
    cancelAnimationFrame(pending.rafId)
    delete pendingMove.current[providerId]
    void commands.forwardPopupMouseMove(
      providerId,
      pending.x,
      pending.y,
      pending.leaving,
      pending.buttons,
      pending.modifiers,
    )
  }, [])

  // Same non-passive-listener requirement as PaneHitLayer's own wheel
  // handling -- see that file's doc.
  useEffect(() => {
    const wrapper = wrapperRef.current
    if (!wrapper) return
    const handler = (e: WheelEvent) => {
      const providerId = (e.target as HTMLElement | null)?.getAttribute(PROVIDER_ID_ATTR)
      if (!providerId) return
      e.preventDefault()
      const { dx, dy } = wheelDeltaPixels(e)
      void commands.forwardPopupMouseWheel(providerId, e.offsetX, e.offsetY, dx, dy, domModifiers(e))
    }
    wrapper.addEventListener('wheel', handler, { passive: false })
    return () => wrapper.removeEventListener('wheel', handler)
  }, [])

  return (
    <div ref={wrapperRef} style={{ position: 'fixed', inset: 0, pointerEvents: 'none' }}>
      {Object.entries(popups).map(([providerId, rect]) => (
        <div
          key={providerId}
          {...{ [PROVIDER_ID_ATTR]: providerId }}
          tabIndex={-1}
          style={{
            // bindings.ts types PaneRectFraction's fields `number | null`
            // (specta's f64 mapping) even though pane_host.rs always
            // populates all four -- `?? 0` is a type-satisfying fallback,
            // not an expected runtime case.
            position: 'absolute',
            left: `${(rect.x ?? 0) * 100}%`,
            top: `${(rect.y ?? 0) * 100}%`,
            width: `${(rect.width ?? 0) * 100}%`,
            height: `${(rect.height ?? 0) * 100}%`,
            pointerEvents: 'auto',
            touchAction: 'none',
          }}
          onContextMenu={(e) => {
            // items.id=379: mirrors PaneHitLayer's own onContextMenu --
            // without this, the right-click still reaches CEF fine (via
            // onPointerDown/onPointerUp below) and its own context menu
            // pipeline runs correctly, but the browser's native
            // `contextmenu` DOM event for this same click also fires,
            // unprevented, on the *outer* Tauri webview (WebKitGTK, a
            // completely different browser than CEF) -- confirmed live,
            // same symptom as the parent-pane case this was first found on.
            e.preventDefault()
          }}
          onPointerDown={(e) => {
            const button = domButton(e.nativeEvent.button)
            if (button === null) return
            e.currentTarget.setPointerCapture(e.pointerId)
            e.currentTarget.focus()
            const detail = e.nativeEvent.detail || 1
            flushPendingMove(providerId)
            lastButtonDown.current[providerId] = { button, detail }
            void commands.forwardPopupMouseClick(
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
          onPointerUp={(e) => {
            const button = domButton(e.nativeEvent.button)
            if (button === null) return
            flushPendingMove(providerId)
            delete lastButtonDown.current[providerId]
            void commands.forwardPopupMouseClick(
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
            void commands.forwardPopupMouseClick(
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
