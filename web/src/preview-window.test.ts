import assert from 'node:assert/strict'
import { test } from 'node:test'
import { stepWindow, type PreviewWindow, type Shown } from './preview-window.ts'

const MAX = 4
const shown = (first: number, last: number): Shown => ({ first, last, max: MAX })

test('widens the window while the server returns what was asked for', () => {
  const w = { ordinal: 6, before: 1, after: 1 }
  assert.deepEqual(stepWindow(w, shown(5, 7), 'up'), { ordinal: 6, before: 2, after: 1 })
  assert.deepEqual(stepWindow(w, shown(5, 7), 'down'), { ordinal: 6, before: 1, after: 2 })
})

test('steps the window once the neighbour limit is reached', () => {
  const w = { ordinal: 6, before: MAX, after: MAX }
  assert.deepEqual(stepWindow(w, shown(2, 10), 'up'), { ordinal: 1, before: MAX, after: 0 })
  assert.deepEqual(stepWindow(w, shown(2, 10), 'down'), { ordinal: 11, before: 0, after: MAX })
})

test('steps rather than widening when the byte budget dropped neighbours', () => {
  // Asked for 6±2 but the budget only allowed 5..7 back.
  const w = { ordinal: 6, before: 2, after: 2 }
  assert.deepEqual(stepWindow(w, shown(5, 7), 'up'), { ordinal: 4, before: MAX, after: 0 })
  assert.deepEqual(stepWindow(w, shown(5, 7), 'down'), { ordinal: 8, before: 0, after: MAX })
})

test('a chunk too big to be a neighbour is still reached', () => {
  // Chunk 8 alone exceeds the response budget, so the server never adds it as
  // a neighbour: it can only appear as the requested chunk. Paging down from
  // 6 must still get there rather than looping on a window that ends at 7.
  const serve = (w: PreviewWindow): Shown => ({
    first: Math.max(0, w.ordinal - w.before),
    last: w.ordinal >= 8 ? w.ordinal : Math.min(w.ordinal + w.after, 7),
    max: MAX,
  })
  let w: PreviewWindow = { ordinal: 6, before: 0, after: 0 }
  const requested: number[] = []
  for (let i = 0; i < 8; i++) {
    w = stepWindow(w, serve(w), 'down')
    requested.push(w.ordinal)
  }
  assert.ok(requested.includes(8), `never reached the oversized chunk: ${requested.join(', ')}`)
})

test('never pages above the first chunk', () => {
  const w = { ordinal: 0, before: MAX, after: MAX }
  assert.deepEqual(stepWindow(w, shown(0, 4), 'up'), { ordinal: 0, before: MAX, after: 0 })
})

test('carries the pinned generation through', () => {
  const w = { ordinal: 3, before: MAX, after: MAX, generation: 12 }
  assert.equal(stepWindow(w, shown(0, 7), 'down').generation, 12)
})
