import { useMutation, useQueryClient } from '@tanstack/react-query'
import { api, type ApiRouteId, type ContentState, type SourceCompleteness, type SourceState } from './api'
import { ago, count, humanState, integerNumber } from './format'

export function StateBadge({ state }: { state: SourceState }) {
  const cls =
    state === 'complete' || state === 'metadata_complete'
      ? 'ok'
      : state === 'content_pending' || state === 'enumerating' || state === 'reconciling'
        ? 'accent'
        : state === 'degraded' || state === 'stale' || state === 'offline'
          ? 'warn'
          : state === 'retired'
            ? ''
            : 'info'
  return <span className={`badge ${cls}`}>{humanState(state)}</span>
}

export function ContentBadge({ state }: { state: ContentState }) {
  const cls =
    state === 'indexed'
      ? 'ok'
      : state === 'partial'
        ? 'info'
        : state === 'pending' || state === 'stale'
          ? 'accent'
          : state === 'failed'
            ? 'bad'
            : state === 'excluded'
              ? 'warn'
              : ''
  if (state === 'not_applicable') return <span className="muted">—</span>
  return <span className={`badge ${cls}`}>{humanState(state)}</span>
}

/**
 * Completeness is part of every listing. The UI never infers completeness
 * from the absence of rows.
 */
export function CompletenessBanner({ c }: { c: SourceCompleteness }) {
  if (c.state === 'enumerating') {
    return (
      <div className="banner warn">
        <span className="badge warn">partial</span>
        <span>
          <strong>{c.name}</strong> is being enumerated for the first time. Listings and totals are incomplete until
          the scan publishes.
        </span>
      </div>
    )
  }
  if (c.state === 'reconciling') {
    return (
      <div className="banner">
        <span className="badge accent">rescanning</span>
        <span>
          <strong>{c.name}</strong> is being rescanned. Showing the last published generation plus new observations.
        </span>
      </div>
    )
  }
  if (c.state === 'degraded' || c.state === 'stale' || c.state === 'offline') {
    return (
      <div className="banner warn">
        <StateBadge state={c.state} />
        <span>
          <strong>{c.name}</strong>: {c.note ?? 'results may be stale'}. Last complete scan {ago(c.last_scan_completed)}.
        </span>
      </div>
    )
  }
  if (!c.metadata_complete) {
    return (
      <div className="banner bad">
        <span className="badge bad">not scanned</span>
        <span>
          <strong>{c.name}</strong> has no published generation yet. Nothing below is complete.
        </span>
      </div>
    )
  }
  if (integerNumber(c.listing_errors) > 0) {
    return (
      <div className="banner warn">
        <span className="badge warn">complete with exceptions</span>
        <span>
          <strong>{c.name}</strong>: {count(c.listing_errors)} directories could not be listed in the published
          generation; their previous contents were preserved and their totals are marked incomplete.
          {integerNumber(c.content_pending) > 0 ? ` ${count(c.content_pending)} files await content indexing.` : ''}
        </span>
      </div>
    )
  }
  if (integerNumber(c.content_pending) > 0) {
    return (
      <div className="banner">
        <span className="badge ok">metadata complete</span>
        <span>
          <strong>{c.name}</strong>: names, sizes and dates are complete; {count(c.content_pending)} files await
          content indexing.
        </span>
      </div>
    )
  }
  return (
    <div className="banner ok">
      <span className="badge ok">complete</span>
      <span>
        <strong>{c.name}</strong> is fully indexed. Last scan {ago(c.last_scan_completed)}.
      </span>
    </div>
  )
}

/** Per-source content policy switch (extraction on/off). */
export function ContentPolicyControl({
  sourceId,
  enabled,
  concurrency,
}: {
  sourceId: ApiRouteId
  enabled: boolean
  concurrency: number
}) {
  const qc = useQueryClient()
  const m = useMutation({
    mutationFn: (body: { enabled?: boolean; concurrency?: number }) => api.setContentPolicy(sourceId, body),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['source', sourceId] })
      qc.invalidateQueries({ queryKey: ['sources'] })
      qc.invalidateQueries({ queryKey: ['activity'] })
    },
  })
  return (
    <span className="toggle-group" title="Literal-text extraction for this source">
      <button className={`btn small ${enabled ? '' : 'primary'}`} disabled={m.isPending} onClick={() => m.mutate({ enabled: !enabled })}>
        content {enabled ? 'on' : 'off'}
      </button>{' '}
      <label className="muted small">
        ×
        <input
          type="number"
          className="small"
          min={1}
          max={64}
          defaultValue={concurrency}
          key={concurrency}
          disabled={m.isPending}
          onBlur={(e) => {
            const v = Number(e.target.value)
            if (Number.isFinite(v) && v >= 1 && v !== concurrency) m.mutate({ concurrency: v })
          }}
        />
      </label>
    </span>
  )
}

export function Spinner({ label }: { label?: string }) {
  return (
    <div className="empty">
      <div className="progress" style={{ width: 160, margin: '0 auto 8px' }}>
        <div className="bar" />
      </div>
      {label ?? 'Loading…'}
    </div>
  )
}

export function ErrorBox({ error }: { error: unknown }) {
  const msg = error instanceof Error ? error.message : String(error)
  return (
    <div className="banner bad">
      <span className="badge bad">error</span>
      <span>{msg}</span>
    </div>
  )
}
