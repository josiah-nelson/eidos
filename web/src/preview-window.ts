/** The slice of stored chunks to ask `GET /api/objects/{id}/content` for. */
export interface PreviewWindow {
  ordinal: number
  before: number
  after: number
  /** Omit to read whatever generation the catalog currently holds. */
  generation?: number
}

/** The ordinals a preview response actually contained, and its own bound. */
export interface Shown {
  first: number
  last: number
  /** `limits.max_neighbors`: neighbours the server will ever add per side. */
  max: number
}

/**
 * The window to request when the reader asks for more text above or below
 * what is on screen.
 *
 * Widening `before`/`after` only helps while the server still returns
 * everything the window asked for. Once the response byte or line budget is
 * the binding limit the server drops whole neighbours, so a wider request
 * comes back identical. The window has to *step* instead, making the next
 * chunk out the requested one — the one the server always returns, cut down
 * if it has to be. Without that step, a single chunk larger than the response
 * budget would sit permanently just outside a window that never advances.
 */
export function stepWindow(w: PreviewWindow, shown: Shown, direction: 'up' | 'down'): PreviewWindow {
  if (direction === 'up') {
    const reached = shown.first <= Math.max(0, w.ordinal - w.before)
    return reached && w.before < shown.max
      ? { ...w, before: w.before + 1 }
      : { ...w, ordinal: Math.max(0, shown.first - 1), before: shown.max, after: 0 }
  }
  const reached = shown.last >= w.ordinal + w.after
  return reached && w.after < shown.max
    ? { ...w, after: w.after + 1 }
    : { ...w, ordinal: shown.last + 1, before: 0, after: shown.max }
}
