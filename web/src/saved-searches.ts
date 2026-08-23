export const SAVED_SEARCHES_STORAGE_KEY = 'eidos.saved-searches'
export const SAVED_SEARCHES_VERSION = 2
export const PAGE_SIZES = [25, 50, 100, 250] as const

type ResultMode = 'files' | 'directories' | 'both'
type SortField =
  | 'relevance'
  | 'name'
  | 'path'
  | 'size'
  | 'allocated_size'
  | 'subtree_size'
  | 'modified'
  | 'created'

export interface SearchViewState {
  q: string
  mode: ResultMode
  sort: SortField
  desc: boolean
  pageSize: number
  details: boolean
}

export interface SavedSearch {
  id: string
  name: string
  state: SearchViewState
  createdAt: number
  updatedAt: number
}

export interface SavedSearchLoad {
  searches: SavedSearch[]
  discarded: number
}

const MODES = new Set<ResultMode>(['files', 'directories', 'both'])
const SORTS = new Set<SortField>([
  'relevance',
  'name',
  'path',
  'size',
  'allocated_size',
  'subtree_size',
  'modified',
  'created',
])

export const DEFAULT_SEARCH_VIEW: SearchViewState = {
  q: '',
  mode: 'files',
  sort: 'relevance',
  desc: true,
  pageSize: 100,
  details: false,
}

/** Read only the durable search/view fields from the URL. */
export function readSearchView(params: URLSearchParams): SearchViewState {
  const mode = params.get('mode')
  const sort = params.get('sort')
  return {
    q: params.get('q') ?? '',
    mode: isMode(mode) ? mode : DEFAULT_SEARCH_VIEW.mode,
    sort: isSort(sort) ? sort : DEFAULT_SEARCH_VIEW.sort,
    desc: params.get('desc') !== '0',
    pageSize: pageSize(params.get('page')),
    details: params.get('details') === '1',
  }
}

/**
 * Produce the canonical, shareable URL state. Cursor and other transient or
 * unknown parameters cannot leak in because this function starts from empty.
 */
export function canonicalSearchParams(state: SearchViewState): URLSearchParams {
  const params = new URLSearchParams()
  params.set('q', state.q)
  params.set('mode', state.mode)
  params.set('sort', state.sort)
  params.set('desc', state.desc ? '1' : '0')
  params.set('page', String(pageSize(String(state.pageSize))))
  if (state.details) params.set('details', '1')
  return params
}

export function canonicalSearchUrl(base: string, state: SearchViewState): string {
  const url = new URL('/search', base)
  url.search = canonicalSearchParams(state).toString()
  return url.toString()
}

/** Decode current and legacy browser-local saved-search documents. */
export function loadSavedSearches(raw: string | null): SavedSearchLoad {
  if (!raw) return { searches: [], discarded: 0 }
  let value: unknown
  try {
    value = JSON.parse(raw)
  } catch {
    return { searches: [], discarded: 1 }
  }
  const root = record(value)
  if (!root || !Array.isArray(root.searches)) return { searches: [], discarded: 1 }
  const version = number(root.version)
  if (version !== 1 && version !== SAVED_SEARCHES_VERSION) {
    return { searches: [], discarded: root.searches.length || 1 }
  }

  const searches: SavedSearch[] = []
  let discarded = 0
  for (const item of root.searches) {
    const decoded = decodeSavedSearch(item, version)
    if (decoded && !searches.some((saved) => saved.id === decoded.id)) searches.push(decoded)
    else discarded += 1
  }
  return { searches, discarded }
}

export function serializeSavedSearches(searches: SavedSearch[]): string {
  return JSON.stringify({ version: SAVED_SEARCHES_VERSION, searches }, null, 2)
}

export function upsertSavedSearch(
  searches: SavedSearch[],
  name: string,
  state: SearchViewState,
  options: { now?: number; id?: () => string } = {},
): { searches: SavedSearch[]; saved: SavedSearch; replaced: boolean } {
  const clean = cleanName(name)
  const now = options.now ?? Date.now()
  const at = searches.findIndex((saved) => sameName(saved.name, clean))
  if (at >= 0) {
    const saved = { ...searches[at], name: clean, state: normalizeState(state), updatedAt: now }
    return { searches: searches.map((item, i) => (i === at ? saved : item)), saved, replaced: true }
  }
  const saved: SavedSearch = {
    id: uniqueId(searches, options.id ?? randomId),
    name: clean,
    state: normalizeState(state),
    createdAt: now,
    updatedAt: now,
  }
  return { searches: [...searches, saved], saved, replaced: false }
}

export function renameSavedSearch(searches: SavedSearch[], id: string, name: string, now = Date.now()): SavedSearch[] {
  const clean = cleanName(name)
  if (searches.some((saved) => saved.id !== id && sameName(saved.name, clean))) {
    throw new Error(`A saved search named "${clean}" already exists.`)
  }
  let found = false
  const next = searches.map((saved) => {
    if (saved.id !== id) return saved
    found = true
    return { ...saved, name: clean, updatedAt: now }
  })
  if (!found) throw new Error('The saved search no longer exists.')
  return next
}

export function duplicateSavedSearch(
  searches: SavedSearch[],
  id: string,
  options: { now?: number; id?: () => string } = {},
): { searches: SavedSearch[]; saved: SavedSearch } {
  const source = searches.find((saved) => saved.id === id)
  if (!source) throw new Error('The saved search no longer exists.')
  const now = options.now ?? Date.now()
  const saved: SavedSearch = {
    ...source,
    id: uniqueId(searches, options.id ?? randomId),
    name: uniqueName(searches, `${source.name} copy`),
    createdAt: now,
    updatedAt: now,
  }
  return { searches: [...searches, saved], saved }
}

export function deleteSavedSearch(searches: SavedSearch[], id: string): SavedSearch[] {
  return searches.filter((saved) => saved.id !== id)
}

/** Import valid rows; preserve existing data and rename collisions explicitly. */
export function importSavedSearches(
  searches: SavedSearch[],
  raw: string,
  id: () => string = randomId,
): { searches: SavedSearch[]; imported: number; renamed: number; discarded: number } {
  const decoded = loadSavedSearches(raw)
  if (decoded.searches.length === 0) throw new Error('No valid saved searches were found in that file.')
  const next = [...searches]
  let renamed = 0
  for (const incoming of decoded.searches) {
    const name = uniqueName(next, incoming.name)
    if (name !== incoming.name) renamed += 1
    next.push({
      ...incoming,
      id: next.some((saved) => saved.id === incoming.id) ? uniqueId(next, id) : incoming.id,
      name,
      state: normalizeState(incoming.state),
    })
  }
  return { searches: next, imported: decoded.searches.length, renamed, discarded: decoded.discarded }
}

export function nameCollision(searches: SavedSearch[], name: string, exceptId?: string): SavedSearch | undefined {
  const clean = name.trim()
  return searches.find((saved) => saved.id !== exceptId && sameName(saved.name, clean))
}

function decodeSavedSearch(value: unknown, version: number): SavedSearch | null {
  const item = record(value)
  if (!item || typeof item.id !== 'string' || typeof item.name !== 'string' || item.name.trim() === '') return null
  const rawState = version === 1 ? item : record(item.state)
  if (!rawState) return null
  const q = version === 1 ? rawState.query : rawState.q
  if (typeof q !== 'string') return null
  const createdAt = number(item.createdAt) ?? 0
  const updatedAt = number(item.updatedAt) ?? createdAt
  return {
    id: item.id,
    name: item.name.trim(),
    state: normalizeState({
      q,
      mode: isMode(rawState.mode) ? rawState.mode : DEFAULT_SEARCH_VIEW.mode,
      sort: isSort(rawState.sort) ? rawState.sort : DEFAULT_SEARCH_VIEW.sort,
      desc: boolean(version === 1 ? rawState.descending : rawState.desc) ?? DEFAULT_SEARCH_VIEW.desc,
      pageSize: pageSize(version === 1 ? rawState.limit : rawState.pageSize),
      details: boolean(version === 1 ? rawState.showPlan : rawState.details) ?? DEFAULT_SEARCH_VIEW.details,
    }),
    createdAt,
    updatedAt,
  }
}

function normalizeState(state: SearchViewState): SearchViewState {
  return {
    q: String(state.q),
    mode: isMode(state.mode) ? state.mode : DEFAULT_SEARCH_VIEW.mode,
    sort: isSort(state.sort) ? state.sort : DEFAULT_SEARCH_VIEW.sort,
    desc: Boolean(state.desc),
    pageSize: pageSize(state.pageSize),
    details: Boolean(state.details),
  }
}

function pageSize(value: unknown): number {
  const parsed = typeof value === 'number' ? value : typeof value === 'string' ? Number(value) : NaN
  return PAGE_SIZES.includes(parsed as (typeof PAGE_SIZES)[number]) ? parsed : DEFAULT_SEARCH_VIEW.pageSize
}

function isMode(value: unknown): value is ResultMode {
  return typeof value === 'string' && MODES.has(value as ResultMode)
}

function isSort(value: unknown): value is SortField {
  return typeof value === 'string' && SORTS.has(value as SortField)
}

function cleanName(value: string): string {
  const clean = value.trim()
  if (!clean) throw new Error('Enter a name for this saved search.')
  if (clean.length > 80) throw new Error('Saved-search names must be 80 characters or fewer.')
  return clean
}

function sameName(a: string, b: string): boolean {
  return a.localeCompare(b, undefined, { sensitivity: 'accent' }) === 0
}

function uniqueName(searches: SavedSearch[], desired: string): string {
  if (!searches.some((saved) => sameName(saved.name, desired))) return desired
  let suffix = 2
  while (searches.some((saved) => sameName(saved.name, `${desired} (${suffix})`))) suffix += 1
  return `${desired} (${suffix})`
}

function uniqueId(searches: SavedSearch[], create: () => string): string {
  for (let attempt = 0; attempt < 10; attempt += 1) {
    const candidate = create()
    if (candidate && !searches.some((saved) => saved.id === candidate)) return candidate
  }
  throw new Error('Could not create a unique saved-search ID.')
}

function randomId(): string {
  return globalThis.crypto.randomUUID()
}

function record(value: unknown): Record<string, unknown> | null {
  return value != null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null
}

function number(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null
}

function boolean(value: unknown): boolean | null {
  return typeof value === 'boolean' ? value : null
}
