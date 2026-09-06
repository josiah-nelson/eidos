import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Link } from 'react-router'
import { api } from '../api'
import { ErrorBox, Spinner } from '../components'

function DeviceEditor({ saved }: { saved: number }) {
  const qc = useQueryClient()
  const [draft, setDraft] = useState(saved)
  const [base, setBase] = useState(saved)
  const [conflict, setConflict] = useState(false)
  if (base !== saved) {
    if (draft === base || draft === saved) { setDraft(saved); setConflict(false) }
    else setConflict(true)
    setBase(saved)
  }
  const reset = () => { setDraft(saved); setConflict(false); save.reset() }
  const save = useMutation({
    mutationFn: api.setDeviceLimits,
    onSuccess: data => { qc.setQueryData(['devices'], data); qc.invalidateQueries({ queryKey: ['devices'] }) },
  })
  return <form onSubmit={event => {
    event.preventDefault()
    if (Number.isInteger(draft) && draft >= 1 && draft <= 64) save.mutate({ readers_per_device: draft })
  }}>
    <div className="toolbar" style={{ flexWrap: 'wrap', gap: 16 }}>
      <label>Readers per backing device{' '}
        <input type="number" min={1} max={64} step={1} required value={Number.isNaN(draft) ? '' : draft}
          disabled={save.isPending} onChange={event => { setDraft(event.target.valueAsNumber); save.reset() }} />
      </label>
      <button type="submit" disabled={save.isPending}>{save.isPending ? 'Saving…' : 'Save device limit'}</button>
      <button type="button" disabled={save.isPending} onClick={reset}>Reset device limit</button>
    </div>
    {conflict && <p className="banner warn" role="status">The device limit changed elsewhere. Your draft is preserved; saving replaces the current limit. <button type="button" disabled={save.isPending} onClick={reset}>Load current device limit</button></p>}
    {save.isError && <ErrorBox error={save.error} />}
    {save.isSuccess && <p role="status">Device limit saved. Existing reservations drain normally.</p>}
  </form>
}

export default function DeviceResources() {
  const q = useQuery({ queryKey: ['devices'], queryFn: api.devices, refetchInterval: 2000 })
  const view = q.data
  return <section aria-labelledby="devices-heading">
    <h2 id="devices-heading">Shared-device reader limits</h2>
    {q.isPending ? <Spinner label="Loading device budgets…" /> : q.isError ? <ErrorBox error={q.error} /> : view && <>
      <DeviceEditor saved={view.budget.readers_per_device} />
      <p role="status">Topology sample: {view.sample_age_s === null ? 'pending' : `${view.sample_age_s}s old`}{view.stale && ' · stale or unavailable'}.</p>
      {view.budget.unresolved_shared_fallback && <p className="banner warn">At least one source has unresolved topology. All local sources share one conservative budget until every backing device is known.</p>}
      {view.budget.topology_draining && <p className="banner warn" role="status">Topology changed. New admissions wait for existing reservations to drain before switching device groups.</p>}
      {view.budget.devices.length === 0 ? <p>No local sources registered.</p> : <table>
        <thead><tr><th>Active budget / source roots</th><th>Content readers</th><th>Scan threads</th><th>Peak combined</th><th>Capacity</th></tr></thead>
        <tbody>{view.budget.devices.map(device => <tr key={device.key}>
          <td>{device.key}<ul>{device.sources.map(source => <li key={source}><Link to={`/sources/${source}`}>{view.source_roots[source] ?? `Source ${source}`}</Link></li>)}</ul></td>
          <td>{device.content_readers}</td><td>{device.scan_threads}</td><td>{device.peak_readers}</td>
          <td>{view.budget.topology_draining ? 'Waiting for topology drain' : device.content_readers + device.scan_threads >= view.budget.readers_per_device ? 'At capacity — new readers wait' : 'Available'}</td>
        </tr>)}</tbody>
      </table>}
      {Object.entries(view.source_errors).map(([source, error]) => <p className="banner warn" key={source}><Link to={`/sources/${source}`}>{view.source_roots[source] ?? `Source ${source}`}</Link>: {error}</p>)}
      <p className="muted small">Saved across restarts. Scan enumerators and content readers share this ceiling across source roots; scans use only their granted width.
        Unknown or stale topology uses a shared fallback, not guessed independent disks. OS disk identities may hide shared RAID or virtual storage.
        This does not cap catalog/index/fleet writes, native change feeds, query I/O, or the single background topology probe. It is not an IOPS, bandwidth or memory quota, or a measured performance preset.</p>
    </>}
  </section>
}
