import { useQuery } from '@tanstack/react-query'
import { Link, useParams } from 'react-router'
import { api } from '../api'
import { CompletenessBanner, ContentPolicyControl, ErrorBox, Spinner, StateBadge } from '../components'
import { bytes, count, duration, humanState, when } from '../format'

export default function SourceDetailPage() {
  const { id } = useParams()
  const sid = id ?? ''
  const enabled = /^[1-9]\d*$/.test(sid)
  const detail = useQuery({
    queryKey: ['source', sid],
    queryFn: () => api.source(sid),
    refetchInterval: (q) => (q.state.data?.scan?.running ? 1000 : 5000),
    enabled,
  })
  const errors = useQuery({
    queryKey: ['source-errors', sid],
    queryFn: () => api.sourceErrors(sid, false),
    enabled,
  })

  if (detail.isPending) return <Spinner />
  if (detail.isError) return <ErrorBox error={detail.error} />
  const d = detail.data
  const s = d.source
  const agg = d.root_aggregate

  return (
    <>
      <div className="toolbar">
        <h1 style={{ margin: 0 }}>
          <Link to="/sources">Sources</Link> / {s.name} <StateBadge state={s.state} />
        </h1>
        <div style={{ flex: 1 }} />
        <ContentPolicyControl sourceId={s.id} enabled={s.content_enabled} concurrency={s.content_concurrency} />
        {s.root_object_id != null && (
          <Link className="btn" to={`/browse/${s.root_object_id}`}>
            Browse
          </Link>
        )}
      </div>
      <CompletenessBanner c={d.completeness} />

      <div className="cards">
        <div className="card">
          <h2 style={{ marginTop: 0 }}>Source</h2>
          <dl className="kv">
            <dt>Root</dt>
            <dd className="mono">{s.root_path}</dd>
            <dt>Kind</dt>
            <dd>{humanState(s.kind)}</dd>
            <dt>Published generation</dt>
            <dd>{s.published_generation ?? '—'}</dd>
            <dt>Policy version</dt>
            <dd>{s.policy_version}</dd>
            <dt>Created</dt>
            <dd>{when(s.created_at)}</dd>
            <dt>Last scan started</dt>
            <dd>{when(s.last_scan_started_at)}</dd>
            <dt>Last scan completed</dt>
            <dd>{when(s.last_scan_completed_at)}</dd>
            {s.state_reason && (
              <>
                <dt>Reason</dt>
                <dd>{s.state_reason}</dd>
              </>
            )}
          </dl>
        </div>
        <div className="card">
          <h2 style={{ marginTop: 0 }}>Totals</h2>
          <dl className="kv">
            <dt>Files</dt>
            <dd>{count(d.counts.files)}</dd>
            <dt>Directories</dt>
            <dd>{count(d.counts.directories)}</dd>
            <dt>Apparent size</dt>
            <dd>{bytes(d.counts.logical_bytes, 2)}</dd>
            <dt>Allocated size</dt>
            <dd>{bytes(d.counts.allocated_bytes, 2)}</dd>
            <dt>Newest file</dt>
            <dd>{when(agg?.newest_modified)}</dd>
            <dt>Oldest file</dt>
            <dd>{when(agg?.oldest_modified)}</dd>
            <dt>Content pending</dt>
            <dd>{count(d.counts.content_pending)}</dd>
            <dt>Content excluded</dt>
            <dd>{count(d.counts.content_excluded)}</dd>
            <dt>Content unsupported</dt>
            <dd>{count(d.counts.content_unsupported)}</dd>
            <dt>Aggregates complete</dt>
            <dd>{agg ? (agg.complete ? 'yes' : 'no — some directories unlisted') : '—'}</dd>
          </dl>
        </div>
      </div>

      <h2>Content exclusions</h2>
      {d.exclusions.length === 0 ? (
        <div className="muted">No policy exclusions recorded.</div>
      ) : (
        <table className="grid">
          <thead>
            <tr>
              <th>Stage</th>
              <th>Reason</th>
              <th className="num">Files</th>
              <th className="num">Bytes</th>
            </tr>
          </thead>
          <tbody>
            {d.exclusions.map((x) => (
              <tr key={`${x.stage}-${x.reason}`}>
                <td>{x.stage}</td>
                <td>{humanState(x.reason)}</td>
                <td className="num">{count(x.count)}</td>
                <td className="num">{bytes(x.bytes)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      <h2>Scan generations</h2>
      <table className="grid">
        <thead>
          <tr>
            <th className="num">Gen</th>
            <th>Kind</th>
            <th>State</th>
            <th>Started</th>
            <th>Duration</th>
            <th className="num">Dirs</th>
            <th className="num">Entries</th>
            <th className="num">Errors</th>
            <th className="num">Tombstoned</th>
            <th>Note</th>
          </tr>
        </thead>
        <tbody>
          {d.generations.map((g) => (
            <tr key={g.generation}>
              <td className="num">{g.generation}</td>
              <td>{g.kind}</td>
              <td>
                <span
                  className={`badge ${g.state === 'published' ? 'ok' : g.state === 'open' ? 'accent' : 'warn'}`}
                >
                  {g.state}
                </span>
              </td>
              <td>{when(g.started_at)}</td>
              <td>{g.finished_at ? duration((BigInt(g.finished_at) - BigInt(g.started_at)) / 1_000_000n) : '—'}</td>
              <td className="num">{count(g.dirs_listed)}</td>
              <td className="num">{count(g.entries_seen)}</td>
              <td className="num">{count(g.errors)}</td>
              <td className="num">{count(g.tombstoned)}</td>
              <td className="muted">{g.note ?? ''}</td>
            </tr>
          ))}
        </tbody>
      </table>

      <h2>Open errors {errors.data ? `(${errors.data.length})` : ''}</h2>
      {errors.isError && <ErrorBox error={errors.error} />}
      {errors.data && errors.data.length === 0 && <div className="muted">No open errors.</div>}
      {errors.data && errors.data.length > 0 && (
        <table className="grid">
          <thead>
            <tr>
              <th>When</th>
              <th>Stage</th>
              <th>Kind</th>
              <th className="num">Code</th>
              <th>Path</th>
              <th>Message</th>
            </tr>
          </thead>
          <tbody>
            {errors.data.map((e) => (
              <tr key={e.id}>
                <td>{when(e.occurred_at)}</td>
                <td>{e.stage}</td>
                <td>
                  <span className="badge warn">{e.kind}</span>
                </td>
                <td className="num">{e.code}</td>
                <td className="mono">
                  {e.object_id != null ? <Link to={`/browse/${e.object_id}`}>{e.path}</Link> : e.path}
                </td>
                <td className="muted">{e.message}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </>
  )
}
