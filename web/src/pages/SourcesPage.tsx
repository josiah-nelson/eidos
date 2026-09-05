import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Link } from 'react-router'
import { api, type ApiInt, type SourceView, type VolumeCandidateView } from '../api'
import { EnrollCard } from './FleetPage'
import { ErrorBox, Spinner, StateBadge } from '../components'
import { ago, bytes, count, duration, integerNumber, rate, when } from '../format'

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
      {sources.data && sources.data.length === 0 && <Onboarding onManual={() => setAdding(true)} />}
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
        {s.reconciliation_deferred && (
          <>
            <dt>Automatic rescan</dt>
            <dd>
              deferred: {s.reconciliation_deferred.reason} · next check{' '}
              {when(s.reconciliation_deferred.next_eligible_at)}
            </dd>
          </>
        )}
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
                ? `${s.watcher.feed === 'macos_fsevents' ? 'FSEvents' : 'USN'} · ${count(s.watcher.events)} events in ${count(s.watcher.batches)} batches` +
                  (s.watcher.last_batch_ms_ago != null ? ` · last ${Math.round(integerNumber(s.watcher.last_batch_ms_ago) / 1000)}s ago` : '')
                : (s.watcher.detail ?? '')}
            </>
          ) : source.checkpoint_kind === 'usn' || source.checkpoint_kind === 'fsevents' ? (
            <span className="muted">not watching</span>
          ) : (
            'periodic reconciliation'
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

// First-run flow: no sources exist yet, so enumerate the machine's drives,
// let the operator pick what to index, and offer the connect-to-master step
// in the same breath. Falls back to manual path entry where drive
// enumeration is unavailable (non-Windows) or the drive is not listed.
function Onboarding({ onManual }: { onManual: () => void }) {
  const qc = useQueryClient()
  const vols = useQuery({ queryKey: ['volumes'], queryFn: api.volumes })
  const [picked, setPicked] = useState<Record<string, boolean>>({})
  const add = useMutation({
    mutationFn: async (roots: string[]) => {
      for (const root of roots) {
        await api.addSource({ name: root.replace(/[\/]+$/, ''), root_path: root, scan: true })
      }
    },
    onSuccess: () => qc.invalidateQueries({ queryKey: ['sources'] }),
  })
  if (vols.isPending) return <Spinner label="Looking at this machine's drives…" />
  const candidates = vols.data ?? []
  const isPicked = (c: VolumeCandidateView) => picked[c.root] ?? (c.drive_type === 'fixed' && !c.already_indexed)
  const chosen = candidates.filter((c) => !c.already_indexed && isPicked(c))
  return (
    <div className="cards">
      <div className="card">
        <div className="head">
          <div className="grow">
            <div className="name">Welcome — pick what to index</div>
            <div className="path">metadata first (fast, read-only); content indexing follows in the background</div>
          </div>
        </div>
        {vols.isError && <ErrorBox error={vols.error} />}
        {candidates.length === 0 && (
          <div className="muted">
            No drives could be enumerated here. Add a drive root such as <code>G:\</code> or a folder manually.
          </div>
        )}
        {candidates.length > 0 && (
          <table className="grid">
            <thead>
              <tr>
                <th />
                <th>Drive</th>
                <th>Type</th>
                <th>Filesystem</th>
                <th className="num">Size</th>
                <th className="num">Free</th>
                <th>Change feed</th>
              </tr>
            </thead>
            <tbody>
              {candidates.map((c) => (
                <tr key={c.root}>
                  <td>
                    <input
                      type="checkbox"
                      checked={c.already_indexed || isPicked(c)}
                      disabled={c.already_indexed || add.isPending}
                      onChange={(e) => setPicked({ ...picked, [c.root]: e.target.checked })}
                    />
                  </td>
                  <td>
                    <span className="mono">{c.root}</span>
                    {c.volume_name ? <span className="muted small"> {c.volume_name}</span> : null}
                    {c.already_indexed && <span className="badge ok"> indexed</span>}
                  </td>
                  <td>{c.drive_type}</td>
                  <td>{c.filesystem}</td>
                  <td className="num">{c.total_bytes !== '0' ? bytes(c.total_bytes) : '—'}</td>
                  <td className="num">{c.total_bytes !== '0' ? bytes(c.free_bytes) : '—'}</td>
                  <td>
                    {c.supports_usn ? (
                      <span className="badge ok">live (USN)</span>
                    ) : (
                      <span className="badge info">periodic rescan</span>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        {add.isError && <div className="error-text">{add.error.message}</div>}
        <div className="actions" style={{ marginTop: 8 }}>
          <button
            className="btn primary"
            disabled={chosen.length === 0 || add.isPending}
            onClick={() => add.mutate(chosen.map((c) => c.root))}
          >
            {add.isPending ? 'adding…' : `Index ${chosen.length} drive${chosen.length === 1 ? '' : 's'}`}
          </button>
          <button className="btn" onClick={onManual}>
            Add a folder manually
          </button>
        </div>
      </div>
      <EnrollCard onEnrolled={() => qc.invalidateQueries({ queryKey: ['fleet'] })} />
    </div>
  )
}
