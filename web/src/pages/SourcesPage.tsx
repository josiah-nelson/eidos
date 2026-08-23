import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Link } from 'react-router'
import { api, type ApiInt, type SourceView } from '../api'
import { ErrorBox, Spinner, StateBadge } from '../components'
import { ago, bytes, count, duration, integerNumber, rate } from '../format'

export default function SourcesPage() {
  const qc = useQueryClient()
  const sources = useQuery({
    queryKey: ['sources'],
    queryFn: api.sources,
    refetchInterval: (q) => (q.state.data?.some((s) => s.scan?.running) ? 1000 : 5000),
  })
  const [adding, setAdding] = useState(false)
  const scan = useMutation({
    mutationFn: (id: ApiInt) => api.scanSource(id),
    onSettled: () => qc.invalidateQueries({ queryKey: ['sources'] }),
  })
  const cancel = useMutation({
    mutationFn: (id: ApiInt) => api.cancelScan(id),
    onSettled: () => qc.invalidateQueries({ queryKey: ['sources'] }),
  })

  return (
    <>
      <div className="toolbar">
        <h1 style={{ margin: 0 }}>Sources</h1>
        <div className="spacer" style={{ flex: 1 }} />
        <button className="btn primary" onClick={() => setAdding(true)}>
          Add source
        </button>
      </div>
      {sources.isError && <ErrorBox error={sources.error} />}
      {scan.isError && <ErrorBox error={scan.error} />}
      {sources.isPending && <Spinner />}
      {sources.data && sources.data.length === 0 && (
        <div className="empty">
          No sources yet. Add a drive root such as <code>G:\</code> or a folder to begin.
        </div>
      )}
      <div className="cards">
        {sources.data?.map((s) => (
          <SourceCard
            key={s.source.id}
            s={s}
            onScan={() => scan.mutate(s.source.id)}
            onCancel={() => cancel.mutate(s.source.id)}
            busy={scan.isPending}
          />
        ))}
      </div>
      {adding && <AddSourceModal onClose={() => setAdding(false)} />}
    </>
  )
}

function SourceCard({
  s,
  onScan,
  onCancel,
  busy,
}: {
  s: SourceView
  onScan: () => void
  onCancel: () => void
  busy: boolean
}) {
  const { source, counts, completeness, scan } = s
  const running = scan?.running ?? false
  return (
    <div className="card">
      <div className="head">
        <div className="grow">
          <div className="name">
            {source.name} <StateBadge state={source.state} />
            {integerNumber(completeness.listing_errors) > 0 && (
              <span className="badge warn" title="directories that could not be listed">
                {count(completeness.listing_errors)} unlisted
              </span>
            )}
          </div>
          <div className="path" title={source.root_path}>
            {source.root_path} · {source.kind.replace('_', ' ')}
          </div>
        </div>
        {source.root_object_id != null && (
          <Link className="btn small" to={`/browse/${source.root_object_id}`}>
            Browse
          </Link>
        )}
        <Link className="btn small" to={`/sources/${source.id}`}>
          Details
        </Link>
        {running ? (
          <button className="btn small" onClick={onCancel}>
            Cancel
          </button>
        ) : (
          <button className="btn small primary" onClick={onScan} disabled={busy}>
            {source.published_generation == null ? 'Scan' : 'Rescan'}
          </button>
        )}
      </div>
      {running && scan && (
        <div>
          <div className="muted" style={{ fontSize: 12 }}>
            Scanning… {count(scan.dirs)} dirs · {count(scan.entries)} entries · {rate(scan.entries_per_sec)} ·{' '}
            {duration(scan.elapsed_ms)}
            {integerNumber(scan.errors) > 0 ? ` · ${scan.errors} errors` : ''}
          </div>
          <div className="progress">
            <div className="bar" />
          </div>
        </div>
      )}
      {scan?.error && !running && <div className="error-text">Last scan failed: {scan.error}</div>}
      <div className="stats">
        <div className="stat">
          <div className="label">Files</div>
          <div className="value">{count(counts.files)}</div>
          <div className="sub">{count(counts.directories)} dirs</div>
        </div>
        <div className="stat">
          <div className="label">Apparent</div>
          <div className="value">{bytes(counts.logical_bytes)}</div>
          <div className="sub">logical size</div>
        </div>
        <div className="stat">
          <div className="label">Allocated</div>
          <div className="value">{bytes(counts.allocated_bytes)}</div>
          <div className="sub">on disk</div>
        </div>
        <div className="stat">
          <div className="label">Indexed</div>
          <div className="value">{count(counts.content_indexed)}</div>
          <div className="sub" title={`${count(counts.content_excluded)} excluded by policy`}>
            {count(counts.content_pending)} pending
          </div>
        </div>
      </div>
      <dl className="kv">
        <dt>Completeness</dt>
        <dd>
          {completeness.metadata_complete ? 'metadata complete' : 'metadata incomplete'} ·{' '}
          {completeness.content_complete ? 'content complete' : `${count(completeness.content_pending)} pending`} ·
          freshness {completeness.freshness}
        </dd>
        <dt>Last scan</dt>
        <dd>
          {ago(source.last_scan_completed_at)}
          {source.published_generation != null ? ` (generation ${source.published_generation})` : ''}
        </dd>
        <dt>Change feed</dt>
        <dd>
          {s.watcher ? (
            <>
              <span
                className={`badge ${s.watcher.live ? 'ok' : s.watcher.state === 'reconciling' ? 'accent' : s.watcher.state === 'stopped' ? 'warn' : ''}`}
              >
                {s.watcher.state}
              </span>{' '}
              {s.watcher.live
                ? `USN · ${count(s.watcher.events)} events in ${count(s.watcher.batches)} batches` +
                  (s.watcher.last_batch_ms_ago != null ? ` · last ${Math.round(integerNumber(s.watcher.last_batch_ms_ago) / 1000)}s ago` : '')
                : (s.watcher.detail ?? '')}
            </>
          ) : source.kind === 'smb' || source.kind === 'windows_generic' ? (
            'periodic reconciliation'
          ) : (
            <span className="muted">not watching</span>
          )}
        </dd>
        {source.state_reason && (
          <>
            <dt>Note</dt>
            <dd>{source.state_reason}</dd>
          </>
        )}
        {integerNumber(counts.open_errors) > 0 && (
          <>
            <dt>Errors</dt>
            <dd>
              <Link to={`/sources/${source.id}`}>{count(counts.open_errors)} open</Link>
            </dd>
          </>
        )}
      </dl>
    </div>
  )
}

function AddSourceModal({ onClose }: { onClose: () => void }) {
  const qc = useQueryClient()
  const [name, setName] = useState('')
  const [root, setRoot] = useState('')
  const [scanNow, setScanNow] = useState(true)
  const add = useMutation({
    mutationFn: () => api.addSource({ name: name.trim(), root_path: root.trim(), scan: scanNow }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['sources'] })
      onClose()
    },
  })
  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h1>Add source</h1>
        <form
          className="form"
          onSubmit={(e) => {
            e.preventDefault()
            add.mutate()
          }}
        >
          <label>
            Name
            <input type="text" value={name} onChange={(e) => setName(e.target.value)} placeholder="G:" autoFocus />
          </label>
          <label>
            Root path
            <input
              type="text"
              value={root}
              onChange={(e) => setRoot(e.target.value)}
              placeholder="G:\  or  \\server\share  or  D:\Projects"
            />
          </label>
          <label style={{ display: 'flex', gap: 6, alignItems: 'center' }}>
            <input type="checkbox" checked={scanNow} onChange={(e) => setScanNow(e.target.checked)} />
            Start a metadata scan immediately (read-only)
          </label>
          {add.isError && <div className="error-text">{(add.error as Error).message}</div>}
          <div className="actions">
            <button type="button" className="btn" onClick={onClose}>
              Cancel
            </button>
            <button type="submit" className="btn primary" disabled={add.isPending || !name.trim() || !root.trim()}>
              Add
            </button>
          </div>
        </form>
      </div>
    </div>
  )
}
