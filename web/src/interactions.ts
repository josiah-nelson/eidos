// Interaction capture: what a search presented, and what was done with it.
//
// This is data collection only. Nothing read here changes what the UI shows,
// and every failure is swallowed: a search must work identically whether the
// service records the events, refuses them, or is not there at all.
//
// Events are queued in memory, flushed on an interval, and flushed again when
// the page is hidden. The queue is bounded, so a long session on a slow or
// absent endpoint costs a fixed amount of memory and then starts dropping the
// oldest events rather than growing.

import type { ApiInt, InteractionEventBody } from './generated/api.ts'

/** Events the service accepts in one request. */
export const MAX_INTERACTION_BATCH = 500
/** Events held in memory; past this the oldest are dropped. */
export const MAX_QUEUED_INTERACTIONS = 1000
export const FLUSH_INTERVAL_MS = 2000

export type InteractionAction =
  | 'presented'
  | 'opened_preview'
  | 'opened_file'
  | 'copied_path'
  | 'exported'

/** One queued event, before the session id is attached. */
export interface Interaction {
  action: InteractionAction
  /** The query the results came from; hashed by the service, never stored. */
  q: string
  object_id?: ApiInt
  source_id?: ApiInt
  presented_rank?: number
}

export interface InteractionQueueOptions {
  send: (events: InteractionEventBody[]) => void
  sessionId: string
  /** Injected so tests can drive time; defaults to the timer functions. */
  schedule?: (fn: () => void, ms: number) => number
  cancel?: (handle: number) => void
  intervalMs?: number
  maxQueued?: number
  maxBatch?: number
}

/**
 * A bounded queue that batches interactions and flushes them on a timer.
 *
 * Deliberately free of anything DOM: the transport and the clock are injected,
 * so the batching, the bound, and the drop policy are testable on their own.
 */
export class InteractionQueue {
  private readonly options: Required<InteractionQueueOptions>
  private queued: InteractionEventBody[] = []
  private timer: number | null = null
  /** Events discarded because the queue was full. Never resets. */
  dropped = 0

  constructor(options: InteractionQueueOptions) {
    // Written out rather than spread over defaults: an explicitly `undefined`
    // option must fall back to the default, not overwrite it with nothing.
    this.options = {
      send: options.send,
      sessionId: options.sessionId,
      schedule: options.schedule ?? ((fn, ms) => setTimeout(fn, ms) as unknown as number),
      cancel: options.cancel ?? ((handle) => clearTimeout(handle)),
      intervalMs: options.intervalMs ?? FLUSH_INTERVAL_MS,
      maxQueued: options.maxQueued ?? MAX_QUEUED_INTERACTIONS,
      maxBatch: options.maxBatch ?? MAX_INTERACTION_BATCH,
    }
  }

  get size(): number {
    return this.queued.length
  }

  push(...events: Interaction[]): void {
    for (const event of events) {
      this.queued.push({ ...event, session_id: this.options.sessionId })
    }
    const overflow = this.queued.length - this.options.maxQueued
    if (overflow > 0) {
      // Keep the newest: what someone just did is more informative than what
      // an unreachable endpoint has already failed to take for minutes.
      this.queued.splice(0, overflow)
      this.dropped += overflow
    }
    this.start()
  }

  /** Send one batch now. Remaining events keep their place in line. */
  flush(): void {
    this.stop()
    if (this.queued.length === 0) return
    const batch = this.queued.splice(0, this.options.maxBatch)
    try {
      this.options.send(batch)
    } catch {
      // The transport is best effort; a batch it could not take is gone.
    }
    if (this.queued.length > 0) this.start()
  }

  /** Send everything queued, in batches. Used when the page is going away. */
  flushAll(): void {
    while (this.queued.length > 0) this.flush()
    this.stop()
  }

  private start(): void {
    if (this.timer === null && this.queued.length > 0) {
      this.timer = this.options.schedule(() => {
        this.timer = null
        this.flush()
      }, this.options.intervalMs)
    }
  }

  private stop(): void {
    if (this.timer !== null) {
      this.options.cancel(this.timer)
      this.timer = null
    }
  }
}

/**
 * A short mark for one page of results, used to tell a page that has already
 * been reported from one whose contents changed under it — a refetch of the
 * same query can return different hits, and those are a new presentation.
 *
 * FNV-1a over the ids, plus the count: cheap, and a collision only costs one
 * page of impressions in data nobody makes a decision on individually.
 */
export function pageFingerprint(ids: readonly string[]): string {
  let hash = 0x811c9dc5
  for (const id of ids) {
    for (let i = 0; i < id.length; i++) {
      hash = Math.imul(hash ^ id.charCodeAt(i), 0x01000193)
    }
    hash = Math.imul(hash ^ 0x2c, 0x01000193)
  }
  return `${ids.length}:${(hash >>> 0).toString(36)}`
}

/**
 * Which pages of a result set are a presentation that has not been recorded.
 *
 * A page is the unit the server rendered. The same page arriving again with
 * the same hits — a refetch on focus, a re-render — is not a new presentation.
 * The same page arriving with different hits is: the index changed under the
 * reader, and what they are looking at now was never reported. Changing the
 * query starts the log over, because that is a different result set entirely.
 */
export class PresentationLog {
  private key = ''
  private pages: string[] = []

  /** Indexes of `pages` (each a page's hit ids) that should be reported. */
  unreported(key: string, pages: readonly (readonly string[])[]): number[] {
    if (this.key !== key) {
      this.key = key
      this.pages = []
    }
    const marks = pages.map(pageFingerprint)
    const fresh = marks.flatMap((mark, index) => (this.pages[index] === mark ? [] : [index]))
    // Pages this result set no longer has must not vouch for pages fetched
    // later under the same query.
    this.pages = marks
    return fresh
  }
}

/** Random opaque id, short enough for the service's session id limit. */
export function newSessionId(): string {
  const uuid = globalThis.crypto?.randomUUID?.()
  if (uuid) return uuid
  return `s-${Math.random().toString(36).slice(2)}${Date.now().toString(36)}`
}

const SESSION_KEY = 'eidos.interaction-session'

/**
 * The id for this tab session: stable across reloads of one tab, different in
 * every other tab, and gone when the tab is closed. Storage can throw or be
 * unavailable (private windows, blocked site data), in which case the id lives
 * only as long as this page does.
 */
export function tabSessionId(storage: Pick<Storage, 'getItem' | 'setItem'> | undefined = safeSessionStorage()): string {
  try {
    const existing = storage?.getItem(SESSION_KEY)
    if (existing) return existing
    const created = newSessionId()
    storage?.setItem(SESSION_KEY, created)
    return created
  } catch {
    return newSessionId()
  }
}

function safeSessionStorage(): Storage | undefined {
  try {
    return globalThis.sessionStorage
  } catch {
    return undefined
  }
}

/**
 * Post a batch. `sendBeacon` is used first because it is the only transport
 * that survives the page going away; a rejected beacon (too large, or missing)
 * falls back to a keepalive fetch. Both are fire-and-forget.
 *
 * This does not go through `api.ts`: that client awaits a response and raises
 * `ApiError`, which is the right shape for everything a person is waiting on
 * and the wrong one for a request whose outcome nobody may ever see. The
 * request body is still the generated `InteractionBatch` shape.
 */
export function postInteractions(events: InteractionEventBody[]): void {
  const body = JSON.stringify({ events })
  try {
    const blob = new Blob([body], { type: 'application/json' })
    if (navigator.sendBeacon?.('/api/interactions', blob)) return
  } catch {
    // Fall through to fetch.
  }
  try {
    void fetch('/api/interactions', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body,
      keepalive: true,
    }).catch(() => {})
  } catch {
    // Nothing about a search depends on this request.
  }
}

let shared: InteractionQueue | null = null

/** The app-wide queue, created on first use. */
export function interactions(): InteractionQueue {
  shared ??= new InteractionQueue({ send: postInteractions, sessionId: tabSessionId() })
  return shared
}

/** Record one or more interactions. Never throws. */
export function track(...events: Interaction[]): void {
  try {
    interactions().push(...events)
  } catch {
    // Capture is never allowed to break the surface it is watching.
  }
}

/**
 * Flush the queue when the page is hidden — a tab close, a navigation away, or
 * a phone locking — because the interval flush will not get another turn.
 * Returns a function that removes the listener.
 */
export function installInteractionFlush(): () => void {
  const flush = () => {
    if (document.visibilityState === 'hidden') interactions().flushAll()
  }
  const leaving = () => interactions().flushAll()
  document.addEventListener('visibilitychange', flush)
  window.addEventListener('pagehide', leaving)
  return () => {
    document.removeEventListener('visibilitychange', flush)
    window.removeEventListener('pagehide', leaving)
  }
}
