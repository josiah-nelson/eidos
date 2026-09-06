import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api, type ResourceLimits } from '../api'
import { bytes } from '../format'
import { ErrorBox, Spinner } from '../components'

const same = (a: ResourceLimits, b: ResourceLimits) =>
  a.scan_threads === b.scan_threads &&
  a.concurrent_scans === b.concurrent_scans &&
  a.minimum_free_mib === b.minimum_free_mib

function LimitsEditor({ saved }: { saved: ResourceLimits }) {
  const qc = useQueryClient()
  // Background status polling must never overwrite an operator's draft, but an
  // untouched form has no draft to protect and must not go stale: saving it
  // would silently revert limits another tab or `eidos resources` just set.
  const [draft, setDraft] = useState(saved)
  const [base, setBase] = useState(saved)
  const [conflict, setConflict] = useState(false)
  if (!same(base, saved)) {
    // `same(draft, saved)` is our own save landing, not someone else's change.
    if (same(draft, base) || same(draft, saved)) {
      setDraft(saved)
      setConflict(false)
    } else setConflict(true)
    setBase(saved)
  }
  const adopt = () => { setDraft(saved); setConflict(false) }
  const save = useMutation({
    mutationFn: api.setResources,
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['resources'] })
      qc.invalidateQueries({ queryKey: ['activity'] })
    },
  })
  const field = (key: keyof ResourceLimits, label: string, min: number, max: number) => (
    <label>
      {label}{' '}
      <input type="number" required min={min} max={max} step={1} className="small"
        value={Number.isNaN(draft[key]) ? '' : draft[key]}
        disabled={save.isPending}
        onChange={e => {
          setDraft({ ...draft, [key]: e.target.valueAsNumber })
          save.reset()
        }} />
    </label>
  )
  return (
    // A cleared or unparseable number field is NaN, which would POST `null`.
    <form onSubmit={e => { e.preventDefault(); if (Object.values(draft).every(Number.isFinite)) save.mutate(draft) }}>
      <div className="toolbar" style={{ flexWrap: 'wrap', gap: 16 }}>
        {field('scan_threads', 'Threads per metadata scan', 1, 64)}
        {field('concurrent_scans', 'Concurrent metadata scans', 1, 16)}
        {field('minimum_free_mib', 'Data-volume reserve (MiB)', 0, 4294967295)}
        <button type="submit" disabled={save.isPending}>{save.isPending ? 'Saving…' : 'Save resource limits'}</button>
        <button type="button" disabled={save.isPending} onClick={() => { adopt(); save.reset() }}>Reset to saved</button>
      </div>
      {conflict && <p className="banner warn" role="status">
        These limits were changed elsewhere while you were editing. Saving now replaces that
        configuration with what is in this form.{' '}
        <button type="button" className="small" disabled={save.isPending}
          onClick={() => { adopt(); save.reset() }}>Load the current values</button>
      </p>}
      {save.isError && <ErrorBox error={save.error} />}
      {save.isSuccess && <p role="status">Resource limits saved. New scans use these limits; active work drains normally.</p>}
    </form>
  )
}

export default function ResourceControls() {
  const q = useQuery({ queryKey: ['resources'], queryFn: api.resources, refetchInterval: 2000 })
  return (
    <section aria-labelledby="resources-heading">
      <h2 id="resources-heading">Resource limits</h2>
      {q.isPending ? <Spinner label="Loading resource limits…" /> : q.isError ? <ErrorBox error={q.error} /> : <>
        <p>
          {q.data.active_scans} metadata scan(s) active · data volume free{' '}
          {q.data.free_bytes === null ? 'unknown' : bytes(q.data.free_bytes)}
          {q.data.disk_sample_age_s !== null && ` (sample ${q.data.disk_sample_age_s}s old)`}
        </p>
        {q.data.admission_blocked && <p className="banner bad" role="status">New scans and content reads are waiting: {q.data.admission_blocked}</p>}
        <LimitsEditor saved={q.data.limits} />
        <p className="muted small">
          Saved across restarts. The scan ceiling covers initial probes through publication; active scans keep their starting width.
          The content pool below is separate. Source caps are not shared-device caps.
          Disk pressure holds new work, but does not stop current files, publication, native updates or fleet writes.
          A zero reserve disables the free-space check. These are admission limits, not a hard disk or memory quota.
        </p>
      </>}
    </section>
  )
}
