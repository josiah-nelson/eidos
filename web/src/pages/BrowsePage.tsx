import { useEffect, useMemo, useRef, useState } from 'react'
import { useInfiniteQuery, useQuery } from '@tanstack/react-query'
import { useVirtualizer } from '@tanstack/react-virtual'
import { Link, useNavigate, useParams, useSearchParams } from 'react-router'
import { api, type ChildRow, type ChildSort } from '../api'
import { CompletenessBanner, ContentBadge, ErrorBox, Spinner } from '../components'
import { attrFlags, bytes, count, when } from '../format'
import { Treemap, type TreemapItem } from '../Treemap'

const PAGE = 500

export default function BrowsePage() {
  const { objectId } = useParams()
  const navigate = useNavigate()
  const [params, setParams] = useSearchParams()
  const sort = (params.get('sort') as ChildSort) || 'name'
  const desc = params.get('desc') === '1'
  const hidden = params.get('hidden') !== '0'

  const sources = useQuery({ queryKey: ['sources'], queryFn: api.sources, enabled: !objectId })
  useEffect(() => {
    if (!objectId && sources.data) {
      const first = sources.data.find((s) => s.source.root_object_id != null)
      if (first) navigate(`/browse/${first.source.root_object_id}`, { replace: true })
    }
  }, [objectId, sources.data, navigate])

  const oid = Number(objectId)
  const enabled = Number.isFinite(oid) && oid > 0

  const children = useInfiniteQuery({
    queryKey: ['children', oid, sort, desc, hidden],
    queryFn: ({ pageParam }) => api.children(oid, { sort, desc, offset: pageParam, limit: PAGE, hidden }),
    initialPageParam: 0,
    getNextPageParam: (last) => {
      const next = last.offset + last.rows.length
      return next < last.total ? next : undefined
    },
    enabled,
  })
  const extensions = useQuery({
    queryKey: ['extensions', oid],
    queryFn: () => api.extensions(oid, 25),
    enabled,
  })
  const object = useQuery({ queryKey: ['object', oid], queryFn: () => api.object(oid), enabled })

  const rows = useMemo(() => children.data?.pages.flatMap((p) => p.rows) ?? [], [children.data])
  const first = children.data?.pages[0]
  const total = first?.total ?? 0
  const [showMap, setShowMap] = useState(true)
  const treemapItems: TreemapItem[] = useMemo(
    () =>
      rows.map((r) => ({
        id: r.object.id,
        label: r.entry.name,
        value: r.object.kind === 'directory' ? (r.aggregate?.logical_bytes ?? 0) : r.object.size,
        isDir: r.object.kind === 'directory',
        detail: r.object.kind === 'directory' ? `${count(r.aggregate?.file_count)} files` : undefined,
      })),
    [rows],
  )
  const treemapTotal = useMemo(() => treemapItems.reduce((s, i) => s + i.value, 0), [treemapItems])

  const setSort = (field: ChildSort) => {
    const next = new URLSearchParams(params)
    if (sort === field) {
      next.set('desc', desc ? '0' : '1')
    } else {
      next.set('sort', field)
      next.set('desc', field === 'name' || field === 'kind' ? '0' : '1')
    }
    setParams(next, { replace: true })
  }

  if (!objectId) {
    if (sources.isPending) return <Spinner />
    return <div className="empty">No scanned sources to browse yet. Add and scan a source first.</div>
  }
  if (children.isError) return <ErrorBox error={children.error} />

  return (
    <div className="browser">
      <div className="listing">
        {first && <CompletenessBanner c={first.source} />}
        <Breadcrumbs path={first?.path ?? object.data?.path ?? null} objectId={oid} />
        <div className="toolbar" style={{ marginBottom: 8 }}>
          <span className="muted">
            {count(total)} entries
            {object.data?.aggregate && (
              <>
                {' '}
                · subtree {count(object.data.aggregate.file_count)} files, {count(object.data.aggregate.dir_count)}{' '}
                dirs · apparent {bytes(object.data.aggregate.logical_bytes)} · allocated{' '}
                {bytes(object.data.aggregate.allocated_bytes)}
                {!object.data.aggregate.complete && (
                  <>
                    {' '}
                    <span className="badge warn">incomplete</span>
                  </>
                )}
              </>
            )}
          </span>
          <div style={{ flex: 1 }} />
          {first?.parent_id != null && (
            <Link className="btn small" to={`/browse/${first.parent_id}`}>
              ↑ Up
            </Link>
          )}
          <button className="btn small" onClick={() => setShowMap((v) => !v)}>
            {showMap ? 'Hide treemap' : 'Treemap'}
          </button>
          <label className="muted" style={{ display: 'flex', gap: 4, alignItems: 'center' }}>
            <input
              type="checkbox"
              checked={hidden}
              onChange={(e) => {
                const next = new URLSearchParams(params)
                next.set('hidden', e.target.checked ? '1' : '0')
                setParams(next, { replace: true })
              }}
            />
            hidden/system
          </label>
        </div>
        {showMap && treemapItems.length > 0 && (
          <div style={{ marginBottom: 8 }}>
            <Treemap items={treemapItems} total={treemapTotal} />
            {rows.length < total && (
              <div className="muted" style={{ fontSize: 11.5, marginTop: 2 }}>
                treemap shows the {count(rows.length)} loaded of {count(total)} entries
              </div>
            )}
          </div>
        )}
        <ChildrenTable
          rows={rows}
          total={total}
          sort={sort}
          desc={desc}
          onSort={setSort}
          loading={children.isPending}
          fetching={children.isFetchingNextPage}
          hasMore={children.hasNextPage ?? false}
          fetchMore={() => children.fetchNextPage()}
        />
      </div>
      <aside className="side">
        <div className="card">
          <h2 style={{ marginTop: 0 }}>Extensions in subtree</h2>
          {extensions.data && extensions.data.length === 0 && <div className="muted">No files beneath this directory.</div>}
          {extensions.data && extensions.data.length > 0 && (
            <ul className="facet-list">
              {extensions.data.map((x) => {
                const max = extensions.data[0].count
                return (
                  <li key={x.extension}>
                    <span className="mono">{x.extension === '' ? '(no extension)' : `.${x.extension}`}</span>
                    <span>{count(x.count)}</span>
                    <span className="muted">{bytes(x.bytes)}</span>
                    <div className="bar" style={{ width: `${Math.max(2, (100 * x.count) / max)}%` }} />
                  </li>
                )
              })}
            </ul>
          )}
        </div>
        {object.data && (
          <div className="card">
            <h2 style={{ marginTop: 0 }}>This directory</h2>
            <dl className="kv">
              <dt>Object</dt>
              <dd className="mono">o:{object.data.object.id}</dd>
              <dt>Identity</dt>
              <dd>{object.data.object.identity_confidence.replace('_', ' ')}</dd>
              <dt>Modified</dt>
              <dd>{when(object.data.object.modified)}</dd>
              <dt>Created</dt>
              <dd>{when(object.data.object.created)}</dd>
              <dt>Attributes</dt>
              <dd className="mono">{attrFlags(object.data.object.attributes) || '—'}</dd>
              {object.data.aggregate && (
                <>
                  <dt>Newest descendant</dt>
                  <dd>{when(object.data.aggregate.newest_modified)}</dd>
                  <dt>Content</dt>
                  <dd>
                    {count(object.data.aggregate.content_indexed)} indexed ·{' '}
                    {count(object.data.aggregate.content_pending)} pending ·{' '}
                    {count(object.data.aggregate.content_excluded)} excluded
                  </dd>
                </>
              )}
            </dl>
          </div>
        )}
      </aside>
    </div>
  )
}

function Breadcrumbs({ path, objectId }: { path: string | null; objectId: number }) {
  // Crumbs are rendered from the current path string; navigation to
  // ancestors uses the parent link chain (cheap and always correct).
  const parts = path ? path.split('\\').filter((p) => p.length > 0) : []
  return (
    <div className="crumbs">
      {parts.length === 0 && <span className="muted">o:{objectId}</span>}
      {parts.map((p, i) => (
        <span key={i}>
          {i > 0 && <span className="sep">\</span>}
          <span>{p}</span>
        </span>
      ))}
    </div>
  )
}

function ChildrenTable({
  rows,
  total,
  sort,
  desc,
  onSort,
  loading,
  fetching,
  hasMore,
  fetchMore,
}: {
  rows: ChildRow[]
  total: number
  sort: ChildSort
  desc: boolean
  onSort: (f: ChildSort) => void
  loading: boolean
  fetching: boolean
  hasMore: boolean
  fetchMore: () => void
}) {
  const parentRef = useRef<HTMLDivElement>(null)
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 28,
    overscan: 20,
  })
  const items = virtualizer.getVirtualItems()
  useEffect(() => {
    const last = items[items.length - 1]
    if (last && hasMore && !fetching && last.index >= rows.length - 50) fetchMore()
  }, [items, hasMore, fetching, rows.length, fetchMore])

  const arrow = (f: ChildSort) => (sort === f ? (desc ? ' ▼' : ' ▲') : '')

  return (
    <div className="vtable" ref={parentRef}>
      <div className="vrow header">
        <div className="col" onClick={() => onSort('name')}>
          Name{arrow('name')}
        </div>
        <div className="col num" onClick={() => onSort('size')}>
          Apparent{arrow('size')}
        </div>
        <div className="col num" onClick={() => onSort('allocated_size')}>
          Allocated{arrow('allocated_size')}
        </div>
        <div className="col num">Files</div>
        <div className="col num">Dirs</div>
        <div className="col" onClick={() => onSort('modified')}>
          Modified{arrow('modified')}
        </div>
        <div className="col" onClick={() => onSort('kind')}>
          Content{arrow('kind')}
        </div>
      </div>
      {loading && <Spinner label="Loading directory…" />}
      {!loading && rows.length === 0 && <div className="empty">Empty directory ({count(total)} entries).</div>}
      <div style={{ height: virtualizer.getTotalSize(), position: 'relative' }}>
        {items.map((vi) => {
          const r = rows[vi.index]
          const isDir = r.object.kind === 'directory'
          const agg = r.aggregate
          return (
            <div
              key={r.entry.id}
              className="vrow"
              style={{ position: 'absolute', top: 0, left: 0, right: 0, transform: `translateY(${vi.start}px)` }}
            >
              <div className={`col name ${isDir ? 'dir' : ''}`}>
                <span className="icon">{isDir ? '▸' : r.object.kind === 'reparse' ? '↪' : '·'}</span>
                {isDir ? (
                  <Link to={`/browse/${r.object.id}`}>{r.entry.name}</Link>
                ) : (
                  <span title={`o:${r.object.id}${r.object.link_count > 1 ? ` · ${r.object.link_count} links` : ''}`}>
                    {r.entry.name}
                  </span>
                )}
                {!isDir && r.object.link_count > 1 && <span className="badge info">{r.object.link_count} links</span>}
              </div>
              <div className="col num">{bytes(isDir ? agg?.logical_bytes : r.object.size)}</div>
              <div className="col num">{bytes(isDir ? agg?.allocated_bytes : r.object.allocated)}</div>
              <div className="col num muted">{isDir ? count(agg?.file_count) : ''}</div>
              <div className="col num muted">{isDir ? count(agg?.dir_count) : ''}</div>
              <div className="col muted">{when(isDir ? (agg?.newest_modified ?? r.object.modified) : r.object.modified)}</div>
              <div className="col">
                {isDir ? (
                  agg && !agg.complete ? (
                    <span className="badge warn">incomplete</span>
                  ) : (
                    <span className="muted">
                      {agg ? `${count(agg.content_pending)} pending` : '—'}
                    </span>
                  )
                ) : (
                  <ContentBadge state={r.object.content_state} />
                )}
              </div>
            </div>
          )
        })}
      </div>
      {fetching && <div className="muted" style={{ padding: 8 }}>Loading more…</div>}
    </div>
  )
}
