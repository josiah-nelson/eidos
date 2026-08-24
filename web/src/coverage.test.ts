import assert from 'node:assert/strict'
import test from 'node:test'
import type { CoverageEnvelope } from './generated/api.js'
import { mergeCoverage } from './coverage.ts'

const offline = {
  kind: 'offline' as const,
  severity: 'warning' as const,
  detail: 'source disconnected',
}

function page(coverage: CoverageEnvelope) {
  return { coverage }
}

test('keeps a degraded source that is absent from the newest page', () => {
  const merged = mergeCoverage([
    page({
      full: false,
      sources: [{ source_id: '1', name: 'archive', watermark: '10', degraded: [offline] }],
    }),
    page({ full: true, sources: [{ source_id: '2', name: 'local', watermark: '20' }] }),
  ])

  assert.equal(merged?.full, false)
  assert.deepEqual(merged?.sources, [
    { source_id: '1', name: 'archive', watermark: '10', degraded: [offline] },
    { source_id: '2', name: 'local', watermark: '20', degraded: [] },
  ])
})

test('uses the newest source watermark and unions distinct reasons', () => {
  const stale = { kind: 'stale' as const, severity: 'info' as const, detail: 'old checkpoint' }
  const merged = mergeCoverage([
    page({
      full: false,
      degraded: [offline],
      sources: [{ source_id: '1', name: 'archive', watermark: '10', degraded: [offline] }],
    }),
    page({
      full: false,
      degraded: [offline, stale],
      sources: [{ source_id: '1', name: 'archive', watermark: '20', degraded: [stale] }],
    }),
  ])

  assert.deepEqual(merged?.degraded, [offline, stale])
  assert.deepEqual(merged?.sources[0], {
    source_id: '1',
    name: 'archive',
    watermark: '20',
    degraded: [offline, stale],
  })
})
