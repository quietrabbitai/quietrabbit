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
// items.id=359 (rail+content-pane redesign, 2026-08-30): replaces the
// former items.id=334 full-width-row-stacking model entirely. Exactly one
// pane is ever composited at a time -- the one whose provider id is
// Tier3AccessPane's own `activeProviderId` -- rendered at (near-)full
// size inside the content pane, rather than every open pane getting its
// own fixed-height row in a scrolling dock. This is simpler than the row-
// stacking code it replaces, not more complex: there is no longer a
// scroll-snap/partial-visibility problem to solve (isRowFullyVisible and
// its "hide a row entirely rather than resize its CEF viewport mid-
// scroll" reasoning no longer apply -- there is no scrolling dock left to
// partially reveal a row). Rust's own `pane_pixel_rect` (pane_host.rs)
// still re-derives physical-pixel rects from whatever fraction this
// module computes, same division of labor as before.
//
// Same placeholder discipline as cloudChatAccessConfig.ts/middleZoneConfig.ts:
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

/** The one active pane's rect is simply the content-pane element's own
 *  bounding box -- no row stacking, no scroll-snap, nothing else to
 *  compute. Returns an empty object when nothing is active (content pane
 *  shows its empty-state prompt instead) or the element hasn't laid out
 *  yet (zero width). `activeProviderId` is `null` when QR is expanded
 *  (nothing shown) or no provider has been activated yet. */
export function computeActivePaneRect(
  contentPaneRect: DOMRectReadOnly,
  activeProviderId: string | null,
): Record<string, PanePixelRect> {
  if (activeProviderId === null || contentPaneRect.width <= 0 || contentPaneRect.height <= 0) {
    return {}
  }
  return {
    [activeProviderId]: {
      left: contentPaneRect.left,
      top: contentPaneRect.top,
      width: contentPaneRect.width,
      height: contentPaneRect.height,
    },
  }
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
