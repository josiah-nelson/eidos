import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Link } from 'react-router'
import {
  api,
  type ActivitySourceView,
  type ApiInt,
  type ContentStatusView,
  type RetryReport,
} from '../api'
import { ErrorBox, Spinner, StateBadge } from '../components'
import { bytes, count, duration, integerNumber, when } from '../format'

const STATE_ORDER = ['indexed', 'partial', 'pending', 'stale', 'failed', 'unsupported', 'excluded']

function StateBar({ states }: { states: Record<string, ApiInt> }) {
  const value = (key: string) => integerNumber(states[key] ?? '0')
  const total = Object.values(states).reduce((sum, count) => sum + integerNumber(count), 0)
  if (total === 0) return <span className="muted">no files</span>
  return (
    <div className="statebar" title={STATE_ORDER.map((k) => `${k}: ${states[k] ?? 0}`).join('\n')}>
      {STATE_ORDER.filter((k) => value(k) > 0).map((k) => (
        <span key={k} className={`seg ${k}`} style={{ width: `${(100 * value(k)) / total}%` }} />
      ))}
    </div>
  )
}

const FLOW_BADGE: Record<ContentStatusView['flow'], string> = {
  disabled: 'warn',
  stopped: 'warn',
  draining: 'warn',
  waiting: 'info',
  running: 'ok',
}

const SEARCH_BADGE: Record<ContentStatusView['search'], string> = {
  ready: 'ok',
  rebuilding: 'warn',
  failed: 'bad',
  disabled: 'warn',
}

// The global extraction pool. Per-volume caps (the Volume cap column
// below) bound how much of this pool one disk can absorb, so raising a
// volume's cap past the pool size has no effect until the pool grows too.
// The server clamps and persists the choice across restarts.
function WorkerPoolControl({ workers }: { workers: number }) {
  const qc = useQueryClient()
  const resize = useMutation({
    mutationFn: (n: number) => api.setContentWorkers(n),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['activity'] }),
  })
  return (
    <span title="global extraction worker pool, shared across all volumes; per-volume caps apply on top">
      pool{' '}
      <input
        type="number"
        min={1}
        max={64}
        className="small"
        defaultValue={workers}
        key={workers}
        disabled={resize.isPending}
        onBlur={(e) => {
          const v = Number(e.target.value)
          if (Number.isFinite(v) && v >= 1 && v !== workers) resize.mutate(v)
        }}
      />
      {resize.isError ? <span className="muted small"> {resize.error.message}</span> : null}
    </span>
  )
}

// Pause/resume for the whole extraction pipeline. The server answers the
// switch with the state it produced, so the badge next to the button is the
// service's own answer rather than an optimistic guess — which matters
// because pausing a busy service lands on "draining", not "stopped".
//
// `--no-content` is a process setting chosen at start-up: there is nothing
// to resume into, so the button is not offered at all.
function ContentControl({ content }: { content: ContentStatusView }) {
  const qc = useQueryClient()
  const toggle = useMutation({
    mutationFn: () => api.setContentPaused(!content.paused),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['activity'] }),
  })
  return (
    <>
      {toggle.isError && <span className="badge bad">{(toggle.error as Error).message}</span>}
      <span className={`badge ${SEARCH_BADGE[content.search]}`} title={content.detail}>
        content search {content.search}
      </span>
      <span className={`badge ${FLOW_BADGE[content.flow]}`} title={content.flow_reason}>
        extraction {content.flow}
      </span>
      {content.enabled && (
        <button
          onClick={() => toggle.mutate()}
          disabled={toggle.isPending}
          title={
            content.paused
              ? 'Claim content jobs again'
              : 'Stop claiming new jobs; batches already claimed finish and publish. Survives a restart.'
          }
        >
          {content.paused ? 'Resume extraction' : 'Pause extraction'}
        </button>
      )}
    </>
  )
}

function retrySummary(r: RetryReport): string {
  const parts = [`${count(r.accepted)} requeued`]
  if (integerNumber(r.skipped) > 0) parts.push(`${count(r.skipped)} skipped (${Object.keys(r.skipped_reasons).join(', ')})`)
  if (integerNumber(r.rejected) > 0) parts.push(`${count(r.rejected)} rejected (${Object.keys(r.rejected_reasons).join(', ')})`)
  return parts.join(' · ')
}

// Two-step bulk retry: the first click previews (count + bytes), the second
// one acts. No browser dialogs. The confirmation carries the preview's
// `as_of`, count, and exact-set confirmation token. Anything that failed in
// between is excluded by `as_of`; if an older candidate changed, the token no
// longer matches and the server asks for a fresh preview rather than
// substituting work the operator was not shown.
function BulkRetry({ s }: { s: ActivitySourceView }) {
  const qc = useQueryClient()
  const [preview, setPreview] = useState<RetryReport | null>(null)
  const [done, setDone] = useState<RetryReport | null>(null)
  const ask = useMutation({
    mutationFn: () => api.retrySourceContent(s.source_id, { preview: true }),
    onSuccess: (r) => {
      setDone(null)
      setPreview(r)
    },
  })
  const apply = useMutation({
    mutationFn: (p: RetryReport) =>
      api.retrySourceContent(s.source_id, {
        preview: false,
        limit: integerNumber(p.accepted),
        as_of: p.as_of,
        confirmation: p.confirmation ?? undefined,
      }),
    onSuccess: (r) => {
      setPreview(null)
      setDone(r)
      qc.invalidateQueries({ queryKey: ['activity'] })
    },
  })
  const busy = ask.isPending || apply.isPending
  const error = ask.error ?? apply.error
  if (integerNumber(s.jobs_failed) === 0 && !preview && !done) return <span className="muted">—</span>
  return (
    <div>
      <div>
        {count(s.jobs_failed)} failed{integerNumber(s.jobs_failed) > 0 ? ` · ${bytes(s.jobs_failed_bytes)}` : ''}
      </div>
      {preview === null ? (
        integerNumber(s.jobs_failed) > 0 && (
          <button className="btn small" disabled={busy} onClick={() => ask.mutate()}>
            {ask.isPending ? 'checking…' : 'retry failed'}
          </button>
        )
      ) : (
        <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap' }}>
          <button
            className="btn small primary"
            disabled={busy || integerNumber(preview.accepted) === 0}
            onClick={() => apply.mutate(preview)}
          >
            {apply.isPending ? 'requeueing…' : `confirm ${count(preview.accepted)} job(s) · ${bytes(preview.bytes)}`}
          </button>
          <button className="btn small" disabled={busy} onClick={() => setPreview(null)}>
            cancel
          </button>
        </div>
      )}
      {preview !== null && (integerNumber(preview.skipped) > 0 || integerNumber(preview.rejected) > 0) && (
        <div className="muted small">{retrySummary(preview)}</div>
      )}
      {done && <div className="muted small">{retrySummary(done)}</div>}
      {error && <div className="muted small">{error.message}</div>}
    </div>
  )
}

function RetryJobButton({ id }: { id: ApiInt }) {
  const qc = useQueryClient()
  const retry = useMutation({
    mutationFn: () => api.retryJob(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['activity'] }),
  })
  if (retry.isError) return <span className="muted small">{retry.error.message}</span>
  if (retry.data && integerNumber(retry.data.accepted) === 0) return <span className="muted small">{retrySummary(retry.data)}</span>
  return (
    <button className="btn small" disabled={retry.isPending || retry.isSuccess} onClick={() => retry.mutate()}>
      {retry.isPending ? 'retrying…' : retry.isSuccess ? 'requeued' : 'retry'}
    </button>
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
  const state = (key: string) => integerNumber(s.content_states[key] ?? '0')
  const total = Object.values(s.content_states).reduce((sum, value) => sum + integerNumber(value), 0)
  const done = state('indexed') + state('partial')
  const terminal = done + state('failed') + state('unsupported') + state('excluded')
  return (
    <tr>
      <td>
        <Link to={`/sources/${s.source_id}`}>{s.name}</Link> <StateBadge state={s.state} />
        {s.reconciliation_deferred && (
          <div className="muted small" title={`next check ${when(s.reconciliation_deferred.next_eligible_at)}`}>
            rescan deferred: {s.reconciliation_deferred.reason}
          </div>
        )}
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
        <div
          className="muted small"
          title="workers reading this volume right now; the input above is this volume's cap on the global pool"
        >
          {s.content_reserved} reading now (peak {s.content_peak_reserved})
        </div>
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
      <td>
        <BulkRetry s={s} />
      </td>
    </tr>
  )
}

export default function ActivityPage() {
  const q = useQuery({ queryKey: ['activity'], queryFn: api.activity, refetchInterval: 2000 })
  if (q.isPending) return <Spinner label="Loading activity…" />
  if (q.isError) return <ErrorBox error={q.error} />
  const a = q.data
  const w = a.workers
  const recovered =
    integerNumber(a.startup_recovery.aborted_scan_generations) +
    integerNumber(a.startup_recovery.requeued_running_jobs) +
    integerNumber(a.startup_recovery.requeued_unfinished_content)
  const settled = integerNumber(w.files_indexed) + integerNumber(w.files_unsupported) + integerNumber(w.files_failed)
  return (
    <>
      <div className="toolbar">
        <h1 style={{ margin: 0 }}>Activity</h1>
        <div style={{ flex: 1 }} />
        <ContentControl content={a.content_status} />
      </div>
      <p className="muted small" style={{ marginTop: 0 }}>
        {a.content_status.detail}
      </p>

      <div className="stats">
        <div className="stat">
          <div className="label">Throughput (60 s)</div>
          <div className="value">{bytes(Math.round(w.throughput_bytes_per_s))}/s</div>
          <div className="sub">
            <WorkerPoolControl workers={integerNumber(w.workers)} /> · {w.current.length} busy
          </div>
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
            {count(w.files_retried)} retried · ≈
            {(integerNumber(w.uptime_s) > 0 ? settled / integerNumber(w.uptime_s) : 0).toFixed(1)} files/s
          </div>
        </div>
        <div className="stat">
          <div className="label">Bytes read this session</div>
          <div className="value">{bytes(w.bytes_read)}</div>
          <div className="sub">{count(w.chunks_written)} chunks · uptime {duration(integerNumber(w.uptime_s) * 1000)}</div>
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
          <div className="label">Index rebuild</div>
          <div className="value">{a.content_status.rebuild.phase}</div>
          <div className="sub">
            {a.content_status.rebuild.phase === 'idle'
              ? 'index complete with respect to stored chunks'
              : `${count(a.content_status.rebuild.docs)} of ${count(a.content_status.rebuild.chunks)} documents · ${duration(a.content_status.rebuild.elapsed_ms)}`}
            {a.content_status.rebuild.error ? ` · ${a.content_status.rebuild.error}` : ''}
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
          <div className="label">On-disk footprint</div>
          <div className="value">
            {bytes(
              integerNumber(a.storage.catalog_db_bytes) +
                integerNumber(a.storage.catalog_index_bytes) +
                integerNumber(a.storage.content_index_bytes),
            )}
          </div>
          <div className="sub">
            catalog {bytes(a.storage.catalog_db_bytes)} · name index {bytes(a.storage.catalog_index_bytes)} · content
            index {bytes(a.storage.content_index_bytes)}
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
        <div className="stat">
          <div className="label">Catalog writer wait</div>
          <div className="value">{duration(a.catalog_writer.max_wait_ms)}</div>
          <div className="sub">
            max · {duration(a.catalog_writer.total_wait_ms)} total ·{' '}
            {count(a.catalog_writer.contended_acquisitions)} / {count(a.catalog_writer.acquisitions)} contended ·{' '}
            {count(a.catalog_writer.waiting)} waiting
          </div>
        </div>
        <div className="stat">
          <div className="label">Catalog writer hold</div>
          <div className="value">{duration(a.catalog_writer.max_hold_ms)}</div>
          <div className="sub">
            max · {duration(a.catalog_writer.total_hold_ms)} total
          </div>
        </div>
        <div className="stat">
          <div className="label">Recovered at startup</div>
          <div className="value">{count(recovered)}</div>
          <div className="sub">
            {count(a.startup_recovery.aborted_scan_generations)} scans aborted ·{' '}
            {count(a.startup_recovery.requeued_running_jobs)} running jobs requeued ·{' '}
            {count(a.startup_recovery.requeued_unfinished_content)} unfinished content records requeued
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
            <th className="num" title="per-volume cap on the global worker pool">
              Volume cap
            </th>
            <th className="num">Queued</th>
            <th className="num">Running</th>
            <th>Content states</th>
            <th className="num">Indexed bytes</th>
            <th>Failed jobs</th>
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
                <th />
              </tr>
            </thead>
            <tbody>
              {a.recent_failures.map((j) => (
                <tr key={j.id}>
                  <td className="mono">{j.id}</td>
                  <td className="mono">{j.object_id != null ? <Link to={`/browse/${j.object_id}`}>#{j.object_id}</Link> : '—'}</td>
                  <td>{j.failure_class ?? ''}</td>
                  <td className="num">
                    {j.attempts}
                    {j.requeue_count > 0 ? ` (${j.requeue_count}× requeued)` : ''}
                  </td>
                  <td className="mono small">{j.last_error ?? ''}</td>
                  <td>
                    <RetryJobButton id={j.id} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </>
      )}
    </>
  )
}
