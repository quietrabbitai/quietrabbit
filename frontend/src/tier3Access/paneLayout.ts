// items.id=202 piece 4 -- the real split-screen container's layout math.
// items.id=257 Path B -- also the geometry source for the invisible
// per-pane pointer hit-layer (PaneHitLayer.tsx).
//
// CEF panes composite into one shared GTK GLArea (single-window compositing,
// items.id=202 real positioning fix, 2026-08-07) -- not separate OS windows,
// not DOM children in their own right. This computes, per open pane, a
// target rect as a fraction (0..1) of the main window's own content area
// (not absolute screen pixels): plain DOM geometry (getBoundingClientRect()
// + window.innerWidth/innerHeight) is enough for this, with no need for
// devicePixelRatio or a Tauri window-position API call, since a fraction of
// CSS pixels equals the same fraction of physical pixels. Rust's own
// pane_pixel_rect (pane_host.rs) re-derives physical-pixel rects from this
// same fraction against GTK's live GLArea size for rendering; PaneHitLayer
// positions each pane's invisible hit-div from the CSS-pixel-space rect this
// module computes *before* dividing into that fraction -- one computation,
// two consumers, so the DOM hit-layer's on-screen position and the fraction
// Rust renders against can never drift apart (items.id=257's own
// requirement).
//
// Same placeholder discipline as tier3AccessConfig.ts/middleZoneConfig.ts:
// structural only, no QR branding/visual grammar applied here.

export interface PaneRectFraction {
  x: number
  y: number
  width: number
  height: number
}

/** One pane's on-screen rect in CSS pixels, viewport-relative --
 *  `Tier3AccessPane`'s `syncPaneLayout` computes this once per pane and
 *  derives both consumers from it: `pixelRectToFraction` for the
 *  `PaneRectFraction` sent to Rust, and this same rect passed straight to
 *  `PaneHitLayer` for its invisible per-pane hit-divs' CSS position -- one
 *  computation, two consumers, so they can never drift apart (see this
 *  file's module doc). */
export interface PanePixelRect {
  left: number
  top: number
  width: number
  height: number
}

/** Splits `dockRect` into `paneIds.length` equal-width side-by-side
 *  columns, full dock height. Columns (not rows) deliberately: narrow
 *  columns are exactly the narrow-desktop-viewport case items.id=202
 *  piece 6 (CEF's WasResized()/GetViewRect()) needs exercised against real,
 *  distinct per-pane sizes -- a fixed single-pane assumption never produces
 *  that. Returns an empty object for zero panes (nothing to sync). */
export function computePaneRects(
  dockRect: DOMRectReadOnly,
  paneIds: string[],
): Record<string, PanePixelRect> {
  const count = paneIds.length
  if (count === 0 || dockRect.width <= 0 || dockRect.height <= 0) {
    return {}
  }

  const columnWidth = dockRect.width / count
  const rects: Record<string, PanePixelRect> = {}
  paneIds.forEach((id, index) => {
    rects[id] = {
      left: dockRect.left + index * columnWidth,
      top: dockRect.top,
      width: columnWidth,
      height: dockRect.height,
    }
  })
  return rects
}

/** Divides a `PanePixelRect` down into the 0..1-of-viewport fraction Rust's
 *  `PaneRectFraction` expects (see `set_pane_layout`). */
export function pixelRectToFraction(
  rect: PanePixelRect,
  viewportWidth: number,
  viewportHeight: number,
): PaneRectFraction {
  return {
    x: rect.left / viewportWidth,
    y: rect.top / viewportHeight,
    width: rect.width / viewportWidth,
    height: rect.height / viewportHeight,
  }
}
