import assert from 'node:assert/strict'
import { test } from 'node:test'
import { applyClause, negate, tokenize } from './query-clause.ts'

// A size facet as the server sends it: disjoint buckets, each with the exact
// clause that selects it and the exact clause that excludes it.
const small = { clause: 'size:<4k', exclude: '-size:<4k' }
const medium = { clause: 'size:>=4k size:<64k', exclude: '-(size:>=4k size:<64k)' }
const buckets = [small, medium]
/** What clicking `pick` supersedes: every other bucket, and its own opposite. */
const replaces = (pick: { clause: string; exclude: string }, form: 'clause' | 'exclude') => [
  ...buckets.filter((b) => b !== pick).flatMap((b) => [b.clause, b.exclude]),
  form === 'clause' ? pick.exclude : pick.clause,
]

test('groups and quotes stay one token', () => {
  assert.deepEqual(tokenize('a "two words" -(size:>=4k size:<64k) ext:cs'), [
    'a',
    '"two words"',
    '-(size:>=4k size:<64k)',
    'ext:cs',
  ])
  assert.deepEqual(tokenize('   '), [])
})

test('a bucket clause is appended to the query', () => {
  assert.equal(applyClause('', medium.clause, replaces(medium, 'clause')), 'size:>=4k size:<64k')
  assert.equal(
    applyClause('readme ext:md', medium.clause, replaces(medium, 'clause')),
    'readme ext:md size:>=4k size:<64k',
  )
})

test('clicking the same bucket twice changes nothing', () => {
  const once = applyClause('readme', small.clause, replaces(small, 'clause'))
  assert.equal(applyClause(once, small.clause, replaces(small, 'clause')), once)
})

test('include and exclude replace each other instead of contradicting', () => {
  const included = applyClause('readme', medium.clause, replaces(medium, 'clause'))
  const excluded = applyClause(included, medium.exclude, replaces(medium, 'exclude'))
  assert.equal(excluded, 'readme -(size:>=4k size:<64k)')
  assert.equal(
    applyClause(excluded, medium.clause, replaces(medium, 'clause')),
    'readme size:>=4k size:<64k',
  )
})

test('another bucket of the same facet is replaced, not intersected', () => {
  const first = applyClause('readme', small.clause, replaces(small, 'clause'))
  assert.equal(
    applyClause(first, medium.clause, replaces(medium, 'clause')),
    'readme size:>=4k size:<64k',
  )
  // Excluding one bucket while another is excluded keeps both: exclusions
  // are not mutually exclusive.
  const excluded = applyClause('readme', small.exclude, [small.clause])
  assert.equal(
    applyClause(excluded, medium.exclude, [medium.clause]),
    'readme -size:<4k -(size:>=4k size:<64k)',
  )
})

test('bounds typed by hand are never rewritten', () => {
  assert.equal(
    applyClause('size:>=100 ext:md', medium.clause, replaces(medium, 'clause')),
    'size:>=100 ext:md size:>=4k size:<64k',
  )
})

test('term facet clauses negate without grouping', () => {
  assert.equal(negate('ext:cs'), '-ext:cs')
  assert.equal(negate('size:>=4k size:<64k'), '-(size:>=4k size:<64k)')
  assert.equal(applyClause('readme -ext:cs', 'ext:cs', ['-ext:cs']), 'readme ext:cs')
})
