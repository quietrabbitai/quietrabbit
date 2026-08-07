// items.id=202 piece 4 -- the real split-screen container's layout math.
//
// CEF panes are separate OS-level windows synced beside the Tauri window
// (sync_window.rs), not DOM children -- there is nothing to render them
// into directly. Instead this computes, per open pane, a target rect as a
// fraction (0..1) of the main window's own content area (not absolute
// screen pixels): plain DOM geometry (getBoundingClientRect() +
// window.innerWidth/innerHeight) is enough for this, with no need for
// devicePixelRatio or a Tauri window-position API call, since a fraction of
// CSS pixels equals the same fraction of physical pixels. The Rust side
// (main.rs's app.run() closure) already has a fresh, authoritative
// content-area rect on every native window Moved/Resized event and
// multiplies it against whatever fraction this module last reported -- so a
// window move alone stays correctly synced without this module re-running.
//
// Same placeholder discipline as tier3AccessConfig.ts/middleZoneConfig.ts:
// structural only, no QR branding/visual grammar applied here.

export interface PaneRectFraction {
  x: number
  y: number
  width: number
  height: number
}

/** Splits `dockRect` into `paneIds.length` equal-width side-by-side
 *  columns, full dock height. Columns (not rows) deliberately: narrow
 *  columns are exactly the narrow-desktop-viewport case items.id=202
 *  piece 6 (CEF's WasResized()/GetViewRect()) needs exercised against real,
 *  distinct per-pane sizes -- a fixed single-pane assumption never produces
 *  that. Returns an empty object for zero panes (nothing to sync). */
export function computePaneLayout(
  dockRect: DOMRectReadOnly,
  viewportWidth: number,
  viewportHeight: number,
  paneIds: string[],
): Record<string, PaneRectFraction> {
  const count = paneIds.length
  if (count === 0 || viewportWidth <= 0 || viewportHeight <= 0) {
    return {}
  }

  const columnWidth = dockRect.width / count
  const layout: Record<string, PaneRectFraction> = {}
  paneIds.forEach((id, index) => {
    layout[id] = {
      x: (dockRect.left + index * columnWidth) / viewportWidth,
      y: dockRect.top / viewportHeight,
      width: columnWidth / viewportWidth,
      height: dockRect.height / viewportHeight,
    }
  })
  return layout
}
