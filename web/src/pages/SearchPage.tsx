import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { useInfiniteQuery, useQuery } from '@tanstack/react-query'
import { useVirtualizer } from '@tanstack/react-virtual'
import { Link, useSearchParams } from 'react-router'
import {
  api,
  exportUrl,
  type ApiInt,
  type CoverageEnvelope,
  type CoverageReason,
  type ExportFormat,
  type FacetField,
  type FacetValue,
  type Hit,
  type ResultMode,
  type SearchResponse,
  type SortField,
} from '../api'
import { ContentBadge, CoverageBanners, ErrorBox } from '../components'
import { ContentPreviewPanel } from '../ContentPreview'
import { bytes, count, duration, when } from '../format'
import { applyFacetClick, negate, type FacetForms } from '../query-clause'
import { SavedSearchControls } from '../SavedSearches'
import { PAGE_SIZES, canonicalSearchParams, readSearchView } from '../saved-searches'

/** What a "show context" click opens: stored text around one chunk. */
export interface PreviewTarget {
  objectId: ApiInt
  name: string
  ordinal: number
  generation?: number
}

const FILE_FACETS: FacetField[] = ['source', 'extension', 'kind', 'content_state', 'size_bucket', 'modified_bucket']
const DIR_FACETS: FacetField[] = ['source', 'size_bucket', 'modified_bucket']

const EXAMPLES: [string, string][] = [
  ['readme ext:md mtime:>=30d', 'Markdown named readme changed in the last 30 days'],
  ['name:=README.md', 'exact, case-sensitive file name'],
  ['ext:dmp', 'every dump file across all sources'],
  ['has:idb has:cs', 'directories containing both .idb and .cs beneath them (use Directories mode)'],
  ['name:/^192\\.0\\.2\\.\\d+( \\(\\d+\\))?$/', 'regex on directory names (Directories mode, sort by subtree size)'],
  ['ext:md mtime:>=30d content:zephyr content:"build 4.2"', 'recent Markdown whose text has the terms and the exact phrase'],
  ['content:=Qz mtime:>=30d', 'files whose text contains the whole word Qz, case-sensitive, with line snippets'],
  ['content:/needle-[0-9a-f]+/ ext:log', 'regex over content: required literals pick candidates, every match is verified'],
  ['size:>1G -ext:vhdx', 'large files that are not VM disks'],
  ['G:\\Tools ext:json', 'JSON files under a path'],
]

export default function SearchPage() {
  const [params, setParams] = useSearchParams()
  const view = readSearchView(params)
  const { q, mode, sort, desc, pageSize, details: showPlan } = view
  const [draft, setDraft] = useState(q)
  const [lastQ, setLastQ] = useState(q)
  const [preview, setPreview] = useState<PreviewTarget | null>(null)
  if (q !== lastQ) {
    // URL changed (back/forward, example click): resync the editor.
    setLastQ(q)
    setDraft(q)
  }

  const set = (patch: Record<string, string>) => {
    const next = new URLSearchParams(params)
    for (const [k, v] of Object.entries(patch)) {
      if (v === '') next.delete(k)
      else next.set(k, v)
    }
    setParams(next, { replace: true })
  }

  const parsed = useQuery({
    queryKey: ['parse', draft],
    queryFn: () => api.parse(draft),
    enabled: draft.trim().length > 0,
    staleTime: 60_000,
  })

  const results = useInfiniteQuery({
    queryKey: ['search', q, mode, sort, desc, pageSize],
    queryFn: ({ pageParam }) =>
      api.search({ q, mode, sort, desc, limit: pageSize, cursor: pageParam || undefined, facets: pageParam ? [] : mode === 'directories' ? DIR_FACETS : FILE_FACETS, explain: !pageParam }),
    initialPageParam: '',
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    enabled: params.has('q'),
  })
  const first = results.data?.pages[0]
  const hits = useMemo(() => results.data?.pages.flatMap((p) => p.hits) ?? [], [results.data])
  // Later pages can warn too (e.g. the index changed since the cursor was
  // issued), so show every distinct warning from every loaded page.
  const warnings = useMemo(
    () => Array.from(new Set(results.data?.pages.flatMap((p) => p.warnings ?? []) ?? [])),
    [results.data],
  )
  // Coverage can also degrade on a later page (a source dropping offline
  // mid-walk, lag appearing). The rendered rows span every loaded page, so
  // the banner merges every page's envelope: full only if every page was
  // full, with distinct reasons unioned (per source from that source's
  // entries) and watermarks from the newest page.
  const coverage = useMemo<CoverageEnvelope | undefined>(() => {
    const pages = results.data?.pages
    if (!pages || pages.length === 0) return undefined
    const key = (r: CoverageReason) => `${r.kind}|${r.severity}|${r.detail}`
    const distinct = (reasons: CoverageReason[]): CoverageReason[] => {
      const seen = new Set<string>()
      return reasons.filter((r) => {
        const k = key(r)
        if (seen.has(k)) return false
        seen.add(k)
        return true
      })
    }
    const latest = pages[pages.length - 1]
    return {
      full: pages.every((p) => p.coverage.full),
      degraded: distinct(pages.flatMap((p) => p.coverage.degraded ?? [])),
      sources: latest.coverage.sources.map((s) => ({
        ...s,
        degraded: distinct(
          pages.flatMap(
            (p) => p.coverage.sources.find((x) => x.source_id === s.source_id)?.degraded ?? [],
          ),
        ),
      })),
    }
  }, [results.data])

  const submit = () => set({ q: draft })
  /** Refine the query with a facet value, including or excluding it. */
  const click = (value: FacetForms, form: 'include' | 'exclude', siblings: FacetForms[]) => {
    const next = applyFacetClick(draft, value, form, siblings)
    setDraft(next)
    set({ q: next })
  }

  return (
    <div className="search">
      <form
        className="searchbar"
        onSubmit={(e) => {
          e.preventDefault()
          submit()
        }}
      >
        <input
          type="text"
          className="searchbox"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          placeholder="Search names, paths, and content — e.g.  readme ext:md mtime:>=30d   ·   content:=Qz   ·   has:idb has:cs"
          autoFocus
          spellCheck={false}
        />
        <select value={mode} onChange={(e) => set({ mode: e.target.value })} title="result mode">
          <option value="files">Files</option>
          <option value="directories">Directories</option>
          <option value="both">Both</option>
        </select>
        <select value={sort} onChange={(e) => set({ sort: e.target.value })} title="sort">
          <option value="relevance">Relevance</option>
          <option value="name">Name</option>
          <option value="path">Path</option>
          <option value="size">Size</option>
          <option value="allocated_size">Allocated</option>
          <option value="subtree_size">Subtree size</option>
          <option value="modified">Modified</option>
          <option value="created">Created</option>
        </select>
        <select value={pageSize} onChange={(e) => set({ page: e.target.value })} title="results per page">
          {PAGE_SIZES.map((size) => (
            <option key={size} value={size}>
              {size}/page
            </option>
          ))}
        </select>
        <button type="button" className="btn small" onClick={() => set({ desc: desc ? '0' : '1' })} title="direction">
          {desc ? '▼' : '▲'}
        </button>
        <button type="submit" className="btn primary">
          Search
        </button>
      </form>

      <div className="interp">
        {parsed.data ? (
          <>
            <span className="muted">interpreted as</span> <code>{parsed.data.rendered}</code>
            {(parsed.data.notes ?? []).map((n, i) => (
              <span key={i} className="badge info" style={{ marginLeft: 6 }}>
                {n}
              </span>
            ))}
            {parsed.data.needs_content && (
              <span className="badge info" style={{ marginLeft: 6 }} title="Sources whose content is still being indexed are reported as partial">
                content clause
              </span>
            )}
          </>
        ) : parsed.isError ? (
          <span className="error-text">{(parsed.error as Error).message}</span>
        ) : (
          <span className="muted">Type a query. Bare words match names; use field:value for filters.</span>
        )}
      </div>

      <SavedSearchControls
        state={view}
        onRun={(saved) => setParams(canonicalSearchParams(saved), { replace: true })}
      />

      {!params.has('q') && (
        <div className="card" style={{ marginTop: 12 }}>
          <h2 style={{ marginTop: 0 }}>Examples</h2>
          <table className="grid">
            <tbody>
              {EXAMPLES.map(([ex, desc]) => (
                <tr key={ex}>
                  <td>
                    <a
                      href="#"
                      onClick={(e) => {
                        e.preventDefault()
                        setDraft(ex)
                        set({ q: ex })
                      }}
                    >
                      <code>{ex}</code>
                    </a>
                  </td>
                  <td className="muted">{desc}</td>
                </tr>
              ))}
            </tbody>
          </table>
          <p className="muted" style={{ marginBottom: 0 }}>
            Fields: name, path, ext, kind, size, alloc, mtime, ctime, state, has, files, subtree, attr, source, in, content.
            Modifiers: <code>=</code> exact (case-sensitive), <code>~</code> contains (case-sensitive),{' '}
            <code>/re/</code> regex (<code>/re/c</code> case-sensitive), <code>*</code> glob, <code>-</code> not,{' '}
            <code>OR</code>, parentheses.
          </p>
        </div>
      )}

      {results.isError && <ErrorBox error={results.error} />}
      {first && (
        <div className="results">
          <div className="results-main">
            {coverage && <CoverageBanners coverage={coverage} />}
            {warnings.map((w, i) => (
              <div key={i} className="banner warn">
                <span className="badge warn">warning</span>
                <span>{w}</span>
              </div>
            ))}
            <div className="toolbar" style={{ marginBottom: 6 }}>
              <span className="muted">
                {count(first.total.value)}
                {first.total.exact ? '' : '+'} results · {duration(first.timing.total_ms)} (plan{' '}
                {first.timing.plan_ms.toFixed(1)} · retrieve {first.timing.retrieve_ms.toFixed(1)} · verify{' '}
                {first.timing.verify_ms.toFixed(1)} · join {first.timing.join_ms.toFixed(1)} ms)
              </span>
              <div style={{ flex: 1 }} />
              {first.explanation && (
                <button className="btn small" onClick={() => set({ details: showPlan ? '' : '1' })}>
                  {showPlan ? 'Hide plan' : 'Explain'}
                </button>
              )}
              <ExportMenu q={q} mode={mode} sort={sort} desc={desc} page={first} />
            </div>
            {showPlan && first.explanation && (
              <div className="card" style={{ marginBottom: 8 }}>
                <div>
                  <strong>Plan:</strong> {first.explanation.readable || '(match everything)'}
                </div>
                <table className="grid" style={{ marginTop: 6 }}>
                  <tbody>
                    {first.explanation.steps.map((s, i) => (
                      <tr key={i}>
                        <td>
                          <span className="badge">{s.stage}</span>
                        </td>
                        <td>{s.description}</td>
                        <td className="num">{s.candidates != null ? `${count(s.candidates)} candidates` : ''}</td>
                        <td className="num">{s.verified != null ? `${count(s.verified)} verified` : ''}</td>
                        <td className="num">{s.elapsed_ms != null ? `${s.elapsed_ms.toFixed(1)} ms` : ''}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
            <HitTable
              hits={hits}
              mode={mode}
              hasMore={results.hasNextPage ?? false}
              fetching={results.isFetchingNextPage}
              fetchMore={() => results.fetchNextPage()}
              onPreview={setPreview}
            />
          </div>
          <aside className="side">
            {(first.facets ?? []).filter((f) => f.values.length > 0).map((f) => (
              <div className="card" key={f.field}>
                <h2 style={{ marginTop: 0 }}>{f.field.replace('_', ' ')}</h2>
                <ul className="facet-list">
                  {f.values.slice(0, 12).map((v) => {
                    const forms = facetForms(f.field, v)
                    // Range buckets are disjoint, so picking one supersedes another.
                    const siblings = f.values.flatMap((o) =>
                      o !== v && o.range ? [o.range] : [],
                    )
                    return (
                      <li key={v.value}>
                        {forms ? (
                          <a
                            href="#"
                            title={`add  ${forms.clause}`}
                            onClick={(e) => {
                              e.preventDefault()
                              click(forms, 'include', siblings)
                            }}
                          >
                            {v.label ?? v.value ?? '(none)'}
                          </a>
                        ) : (
                          <span title={v.label}>{v.label ?? v.value}</span>
                        )}
                        <span>{count(v.count)}</span>
                        {forms ? (
                          <a
                            href="#"
                            className="facet-exclude"
                            title={`add  ${forms.exclude}`}
                            aria-label={`exclude ${v.label ?? v.value}`}
                            onClick={(e) => {
                              e.preventDefault()
                              click(forms, 'exclude', siblings)
                            }}
                          >
                            −
                          </a>
                        ) : (
                          <span />
                        )}
                      </li>
                    )
                  })}
                </ul>
                {f.truncated && <div className="muted">more values not shown</div>}
              </div>
            ))}
          </aside>
        </div>
      )}
      {preview && (
        <ContentPreviewPanel
          objectId={preview.objectId}
          name={preview.name}
          ordinal={preview.ordinal}
          generation={preview.generation}
          onClose={() => setPreview(null)}
        />
      )}
    </div>
  )
}

/**
 * The clauses that select and exclude one facet value. Range buckets carry
 * them from the server; term facets get them from the value itself. `null`
 * when the value cannot be turned into a filter at all.
 */
function facetForms(field: FacetField, v: FacetValue): FacetForms | null {
  if (v.range) return v.range
  const clause = facetClause(field, v.value, v.label)
  return clause ? { clause, exclude: negate(clause) } : null
}

/**
 * Export the current query. The links hit `/api/search/export`, which walks
 * the cursors server-side and streams; the browser never holds the result set.
 */
function ExportMenu({
  q,
  mode,
  sort,
  desc,
  page,
}: {
  q: string
  mode: ResultMode
  sort: SortField
  desc: boolean
  page: SearchResponse
}) {
  const health = useQuery({ queryKey: ['health'], queryFn: api.health, staleTime: 60_000 })
  const cap = health.data?.export_max_rows
  const [copied, setCopied] = useState('')
  const href = (format: ExportFormat, bom = false) => exportUrl({ q, mode, sort, desc }, format, bom)
  const copyPage = async () => {
    try {
      await navigator.clipboard.writeText(JSON.stringify(page, null, 2))
      setCopied('Copied')
    } catch {
      setCopied('Clipboard unavailable')
    }
  }
  return (
    <details className="menu">
      <summary className="btn small">Export</summary>
      <div className="menu-body">
        <div className="muted">
          Streams every matching row{cap ? ` · up to ${count(cap)} rows` : ''}
          {page.total.value > (cap ?? Infinity) ? ' · this query would be truncated' : ''}
        </div>
        <a href={href('csv')} download>
          CSV
        </a>
        <a href={href('csv', true)} download>
          CSV with BOM (Excel)
        </a>
        <a href={href('json')} download>
          JSON
        </a>
        <a href={href('ndjson')} download>
          NDJSON (one row per line)
        </a>
        <button type="button" className="btn small" onClick={copyPage}>
          {copied || 'Copy this page as JSON'}
        </button>
      </div>
    </details>
  )
}

function facetClause(field: FacetField, value: string, label?: string): string | null {
  switch (field) {
    case 'extension':
      return `ext:${value === '' ? 'none' : value}`
    case 'kind':
      return `kind:${value}`
    case 'content_state':
      return `state:${value}`
    case 'source':
      return `source:${label ?? value}`
    default:
      return null
  }
}

function HitTable({
  hits,
  mode,
  hasMore,
  fetching,
  fetchMore,
  onPreview,
}: {
  hits: Hit[]
  mode: ResultMode
  hasMore: boolean
  fetching: boolean
  fetchMore: () => void
  onPreview: (t: PreviewTarget) => void
}) {
  const parentRef = useRef<HTMLDivElement>(null)
  const virtualizer = useVirtualizer({
    count: hits.length,
    getScrollElement: () => parentRef.current,
    estimateSize: (i) => 30 + (hits[i]?.snippets?.length ?? 0) * 18,
    overscan: 20,
  })
  const items = virtualizer.getVirtualItems()
  useEffect(() => {
    const last = items[items.length - 1]
    if (last && hasMore && !fetching && last.index >= hits.length - 30) fetchMore()
  }, [items, hasMore, fetching, hits.length, fetchMore])
  const dirMode = mode === 'directories'
  return (
    <div className="vtable results-table" ref={parentRef}>
      <div className="vrow header hitrow">
        <div className="col">Name</div>
        <div className="col">Path</div>
        <div className="col num">{dirMode ? 'Subtree' : 'Size'}</div>
        <div className="col num">{dirMode ? 'Files' : ''}</div>
        <div className="col">Modified</div>
        <div className="col">{dirMode ? 'Extensions' : 'Content'}</div>
      </div>
      {hits.length === 0 && <div className="empty">No matches.</div>}
      <div style={{ height: virtualizer.getTotalSize(), position: 'relative' }}>
        {items.map((vi) => {
          const h = hits[vi.index]
          const isDir = h.kind === 'directory'
          const parent = h.path ? h.path.slice(0, Math.max(0, h.path.lastIndexOf('\\'))) : ''
          const snippets = h.snippets ?? []
          return (
            <div
              key={`${h.entry_id ?? h.object_id}`}
              data-index={vi.index}
              ref={virtualizer.measureElement}
              className={`vrow hitrow ${snippets.length ? 'with-snippets' : ''}`}
              style={{ position: 'absolute', top: 0, left: 0, right: 0, transform: `translateY(${vi.start}px)` }}
            >
              <div className={`col name ${isDir ? 'dir' : ''}`}>
                <span className="icon">{isDir ? '▸' : '·'}</span>
                {isDir ? (
                  <Link to={`/browse/${h.object_id}`}>{h.name || h.path}</Link>
                ) : h.parent_id != null ? (
                  <Link to={`/browse/${h.parent_id}`} title="open containing folder" style={{ color: 'inherit' }}>
                    {h.name}
                  </Link>
                ) : (
                  h.name
                )}
                {h.hard_link_count > 1 && <span className="badge info">{h.hard_link_count} links</span>}
                {h.directory && !h.directory.complete && <span className="badge warn">incomplete</span>}
              </div>
              <div className="col mono muted" title={h.path}>
                {parent}
              </div>
              <div className="col num">{bytes(isDir ? h.directory?.logical_bytes : h.size)}</div>
              <div className="col num muted">{isDir ? count(h.directory?.file_count) : ''}</div>
              <div className="col muted">{when(isDir ? h.directory?.newest_modified : h.modified)}</div>
              <div className="col">
                {isDir ? (
                  <span className="muted">
                    {Object.entries(h.directory?.extension_counts ?? {})
                      .sort((a, b) => (BigInt(a[1]) === BigInt(b[1]) ? 0 : BigInt(a[1]) < BigInt(b[1]) ? 1 : -1))
                      .slice(0, 5)
                      .map(([e, n]) => `${e || '∅'} ${n}`)
                      .join(' · ')}
                  </span>
                ) : h.content.state === 'indexed' || h.content.state === 'partial' ? (
                  <button
                    className="linkish"
                    title="Show the text the indexer stored for this file"
                    onClick={() =>
                      onPreview({
                        objectId: h.object_id,
                        name: h.name,
                        ordinal: snippets[0]?.chunk_ordinal ?? 0,
                        generation: h.content.generation,
                      })
                    }
                  >
                    <ContentBadge state={h.content.state} />
                  </button>
                ) : (
                  <ContentBadge state={h.content.state} />
                )}
              </div>
              {snippets.length > 0 && (
                <div className="snippets">
                  {snippets.map((s) => (
                    <div key={s.chunk_ordinal + ':' + s.line_start}>
                      <span className="line">L{BigInt(s.line_start) + 1n}</span>
                      <Highlighted text={s.text} ranges={s.highlights} />{' '}
                      <button
                        className="linkish"
                        title="Show the indexed text around this match"
                        onClick={() =>
                          onPreview({
                            objectId: h.object_id,
                            name: h.name,
                            ordinal: s.chunk_ordinal,
                            generation: h.content.generation,
                          })
                        }
                      >
                        show context
                      </button>
                    </div>
                  ))}
                </div>
              )}
            </div>
          )
        })}
      </div>
      {fetching && (
        <div className="muted" style={{ padding: 8 }}>
          Loading more…
        </div>
      )}
    </div>
  )
}

/** Render text with `[start, end)` character ranges wrapped in <mark>. */
function Highlighted({ text, ranges }: { text: string; ranges: [number, number][] }) {
  const chars = Array.from(text)
  const parts: ReactNode[] = []
  let pos = 0
  const sorted = [...ranges].sort((a, b) => a[0] - b[0])
  sorted.forEach(([a, b], i) => {
    if (a > pos) parts.push(chars.slice(pos, a).join(''))
    if (b > a) parts.push(<mark key={i}>{chars.slice(Math.max(a, pos), b).join('')}</mark>)
    pos = Math.max(pos, b)
  })
  if (pos < chars.length) parts.push(chars.slice(pos).join(''))
  return <>{parts}</>
}
