import assert from 'node:assert/strict'
import { test } from 'node:test'
import {
  InteractionQueue,
  MAX_INTERACTION_BATCH,
  PresentationLog,
  newSessionId,
  pageFingerprint,
  tabSessionId,
  type Interaction,
} from './interactions.ts'

/** A queue whose clock is a list of pending callbacks. */
function harness(options: { maxQueued?: number; maxBatch?: number; failing?: boolean } = {}) {
  const sent: string[][] = []
  let pending: (() => void) | null = null
  const queue = new InteractionQueue({
    sessionId: 'tab-1',
    send: (events) => {
      if (options.failing) throw new Error('offline')
      sent.push(events.map((e) => `${e.action}:${e.presented_rank ?? ''}`))
    },
    schedule: (fn) => {
      pending = fn
      return 1
    },
    cancel: () => {
      pending = null
    },
    maxQueued: options.maxQueued,
    maxBatch: options.maxBatch,
  })
  // A one-shot timer: firing it consumes it, as `setTimeout` does.
  const tick = () => {
    const fire = pending
    pending = null
    fire?.()
  }
  return { queue, sent, tick, pending: () => pending !== null }
}

function presented(rank: number): Interaction {
  return { action: 'presented', q: 'ext:md readme', presented_rank: rank, object_id: String(rank) }
}

test('events are batched into one send per interval', () => {
  const h = harness()
  h.queue.push(presented(0), presented(1))
  h.queue.push({ action: 'opened_preview', q: 'ext:md readme', object_id: '7' })
  assert.deepEqual(h.sent, [], 'nothing is sent before the interval elapses')
  assert.equal(h.queue.size, 3)

  h.tick()
  assert.deepEqual(h.sent, [['presented:0', 'presented:1', 'opened_preview:']])
  assert.equal(h.queue.size, 0)
  assert.equal(h.pending(), false, 'an empty queue holds no timer')
})

test('every event carries the session id', () => {
  const seen: string[] = []
  const queue = new InteractionQueue({
    sessionId: 'tab-9',
    send: (events) => seen.push(...events.map((e) => e.session_id)),
    schedule: (fn) => {
      fn()
      return 1
    },
    cancel: () => {},
  })
  queue.push(presented(0), presented(1))
  queue.flush()
  assert.deepEqual(seen, ['tab-9', 'tab-9'])
})

test('the queue is bounded and drops the oldest events', () => {
  const h = harness({ maxQueued: 4 })
  for (let rank = 0; rank < 10; rank++) h.queue.push(presented(rank))
  assert.equal(h.queue.size, 4)
  assert.equal(h.queue.dropped, 6)
  h.tick()
  assert.deepEqual(h.sent, [['presented:6', 'presented:7', 'presented:8', 'presented:9']])
})

test('a batch never exceeds what the service accepts', () => {
  const h = harness({ maxQueued: 1200, maxBatch: MAX_INTERACTION_BATCH })
  for (let rank = 0; rank < 1100; rank++) h.queue.push(presented(rank))
  assert.equal(h.queue.dropped, 0)

  h.tick()
  assert.equal(h.sent.length, 1)
  assert.equal(h.sent[0].length, MAX_INTERACTION_BATCH)
  assert.equal(h.queue.size, 600, 'the rest keeps its place in line')
  assert.equal(h.pending(), true, 'a partial flush schedules the next one')

  // A page going away sends everything that is left, in accepted batches.
  h.queue.flushAll()
  assert.deepEqual(
    h.sent.map((batch) => batch.length),
    [MAX_INTERACTION_BATCH, MAX_INTERACTION_BATCH, 100],
  )
  assert.equal(h.queue.size, 0)
  assert.equal(h.pending(), false)
})

test('a transport that throws loses the batch and nothing else', () => {
  const h = harness({ failing: true })
  h.queue.push(presented(0))
  assert.doesNotThrow(() => h.tick())
  assert.equal(h.queue.size, 0)
  h.queue.push(presented(1))
  assert.doesNotThrow(() => h.queue.flushAll())
})

test('a page of results is reported once, and again when its hits change', () => {
  const log = new PresentationLog()
  const key = 'ext:md|files|relevance|true|50'

  assert.deepEqual(log.unreported(key, [['1', '2', '3']]), [0])
  // A refetch that returns the same hits is the same presentation.
  assert.deepEqual(log.unreported(key, [['1', '2', '3']]), [])
  // Scrolling on adds a page; the first one is not reported again.
  assert.deepEqual(log.unreported(key, [['1', '2', '3'], ['4', '5']]), [1])
  assert.deepEqual(log.unreported(key, [['1', '2', '3'], ['4', '5']]), [])

  // A stale refetch returns different hits under the same query: what the
  // reader is looking at now was never reported.
  assert.deepEqual(log.unreported(key, [['1', '9', '3'], ['4', '5']]), [0])
  // Order matters too — a re-sorted page is a different presentation.
  assert.deepEqual(log.unreported(key, [['3', '9', '1'], ['4', '5']]), [0])

  // A different query starts over, even when it lands on the same hits.
  assert.deepEqual(log.unreported('ext:txt|files|relevance|true|50', [['3', '9', '1']]), [0])
  // Going back reports again: the first page is a fresh presentation.
  assert.deepEqual(log.unreported(key, [['3', '9', '1'], ['4', '5']]), [0, 1])

  // Shrinking the result set must not let a dropped page vouch for a later one.
  assert.deepEqual(log.unreported(key, [['3', '9', '1']]), [])
  assert.deepEqual(log.unreported(key, [['3', '9', '1'], ['7']]), [1])

  assert.equal(pageFingerprint(['1', '2']), pageFingerprint(['1', '2']))
  assert.notEqual(pageFingerprint(['1', '2']), pageFingerprint(['12']))
  assert.notEqual(pageFingerprint([]), pageFingerprint(['1']))
})

test('a tab keeps one session id, and separate tabs differ', () => {
  const tab = () => {
    const store = new Map<string, string>()
    return {
      getItem: (k: string) => store.get(k) ?? null,
      setItem: (k: string, v: string) => void store.set(k, v),
    }
  }
  const storage = tab()
  const first = tabSessionId(storage)
  assert.equal(tabSessionId(storage), first, 'one tab keeps its id across reloads')
  assert.notEqual(tabSessionId(tab()), first, 'another tab gets its own')
  assert.ok(first.length > 0 && first.length <= 64, 'ids fit the service limit')

  // Storage that refuses still yields a usable id.
  const refusing = {
    getItem: () => {
      throw new Error('blocked')
    },
    setItem: () => {},
  }
  assert.ok(tabSessionId(refusing).length > 0)
  assert.notEqual(newSessionId(), newSessionId())
})
