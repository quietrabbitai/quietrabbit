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
// items.id=334 -- panes stack as full-width rows, top-to-bottom, not
// side-by-side columns: with 2-3 panes open, columns squeezed each to an
// unusable ~170px width. Row height is fixed (PANE_ROW_HEIGHT) rather than
// derived from dockRect.height, so the dock can grow taller than its
// column's own available space and the column's overflow:auto scrolls to
// it (Tier3AccessPane.tsx drives the dock element's actual height from
// openPaneIds.length * PANE_ROW_HEIGHT). This knowingly trades away the
// narrow-per-pane-width CEF WasResized()/GetViewRect() exercise the prior
// column layout gave items.id=202 piece 6 -- deliberate, not an oversight.
//
// PaneHitLayer's wheel handler unconditionally claims and forwards to CEF
// any scroll landing on a pane's hit-div (see its own doc), which was
// harmless under column layout (every pane always spanned the full,
// always-on-screen dockRect.height, so the dock never needed to scroll)
// but leaves no surface for the user to wheel-scroll the growable dock now
// that panes fill nearly the whole visible area. Resolved on the CSS side
// instead of here: NavShell.css forces .tier3-access-pane__dock-column's
// scrollbar to render as a classic, always-visible, gutter-reserving one
// rather than GTK's auto-hiding overlay style -- a reserved gutter is
// naturally excluded from dockRect.width below, so no pane's hit-div ever
// covers it, and dragging the thumb (a plain pointer sequence, not a wheel
// event) reaches the column's native scroll without touching pane click/
// wheel routing at all. An earlier attempt reserved a per-row DOM header
// strip for this instead (PANE_HEADER_HEIGHT) -- reverted, disliked
// visually (cramped labels between panes, text bleeding into pane content).
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

/** Fixed row height (CSS px) for each open Tier 3 pane -- see items.id=334.
 *  Sized so two open panes fit a typical window without forcing a scroll;
 *  a third pane or more relies on the dock column's own overflow:auto. */
export const PANE_ROW_HEIGHT = 280

/** Splits `dockRect` into `paneIds.length` full-width rows, stacked
 *  top-to-bottom at `PANE_ROW_HEIGHT` each (items.id=334). Returns an empty
 *  object for zero panes or a collapsed dock (nothing to sync). */
export function computePaneRects(
  dockRect: DOMRectReadOnly,
  paneIds: string[],
): Record<string, PanePixelRect> {
  const count = paneIds.length
  if (count === 0 || dockRect.width <= 0) {
    return {}
  }

  const rects: Record<string, PanePixelRect> = {}
  paneIds.forEach((id, index) => {
    rects[id] = {
      left: dockRect.left,
      top: dockRect.top + index * PANE_ROW_HEIGHT,
      width: dockRect.width,
      height: PANE_ROW_HEIGHT,
    }
  })
  return rects
}

/** Whether `rect`'s full extent fits within `viewport` (both
 *  viewport-relative CSS pixels). items.id=334: an earlier version of this
 *  helper (clipToViewport) sent Rust a *shrunk* height for a
 *  partially-scrolled pane, which fed straight into CEF's WasResized() --
 *  a browser engine's real re-render lags a continuously-changing height,
 *  so mid-scroll the compositor was blitting the old, full-size texture
 *  into a smaller rect, stretching/squashing the page visually. A pane's
 *  actual CEF viewport size must stay stable (always the full
 *  PANE_ROW_HEIGHT): a partially-scrolled row is hidden entirely rather
 *  than resized, reappearing at full size once fully back in view.
 *  NavShell.css's scroll-snap rules keep the column from resting anywhere
 *  that would leave a row only partially visible. */
export function isRowFullyVisible(
  rect: PanePixelRect,
  viewport: { top: number; bottom: number },
): boolean {
  return rect.top >= viewport.top && rect.top + rect.height <= viewport.bottom
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
