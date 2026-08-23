import assert from 'node:assert/strict'
import { test } from 'node:test'
import { applyClause, applyFacetClick, negate, tokenize } from './query-clause.ts'

// A size facet as the server sends it: disjoint buckets, each with the exact
// clause that selects it and the exact clause that excludes it.
const small = { clause: 'size:<4k', exclude: '-size:<4k' }
const medium = { clause: 'size:>=4k size:<64k', exclude: '-(size:>=4k size:<64k)' }
const large = { clause: 'size:>=64k', exclude: '-size:>=64k' }
const buckets = [small, medium, large]
const others = (pick: (typeof buckets)[number]) => buckets.filter((b) => b !== pick)

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
  assert.equal(applyFacetClick('', medium, 'include', others(medium)), 'size:>=4k size:<64k')
  assert.equal(
    applyFacetClick('readme ext:md', medium, 'include', others(medium)),
    'readme ext:md size:>=4k size:<64k',
  )
})

test('clicking the same bucket twice changes nothing', () => {
  const once = applyFacetClick('readme', small, 'include', others(small))
  assert.equal(applyFacetClick(once, small, 'include', others(small)), once)
})

test('include and exclude replace each other instead of contradicting', () => {
  const included = applyFacetClick('readme', medium, 'include', others(medium))
  const excluded = applyFacetClick(included, medium, 'exclude', others(medium))
  assert.equal(excluded, 'readme -(size:>=4k size:<64k)')
  assert.equal(
    applyFacetClick(excluded, medium, 'include', others(medium)),
    'readme size:>=4k size:<64k',
  )
})

test('another bucket of the same facet is replaced, not intersected', () => {
  const first = applyFacetClick('readme', small, 'include', others(small))
  assert.equal(
    applyFacetClick(first, medium, 'include', others(medium)),
    'readme size:>=4k size:<64k',
  )
})

test('excluding several buckets keeps every exclusion', () => {
  const one = applyFacetClick('readme', small, 'exclude', others(small))
  const two = applyFacetClick(one, medium, 'exclude', others(medium))
  assert.equal(two, 'readme -size:<4k -(size:>=4k size:<64k)')
  // Selecting a bucket then subsumes those exclusions, which it implies.
  assert.equal(applyFacetClick(two, large, 'include', others(large)), 'readme size:>=64k')
})

test('bounds typed by hand are never rewritten', () => {
  assert.equal(
    applyFacetClick('size:>=100 ext:md', medium, 'include', others(medium)),
    'size:>=100 ext:md size:>=4k size:<64k',
  )
})

test('a clause added to a disjunction filters every branch', () => {
  // AND binds tighter than OR, so `a OR b ext:cs` would filter only `b`.
  assert.equal(applyClause('a OR b', 'ext:cs'), '(a OR b) ext:cs')
  assert.equal(applyFacetClick('a OR b', small, 'include', others(small)), '(a OR b) size:<4k')
  // Grouping happens once, not on every click.
  assert.equal(
    applyFacetClick('(a OR b) size:<4k', medium, 'include', others(medium)),
    '(a OR b) size:>=4k size:<64k',
  )
  assert.equal(applyClause('(a OR b) ext:cs', 'ext:md'), '(a OR b) ext:cs ext:md')
})

test('term facet clauses negate without grouping', () => {
  assert.equal(negate('ext:cs'), '-ext:cs')
  assert.equal(negate('size:>=4k size:<64k'), '-(size:>=4k size:<64k)')
  // Term facets pass no siblings: two of them are a legitimate intersection.
  const cs = { clause: 'ext:cs', exclude: '-ext:cs' }
  assert.equal(applyFacetClick('readme -ext:cs', cs, 'include'), 'readme ext:cs')
  assert.equal(applyFacetClick('readme ext:md', cs, 'exclude'), 'readme ext:md -ext:cs')
})
