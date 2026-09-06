import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Link } from 'react-router'
import { api, type AddedSource, type ApiInt } from '../api'
import { ErrorBox, Spinner } from '../components'
import { bytes } from '../format'
import { JoinCard } from './FleetPage'

type RootResult = { root: string; added?: AddedSource; error?: string; pending?: boolean }
type Role = 'standalone' | 'master' | 'join'

// This form belongs to the operator, not to polling results. SourcesPage keeps
// it mounted until Done, including when the first of several roots succeeds.
export default function Onboarding({ onDone, onManual }: { onDone: () => void; onManual: () => void }) {
  const qc = useQueryClient()
  const fleet = useQuery({ queryKey: ['fleet'], queryFn: api.fleetStatus, refetchInterval: 2000, retry: false })
  const volumes = useQuery({ queryKey: ['volumes'], queryFn: api.volumes })
  const [role, setRole] = useState<Role | null>(null)
  const [choosingRoots, setChoosingRoots] = useState(false)
  const [picked, setPicked] = useState<Record<string, boolean>>({})
  const [results, setResults] = useState<RootResult[]>([])
  const [busy, setBusy] = useState(false)
  const [scanNow, setScanNow] = useState(true)
  const roleChange = useMutation({
    mutationFn: () => api.setFleetCentral({ central: role === 'master', ...(role === 'standalone' ? { listen: '' } : {}) }),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ['fleet'] })
      setChoosingRoots(true)
    },
  })
  const current = fleet.data
  const selected = (volumes.data ?? []).filter(v => picked[v.root] && !v.already_indexed && !results.some(r => r.root === v.root && r.added))
  const failed = results.filter(r => r.error && !r.added).map(r => r.root)
  async function addRoots(roots: string[]) {
    setBusy(true)
    // Sequential requests bound setup load. Every outcome is retained even if
    // the next drive fails; only failed creations are submitted again.
    try {
      for (const root of roots) {
        setResults(prev => [...prev.filter(r => r.root !== root), { root, pending: true }])
        let result: RootResult
        try {
          const added = await api.addSource({ name: root.replace(/[\\/]+$/, '') || root, root_path: root, scan: scanNow })
          result = { root, added }
        } catch (error) {
          result = { root, error: error instanceof Error ? error.message : String(error) }
        }
        setResults(prev => prev.map(r => r.root === root ? result : r))
        void qc.invalidateQueries({ queryKey: ['sources'] })
      }
    } finally {
      setBusy(false)
      void qc.invalidateQueries({ queryKey: ['volumes'] })
    }
  }
  const roleLocked = Boolean(current?.enrolled || current?.pending_join || current?.central)
  return (
    <section className="cards onboarding" aria-label="Getting started">
      <div className="card">
        <h2>Set up this node</h2>
        <p>Choose its role, then choose what to index. No drives are selected automatically.</p>
        {!choosingRoots ? (
          <>
            {fleet.isPending && <Spinner label="Reading node status…" />}
            {fleet.isError && <ErrorBox error={fleet.error} />}
            {roleLocked ? (
              <p>
                This node is {current?.central ? 'a master' : current?.enrolled ? 'joined to a master' : 'waiting for a master'}.
                {' '}Manage or change that role on <Link to="/nodes">Nodes</Link>.
              </p>
            ) : (
              <fieldset disabled={roleChange.isPending}>
                <legend>Node role</legend>
                <label className="toggle"><input type="radio" name="role" checked={role === 'standalone'} onChange={() => setRole('standalone')} /> Standalone — search this machine</label>
                <label className="toggle"><input type="radio" name="role" checked={role === 'master'} onChange={() => setRole('master')} /> Master — approve nodes and search their replicas</label>
                <label className="toggle"><input type="radio" name="role" checked={role === 'join'} onChange={() => setRole('join')} /> Join an existing master</label>
              </fieldset>
            )}
            {(role === 'join' || current?.pending_join) && current && !current.central && !current.enrolled && <JoinCard f={current} />}
            {current?.pending_join?.rejected_reason && <p className="error-text">Clear the rejected request above before choosing another master.</p>}
            {roleChange.isError && <ErrorBox error={roleChange.error} />}
            {role === 'master' && <p className="muted">Enables the sync listener and advertises this master on the local network.</p>}
            <button
              className="btn primary"
              disabled={roleChange.isPending || (!roleLocked && (!role || (role !== 'standalone' && !current) || role === 'join')) || Boolean(current?.pending_join?.rejected_reason)}
              onClick={() => {
                if (roleLocked) setChoosingRoots(true)
                else roleChange.mutate()
              }}
            >Choose sources</button>
          </>
        ) : (
          <>
            <h3>Choose drives</h3>
            <p className="muted">Metadata becomes searchable first; file content is processed in the background. Start with a deliberately chosen root.</p>
            <button type="button" className="small" disabled={busy || volumes.isFetching}
              onClick={() => { void qc.invalidateQueries({ queryKey: ['volumes'] }) }}
            >{volumes.isFetching ? 'Rescanning drives…' : 'Rescan drives'}</button>
            {volumes.isPending && <Spinner label="Reading local drives…" />}
            {volumes.isError && <ErrorBox error={volumes.error} />}
            {!volumes.isPending && !volumes.data?.length && <p>No drives listed. Add a folder or network share manually.</p>}
            {volumes.data?.map(v => {
              const added = results.some(r => r.root === v.root && r.added)
              return (
                <label className="toggle" key={v.root}>
                  <input type="checkbox" aria-label={'Index ' + v.root}
                    checked={v.already_indexed || added || Boolean(picked[v.root])}
                    disabled={busy || v.already_indexed || added}
                    onChange={e => setPicked(prev => ({ ...prev, [v.root]: e.target.checked }))} />
                  <span className="mono">{v.root}</span> {v.volume_name} · {v.filesystem} · {v.drive_type}
                  {v.total_bytes !== '0' && <span> · {bytes(v.free_bytes)} free / {bytes(v.total_bytes)}</span>}
                  {v.already_indexed || added ? ' · already added' : ''}
                </label>
              )
            })}
            <label className="toggle">
              <input type="checkbox" checked={scanNow} disabled={busy} onChange={e => setScanNow(e.target.checked)} />
              Start metadata scans now (content follows in the background)
            </label>
            <div className="actions">
              <button className="btn primary" disabled={busy || !selected.length} onClick={() => void addRoots(selected.map(v => v.root))}>
                {busy ? 'Adding sources…' : 'Add selected drives'}
              </button>
              <button className="btn" disabled={busy} onClick={onManual}>Add a folder manually</button>
              {failed.length > 0 && <button className="btn" disabled={busy} onClick={() => void addRoots(failed)}>Retry failed drives</button>}
            </div>
            <div aria-live="polite">
              {results.map(r => (
                <div className="card" key={r.root}>
                  <strong>{r.root}</strong>
                  {r.pending && <span> · adding…</span>}
                  {r.error && <p className="error-text">{r.error}</p>}
                  {r.added && <CreatedSourceResult added={r.added} />}
                </div>
              ))}
            </div>
          </>
        )}
        <div className="actions" style={{ marginTop: 12 }}>
          <button className="btn" disabled={busy || roleChange.isPending} onClick={onDone}>{choosingRoots ? 'Done' : 'Set up later'}</button>
        </div>
      </div>
    </section>
  )
}

// A scan retry always addresses the returned ID, never POST /sources again.
export function CreatedSourceResult({ added }: { added: AddedSource }) {
  const qc = useQueryClient()
  const retry = useMutation({
    mutationFn: (id: ApiInt) => api.scanSource(id),
    onSettled: () => qc.invalidateQueries({ queryKey: ['sources'] }),
  })
  return (
    <div>
      <p>Source added. <Link to={'/sources/' + added.source.id}>View source and progress</Link></p>
      {added.warning && <p className="error-text">{added.warning}</p>}
      {added.scan_error && !retry.isSuccess && (
        <>
          <p className="error-text">Scan did not start: {added.scan_error}</p>
          <button className="btn" disabled={retry.isPending} onClick={() => retry.mutate(added.source.id)}>Retry scan</button>
        </>
      )}
      {retry.isSuccess && <p>Scan started.</p>}
      {retry.isError && <ErrorBox error={retry.error} />}
    </div>
  )
}
