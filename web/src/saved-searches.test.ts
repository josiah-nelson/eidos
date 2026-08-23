import assert from 'node:assert/strict'
import { test } from 'node:test'
import {
  DEFAULT_SEARCH_VIEW,
  MAX_SAVED_SEARCH_BYTES,
  MAX_SAVED_SEARCHES,
  canonicalSearchParams,
  canonicalSearchUrl,
  deleteSavedSearch,
  duplicateSavedSearch,
  importSavedSearches,
  loadSavedSearches,
  readSearchView,
  renameSavedSearch,
  serializeSavedSearches,
  updateSavedSearches,
  upsertSavedSearch,
  type SavedSearch,
  type SavedSearchLock,
  type SearchViewState,
} from './saved-searches.ts'

const state: SearchViewState = {
  q: 'content:"build 4.2" ext:md',
  mode: 'both',
  sort: 'modified',
  desc: false,
  pageSize: 250,
  details: true,
}

function ids(...values: string[]) {
  let at = 0
  return () => values[at++] ?? `id-${at}`
}

function one(name = 'Build notes'): SavedSearch[] {
  return upsertSavedSearch([], name, state, { now: 10, id: ids('a') }).searches
}

test('canonical URL round trip preserves durable view state and drops transient parameters', () => {
  const dirty = new URLSearchParams({
    q: state.q,
    mode: state.mode,
    sort: state.sort,
    desc: '0',
    page: '250',
    details: '1',
    cursor: 'generation-bound-secret',
    explain_once: '1',
  })
  const decoded = readSearchView(dirty)
  assert.deepEqual(decoded, state)
  const canonical = canonicalSearchParams(decoded)
  assert.equal(canonical.get('cursor'), null)
  assert.equal(canonical.get('explain_once'), null)
  assert.deepEqual(readSearchView(canonical), state)
  assert.equal(
    canonicalSearchUrl('http://127.0.0.1:7700/search?cursor=old', state),
    'http://127.0.0.1:7700/search?q=content%3A%22build+4.2%22+ext%3Amd&mode=both&sort=modified&desc=0&page=250&details=1',
  )
})

test('invalid URL values fall back without breaking search', () => {
  assert.deepEqual(
    readSearchView(new URLSearchParams('q=readme&mode=unknown&sort=fastest&page=999&desc=yes&details=yes')),
    { ...DEFAULT_SEARCH_VIEW, q: 'readme' },
  )
})

test('save creates a durable record and a case-insensitive name collision replaces it', () => {
  const first = upsertSavedSearch([], '  Build notes  ', state, { now: 10, id: ids('a') })
  assert.equal(first.replaced, false)
  assert.deepEqual(first.saved, { id: 'a', name: 'Build notes', state, createdAt: 10, updatedAt: 10 })

  const changed = { ...state, q: 'ext:log' }
  const second = upsertSavedSearch(first.searches, 'build NOTES', changed, { now: 20, id: ids('unused') })
  assert.equal(second.replaced, true)
  assert.equal(second.searches.length, 1)
  assert.deepEqual(second.saved, { id: 'a', name: 'build NOTES', state: changed, createdAt: 10, updatedAt: 20 })
})

test('empty and overlong names are rejected', () => {
  assert.throws(() => upsertSavedSearch([], '  ', state), /Enter a name/)
  assert.throws(() => upsertSavedSearch([], 'x'.repeat(81), state), /80 characters/)
})

test('rename updates the record and rejects collisions', () => {
  const searches = upsertSavedSearch(one(), 'Errors', { ...state, q: 'state:failed' }, { now: 20, id: ids('b') }).searches
  const renamed = renameSavedSearch(searches, 'a', 'Build history', 30)
  assert.equal(renamed[0].name, 'Build history')
  assert.equal(renamed[0].updatedAt, 30)
  assert.throws(() => renameSavedSearch(renamed, 'a', 'errors'), /already exists/)
  assert.throws(() => renameSavedSearch(renamed, 'missing', 'Nope'), /no longer exists/)
})

test('duplicate chooses an unambiguous name and delete removes only its ID', () => {
  const start = one()
  const first = duplicateSavedSearch(start, 'a', { now: 20, id: ids('b') })
  assert.equal(first.saved.name, 'Build notes copy')
  const second = duplicateSavedSearch(first.searches, 'a', { now: 30, id: ids('c') })
  assert.equal(second.saved.name, 'Build notes copy (2)')
  assert.deepEqual(deleteSavedSearch(second.searches, 'b').map((saved) => saved.id), ['a', 'c'])
})

test('current saved-search documents serialize and load without loss', () => {
  const searches = one()
  const decoded = loadSavedSearches(serializeSavedSearches(searches))
  assert.equal(decoded.discarded, 0)
  assert.deepEqual(decoded.searches, searches)
})

test('version 1 data migrates missing and renamed fields to current defaults', () => {
  const legacy = JSON.stringify({
    version: 1,
    searches: [
      {
        id: 'old',
        name: 'Legacy',
        query: 'ext:cs',
        mode: 'directories',
        sort: 'size',
        descending: false,
        createdAt: 4,
      },
    ],
  })
  const decoded = loadSavedSearches(legacy)
  assert.equal(decoded.discarded, 0)
  assert.deepEqual(decoded.searches[0], {
    id: 'old',
    name: 'Legacy',
    state: {
      q: 'ext:cs',
      mode: 'directories',
      sort: 'size',
      desc: false,
      pageSize: 100,
      details: false,
    },
    createdAt: 4,
    updatedAt: 4,
  })
})

test('malformed documents and rows are isolated instead of breaking the page', () => {
  assert.deepEqual(loadSavedSearches('{nope'), { searches: [], discarded: 1 })
  assert.deepEqual(loadSavedSearches('x'.repeat(MAX_SAVED_SEARCH_BYTES + 1)), { searches: [], discarded: 1 })
  assert.deepEqual(loadSavedSearches(JSON.stringify({ version: 99, searches: [{}] })), {
    searches: [],
    discarded: 1,
  })
  const mixed = loadSavedSearches(
    JSON.stringify({
      version: 2,
      searches: [one()[0], { id: 'bad', name: '', state: {} }, { id: 'a', name: 'duplicate id', state }],
    }),
  )
  assert.deepEqual(mixed.searches, one())
  assert.equal(mixed.discarded, 2)
})

test('stored documents and imports are bounded', () => {
  const base = one()[0]
  const oversized = Array.from({ length: MAX_SAVED_SEARCHES + 2 }, (_, i) => ({
    ...base,
    id: `stored-${i}`,
    name: `Stored ${i}`,
  }))
  const loaded = loadSavedSearches(serializeSavedSearches(oversized))
  assert.equal(loaded.searches.length, MAX_SAVED_SEARCHES)
  assert.equal(loaded.discarded, 2)

  const current = oversized.slice(0, MAX_SAVED_SEARCHES - 1)
  const incoming = oversized.slice(-2).map((saved, i) => ({ ...saved, id: `incoming-${i}` }))
  const imported = importSavedSearches(current, serializeSavedSearches(incoming))
  assert.equal(imported.searches.length, MAX_SAVED_SEARCHES)
  assert.equal(imported.imported, 1)
  assert.equal(imported.discarded, 1)

  const large = { ...state, q: 'x'.repeat(520_000) }
  const first = upsertSavedSearch([], 'Large A', large, { id: ids('large-a') }).searches
  const second = upsertSavedSearch([], 'Large B', large, { id: ids('large-b') }).searches
  assert.throws(() => importSavedSearches(first, serializeSavedSearches(second)), /1 MB/)
})

test('overlapping browser updates are serialized and preserve both changes', async () => {
  let raw: string | null = null
  let tail = Promise.resolve()
  const storage = {
    getItem: () => raw,
    setItem: (_key: string, value: string) => {
      raw = value
    },
  }
  const lock: SavedSearchLock = {
    run<T>(_name: string, action: () => T | Promise<T>): Promise<T> {
      const previous = tail
      let release = () => {}
      tail = new Promise<void>((resolve) => {
        release = resolve
      })
      return (async () => {
        await previous
        try {
          return await action()
        } finally {
          release()
        }
      })()
    },
  }
  const save = (name: string, id: string) =>
    updateSavedSearches(storage, lock, (latest) => {
      const result = upsertSavedSearch(latest, name, state, { id: ids(id) })
      return { searches: result.searches, value: id }
    })

  await Promise.all([save('From tab A', 'tab-a'), save('From tab B', 'tab-b')])
  assert.deepEqual(loadSavedSearches(raw).searches.map((saved) => saved.id), ['tab-a', 'tab-b'])
})

test('import preserves existing searches while making names and IDs unique', () => {
  const current = one()
  const incoming = [
    { ...current[0], updatedAt: 50 },
    { ...current[0], id: 'b', name: 'Other' },
    { ...current[0], id: 'c', name: 'Build notes (2)' },
  ]
  const result = importSavedSearches(current, serializeSavedSearches(incoming), ids('new-a'))
  assert.equal(result.imported, 3)
  assert.equal(result.renamed, 2)
  assert.equal(result.discarded, 0)
  assert.deepEqual(result.searches.map((saved) => saved.id), ['a', 'new-a', 'b', 'c'])
  assert.deepEqual(result.searches.map((saved) => saved.name), [
    'Build notes',
    'Build notes (2)',
    'Other',
    'Build notes (2) (2)',
  ])
})

test('an import with no valid rows reports a useful failure', () => {
  assert.throws(() => importSavedSearches([], JSON.stringify({ version: 2, searches: [{}] })), /No valid/)
})
