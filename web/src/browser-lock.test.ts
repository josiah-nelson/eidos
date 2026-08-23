/// <reference lib="dom" />

import assert from 'node:assert/strict'
import test from 'node:test'
import { createBrowserLock } from './browser-lock.ts'

test('browser locks are preferred when available', async () => {
  const calls: string[] = []
  const webLocks = {
    request: async <T>(name: string, action: () => T) => {
      calls.push(name)
      return action()
    },
  } as LockManager
  const lock = createBrowserLock(webLocks, undefined)

  assert.equal(await lock.run('saved-searches', () => 42), 42)
  assert.deepEqual(calls, ['saved-searches'])
})

test('the last-resort fallback keeps same-tab actions ordered', async () => {
  const lock = createBrowserLock(undefined, undefined)
  const calls: string[] = []

  const first = lock.run('saved-searches', () => calls.push('first'))
  const second = lock.run('saved-searches', () => calls.push('second'))

  await Promise.all([first, second])
  assert.deepEqual(calls, ['first', 'second'])
})
