import assert from 'node:assert/strict'
import test from 'node:test'
import { ago, bytes, count, integerNumber } from './format.ts'

test('decimal API integers format without passing through Number', () => {
  assert.equal(count('9007199254740993'), '9,007,199,254,740,993')
  assert.equal(count('18446744073709551615'), '18,446,744,073,709,551,615')
  assert.equal(bytes('1024'), '1.0 KiB')
})

test('lossy conversion is explicit and timestamps retain millisecond order', () => {
  assert.equal(integerNumber('9007199254740993'), 9_007_199_254_740_992)
  const originalNow = Date.now
  Date.now = () => 1_700_000_001_000
  try {
    assert.equal(ago('1700000000123456789'), '0s ago')
  } finally {
    Date.now = originalNow
  }
})
