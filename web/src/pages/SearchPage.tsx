import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { useInfiniteQuery, useQuery } from '@tanstack/react-query'
import { useVirtualizer } from '@tanstack/react-virtual'
import { Link, useSearchParams } from 'react-router'
import { api, type FacetField, type Hit, type ResultMode, type SortField } from '../api'
import { CompletenessBanner, ContentBadge, ErrorBox } from '../components'
import { bytes, count, duration, when } from '../format'

const PAGE = 100
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
  const q = params.get('q') ?? ''
  const mode = (params.get('mode') as ResultMode) || 'files'
  const sort = (params.get('sort') as SortField) || 'relevance'
  const desc = params.get('desc') !== '0'
  const [draft, setDraft] = useState(q)
  const [lastQ, setLastQ] = useState(q)
  const [showPlan, setShowPlan] = useState(false)
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
    queryKey: ['search', q, mode, sort, desc],
    queryFn: ({ pageParam }) =>
      api.search({ q, mode, sort, desc, limit: PAGE, cursor: pageParam || undefined, facets: pageParam ? [] : mode === 'directories' ? DIR_FACETS : FILE_FACETS, explain: !pageParam }),
    initialPageParam: '',
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    enabled: params.has('q'),
  })
  const first = results.data?.pages[0]
  const hits = useMemo(() => results.data?.pages.flatMap((p) => p.hits) ?? [], [results.data])

  const submit = () => set({ q: draft })
  const addClause = (clause: string) => {
    const next = draft.trim().length ? `${draft.trim()} ${clause}` : clause
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
            {first.completeness.map((c) => (
              <CompletenessBanner key={c.source_id} c={c} />
            ))}
            {(first.warnings ?? []).map((w, i) => (
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
                <button className="btn small" onClick={() => setShowPlan((v) => !v)}>
                  {showPlan ? 'Hide plan' : 'Explain'}
                </button>
              )}
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
            />
          </div>
          <aside className="side">
            {(first.facets ?? []).filter((f) => f.values.length > 0).map((f) => (
              <div className="card" key={f.field}>
                <h2 style={{ marginTop: 0 }}>{f.field.replace('_', ' ')}</h2>
                <ul className="facet-list">
                  {f.values.slice(0, 12).map((v) => {
                    const clause = facetClause(f.field, v.value, v.label)
                    return (
                      <li key={v.value}>
                        {clause ? (
                          <a
                            href="#"
                            onClick={(e) => {
                              e.preventDefault()
                              addClause(clause)
                            }}
                          >
                            {v.label ?? v.value ?? '(none)'}
                          </a>
                        ) : (
                          <span>{v.label ?? v.value}</span>
                        )}
                        <span>{count(v.count)}</span>
                        <span />
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
    </div>
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
}: {
  hits: Hit[]
  mode: ResultMode
  hasMore: boolean
  fetching: boolean
  fetchMore: () => void
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
          const isDir = h.kind === 'directory' || h.kind === 'virtual_directory'
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
              <div className="col num">
                {bytes(
                  h.kind === 'virtual_directory'
                    ? h.directory?.archive_declared_bytes
                    : isDir
                      ? h.directory?.logical_bytes
                      : h.size,
                )}
              </div>
              <div className="col num muted">{isDir ? count(h.directory?.file_count) : ''}</div>
              <div className="col muted">{when(isDir ? h.directory?.newest_modified : h.modified)}</div>
              <div className="col">
                {isDir ? (
                  <span className="muted">
                    {Object.entries(h.directory?.extension_counts ?? {})
                      .sort((a, b) => b[1] - a[1])
                      .slice(0, 5)
                      .map(([e, n]) => `${e || '∅'} ${n}`)
                      .join(' · ')}
                  </span>
                ) : (
                  <ContentBadge state={h.content.state} />
                )}
              </div>
              {snippets.length > 0 && (
                <div className="snippets">
                  {snippets.map((s) => (
                    <div key={s.chunk_ordinal + ':' + s.line_start}>
                      <span className="line">L{s.line_start + 1}</span>
                      <Highlighted text={s.text} ranges={s.highlights} />
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
