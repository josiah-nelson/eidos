import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Link } from 'react-router'
import { api, type ActivitySourceView } from '../api'
import { ErrorBox, Spinner, StateBadge } from '../components'
import { bytes, count, duration } from '../format'

const STATE_ORDER = ['indexed', 'partial', 'pending', 'stale', 'failed', 'unsupported', 'excluded']

function StateBar({ states }: { states: Record<string, number> }) {
  const total = Object.values(states).reduce((a, b) => a + b, 0)
  if (total === 0) return <span className="muted">no files</span>
  return (
    <div className="statebar" title={STATE_ORDER.map((k) => `${k}: ${states[k] ?? 0}`).join('\n')}>
      {STATE_ORDER.filter((k) => (states[k] ?? 0) > 0).map((k) => (
        <span key={k} className={`seg ${k}`} style={{ width: `${(100 * (states[k] ?? 0)) / total}%` }} />
      ))}
    </div>
  )
}

function SourceRow({ s }: { s: ActivitySourceView }) {
  const qc = useQueryClient()
  const policy = useMutation({
    mutationFn: (body: { enabled?: boolean; concurrency?: number }) => api.setContentPolicy(s.source_id, body),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['activity'] })
      qc.invalidateQueries({ queryKey: ['sources'] })
    },
  })
  const total = Object.values(s.content_states).reduce((a, b) => a + b, 0)
  const done = (s.content_states.indexed ?? 0) + (s.content_states.partial ?? 0)
  const terminal = done + (s.content_states.failed ?? 0) + (s.content_states.unsupported ?? 0) + (s.content_states.excluded ?? 0)
  return (
    <tr>
      <td>
        <Link to={`/sources/${s.source_id}`}>{s.name}</Link> <StateBadge state={s.state} />
      </td>
      <td>
        <label className="toggle">
          <input
            type="checkbox"
            checked={s.content_enabled}
            disabled={policy.isPending}
            onChange={(e) => policy.mutate({ enabled: e.target.checked })}
          />{' '}
          {s.content_enabled ? 'on' : 'off'}
        </label>
      </td>
      <td className="num">
        <input
          type="number"
          min={1}
          max={64}
          className="small"
          defaultValue={s.content_concurrency}
          key={s.content_concurrency}
          disabled={policy.isPending}
          onBlur={(e) => {
            const v = Number(e.target.value)
            if (Number.isFinite(v) && v >= 1 && v !== s.content_concurrency) policy.mutate({ concurrency: v })
          }}
        />
      </td>
      <td className="num">{count(s.jobs_queued)}</td>
      <td className="num">{count(s.jobs_running)}</td>
      <td style={{ minWidth: 180 }}>
        <StateBar states={s.content_states} />
        <div className="muted small">
          {count(terminal)} / {count(total)} files settled · {count(s.content_states.pending ?? 0)} pending
        </div>
      </td>
      <td className="num">{bytes(s.content_bytes_indexed)}</td>
    </tr>
  )
}

export default function ActivityPage() {
  const q = useQuery({ queryKey: ['activity'], queryFn: api.activity, refetchInterval: 2000 })
  if (q.isPending) return <Spinner label="Loading activity…" />
  if (q.isError) return <ErrorBox error={q.error} />
  const a = q.data
  const w = a.workers
  const settled = w.files_indexed + w.files_unsupported + w.files_failed
  return (
    <>
      <div className="toolbar">
        <h1 style={{ margin: 0 }}>Activity</h1>
        <div style={{ flex: 1 }} />
        <span className={`badge ${a.content_enabled ? 'ok' : 'warn'}`}>
          content extraction {a.content_enabled ? 'running' : 'paused (--no-content)'}
        </span>
      </div>

      <div className="stats">
        <div className="stat">
          <div className="label">Throughput (60 s)</div>
          <div className="value">{bytes(Math.round(w.throughput_bytes_per_s))}/s</div>
          <div className="sub">{w.workers} workers · {w.current.length} busy</div>
        </div>
        <div className="stat">
          <div className="label">Content queue</div>
          <div className="value">{count(a.jobs.queued)}</div>
          <div className="sub">
            {count(a.jobs.running)} running · oldest {a.jobs.oldest_queued_age_ms ? duration(a.jobs.oldest_queued_age_ms) : '—'}
          </div>
        </div>
        <div className="stat">
          <div className="label">Files this session</div>
          <div className="value">{count(settled)}</div>
          <div className="sub">
            {count(w.files_indexed)} indexed · {count(w.files_unsupported)} binary · {count(w.files_failed)} failed ·{' '}
            {count(w.files_retried)} retried
          </div>
        </div>
        <div className="stat">
          <div className="label">Bytes read this session</div>
          <div className="value">{bytes(w.bytes_read)}</div>
          <div className="sub">{count(w.chunks_written)} chunks · uptime {duration(w.uptime_s * 1000)}</div>
        </div>
        <div className="stat">
          <div className="label">Content index</div>
          <div className="value">{count(a.content_index_documents)}</div>
          <div className="sub">
            chunk documents · {count(w.commits)} commits · {count(w.uncommitted_documents)} uncommitted ·{' '}
            {count(w.pending_publish)} awaiting publish
          </div>
        </div>
        <div className="stat">
          <div className="label">Stored content</div>
          <div className="value">{bytes(a.content.indexed_bytes)}</div>
          <div className="sub">
            {count(a.content.total_records)} records · {count(a.content.chunks)} chunks
          </div>
        </div>
        <div className="stat">
          <div className="label">Catalog index follower</div>
          <div className="value">{count(a.follower.outbox_pending)}</div>
          <div className="sub">
            outbox rows pending · {count(a.follower.index_documents)} documents
            {a.follower.rebuilding_source != null ? ` · rebuilding source ${a.follower.rebuilding_source}` : ''}
          </div>
        </div>
      </div>

      {(w.last_error || a.follower.last_error) && (
        <div className="banner bad">
          {w.last_error && <div>content workers: {w.last_error}</div>}
          {a.follower.last_error && <div>index follower: {a.follower.last_error}</div>}
        </div>
      )}

      <h2>Sources</h2>
      <table className="grid">
        <thead>
          <tr>
            <th>Source</th>
            <th>Content</th>
            <th className="num">Concurrency</th>
            <th className="num">Queued</th>
            <th className="num">Running</th>
            <th>Content states</th>
            <th className="num">Indexed bytes</th>
          </tr>
        </thead>
        <tbody>
          {a.sources.map((s) => (
            <SourceRow key={s.source_id} s={s} />
          ))}
        </tbody>
      </table>

      <h2>Workers</h2>
      {w.current.length === 0 ? (
        <div className="muted">idle</div>
      ) : (
        <table className="grid">
          <thead>
            <tr>
              <th>Worker</th>
              <th>Source</th>
              <th>Object</th>
              <th className="num">Size</th>
              <th className="num">Running for</th>
            </tr>
          </thead>
          <tbody>
            {w.current.map((c) => (
              <tr key={c.worker}>
                <td className="mono">{c.worker}</td>
                <td>{a.sources.find((s) => s.source_id === c.source_id)?.name ?? c.source_id}</td>
                <td className="mono">
                  <Link to={`/browse/${c.object_id}`}>#{c.object_id}</Link>
                </td>
                <td className="num">{bytes(c.size)}</td>
                <td className="num">{duration(c.started_ms_ago)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      {a.recent_failures.length > 0 && (
        <>
          <h2>Recent failures</h2>
          <table className="grid">
            <thead>
              <tr>
                <th>Job</th>
                <th>Object</th>
                <th>Class</th>
                <th className="num">Attempts</th>
                <th>Error</th>
              </tr>
            </thead>
            <tbody>
              {a.recent_failures.map((j) => (
                <tr key={j.id}>
                  <td className="mono">{j.id}</td>
                  <td className="mono">{j.object_id != null ? <Link to={`/browse/${j.object_id}`}>#{j.object_id}</Link> : '—'}</td>
                  <td>{j.failure_class ?? ''}</td>
                  <td className="num">{j.attempts}</td>
                  <td className="mono small">{j.last_error ?? ''}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </>
      )}
    </>
  )
}
