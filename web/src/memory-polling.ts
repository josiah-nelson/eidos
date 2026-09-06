// Activity's poll cadence for `GET /api/memory`. The service answers a cold or
// aged-out request by waiting briefly for its own refresh, so a pending or
// stale sample is short-lived and worth asking about again sooner.

import type { MemoryView } from './generated/api.js'

export const PENDING_POLL_MS = 1000
export const SETTLED_POLL_MS = 5000

export function memoryPollInterval(memory: MemoryView | undefined) {
  return !memory?.process || memory.stale ? PENDING_POLL_MS : SETTLED_POLL_MS
}
