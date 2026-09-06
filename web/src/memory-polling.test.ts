import assert from 'node:assert/strict'
import test from 'node:test'
import type { MemoryView } from './generated/api.js'
import { PENDING_POLL_MS, SETTLED_POLL_MS, memoryPollInterval } from './memory-polling.ts'

const settled: MemoryView = {
  process: {
    pid: 42,
    resident_bytes: '104857600',
    peak_resident_bytes: null,
    private_commit_bytes: null,
  },
  sample_age_s: '1',
  stale: false,
  error: null,
  catalog: {
    baseline_connections: 13,
    page_cache_per_connection_bytes: '67108864',
    page_cache_baseline_target_bytes: '872415232',
    mmap_per_connection_limit_bytes: '1099511627776',
  },
  catalog_writer_budget_bytes: '100663296',
  content_writer_budget_bytes: '268435456',
  content_input_budget_bytes: '16777216',
}

test('a settled sample polls at the sampler refresh interval', () => {
  assert.equal(memoryPollInterval(settled), SETTLED_POLL_MS)
})

test('pending, absent and stale samples are asked about again sooner', () => {
  assert.equal(memoryPollInterval(undefined), PENDING_POLL_MS)
  assert.equal(memoryPollInterval({ ...settled, process: null }), PENDING_POLL_MS)
  assert.equal(memoryPollInterval({ ...settled, stale: true }), PENDING_POLL_MS)
})
