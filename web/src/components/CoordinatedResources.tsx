import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api, type ResourceSettings, type ResourceSettingsView } from '../api'
import { ErrorBox, Spinner } from '../components'

const same = (a: ResourceSettings, b: ResourceSettings) =>
  a.content_workers === b.content_workers &&
  a.scan_threads === b.scan_threads &&
  a.concurrent_scans === b.concurrent_scans &&
  a.minimum_free_mib === b.minimum_free_mib &&
  a.readers_per_device === b.readers_per_device

const componentName = (component: string | null | undefined) => {
  switch (component) {
    case 'content_workers': return 'content worker limit'
    case 'metadata_limits': return 'metadata limits'
    case 'device_readers': return 'device reader limit'
    case 'operation_journal': return 'operation journal'
    case 'journal_cleanup': return 'operation journal cleanup'
    default: return 'remaining settings'
  }
}

function PendingOperation({ view, repair }: {
  view: ResourceSettingsView
  repair: { mutate: () => void, isPending: boolean, isError: boolean, error: Error | null }
}) {
  const pending = view.pending
  if (!pending) return null
  return <div className="banner bad" role="alert">
    <strong>Coordinated save needs repair.</strong>{' '}
    {pending.cleanup_pending
      ? 'All target settings are live, but the completed operation journal still needs cleanup.'
      : <>Saved {pending.completed_components.length} component(s); next is {componentName(pending.next_component)}.</>}
    {(pending.error ?? view.error) && <div className="mono small">{pending.error ?? view.error}</div>}
    <button type="button" disabled={repair.isPending} onClick={() => repair.mutate()}>
      {repair.isPending ? 'Repairing…' : 'Repair coordinated save'}
    </button>
    {repair.isError && <ErrorBox error={repair.error} />}
  </div>
}

function SettingsEditor({ view }: { view: ResourceSettingsView }) {
  const qc = useQueryClient()
  const saved = view.current
  const [draft, setDraft] = useState(saved)
  const [base, setBase] = useState(saved)
  const [conflict, setConflict] = useState(false)
  if (!same(base, saved)) {
    if (same(draft, base) || same(draft, saved)) {
      setDraft(saved)
      setConflict(false)
    } else setConflict(true)
    setBase(saved)
  }
  const refresh = (data: ResourceSettingsView) => {
    qc.setQueryData(['resource-settings'], data)
    qc.invalidateQueries({ queryKey: ['resource-settings'] })
    qc.invalidateQueries({ queryKey: ['resources'] })
    qc.invalidateQueries({ queryKey: ['devices'] })
    qc.invalidateQueries({ queryKey: ['activity'] })
  }
  const save = useMutation({ mutationFn: api.applyResourceSettings, onSuccess: refresh })
  const repair = useMutation({ mutationFn: api.repairResourceSettings, onSuccess: refresh })
  const pending = view.pending !== null
  const reset = () => { setDraft(saved); setConflict(false); save.reset() }
  const field = (key: keyof ResourceSettings, label: string, min: number, max: number) => <label>
    {label}{' '}
    <input type="number" min={min} max={max} step={1} required className="small"
      value={Number.isNaN(draft[key]) ? '' : draft[key]}
      disabled={save.isPending || pending}
      onChange={event => { setDraft({ ...draft, [key]: event.target.valueAsNumber }); save.reset() }} />
  </label>
  return <>
    <PendingOperation view={view} repair={repair} />
    {!pending && view.error && <p className="banner bad" role="alert">
      Last coordinated save failed before any component changed: {view.error}
    </p>}
    <form onSubmit={event => {
      event.preventDefault()
      if (Object.values(draft).every(Number.isInteger)) save.mutate(draft)
    }}>
      <div className="toolbar" style={{ flexWrap: 'wrap', gap: 16 }}>
        {field('content_workers', 'Content workers', 1, 64)}
        {field('scan_threads', 'Threads per metadata scan', 1, 64)}
        {field('concurrent_scans', 'Concurrent metadata scans', 1, 16)}
        {field('minimum_free_mib', 'Data-volume reserve (MiB)', 0, 4294967295)}
        {field('readers_per_device', 'Readers per backing device', 1, 64)}
        <button type="submit" disabled={save.isPending || pending}>
          {save.isPending ? 'Applying…' : 'Apply coordinated settings'}
        </button>
        <button type="button" disabled={save.isPending || pending} onClick={reset}>Reset to current</button>
      </div>
      {conflict && <p className="banner warn" role="status">
        The live resource tuple changed while you were editing. Your draft is preserved.{' '}
        <button type="button" disabled={save.isPending || pending} onClick={reset}>Load the current tuple</button>
      </p>}
      {save.isError && <ErrorBox error={save.error} />}
      {save.data?.outcome === 'applied' && !save.data.pending &&
        <p role="status">Coordinated settings applied and saved across restarts. Existing work drains normally.</p>}
      {save.data?.outcome === 'failed' && <p className="banner bad" role="alert">{save.data.error}</p>}
    </form>
  </>
}

export default function CoordinatedResources() {
  const query = useQuery({
    queryKey: ['resource-settings'], queryFn: api.resourceSettings, refetchInterval: 2000,
  })
  // A failed background poll must not unmount the editor: React Query keeps
  // the last view, and remounting would silently discard an unsaved draft.
  const view = query.data
  return <section aria-labelledby="coordinated-resources-heading">
    <h2 id="coordinated-resources-heading">Coordinated resource settings</h2>
    {query.isError && <ErrorBox error={query.error} />}
    {view === undefined
      ? query.isError ? null : <Spinner label="Loading coordinated resource settings…" />
      : <>
        <p>Apply the complete custom tuple as one recoverable operation. No measured preset is selected.</p>
        <SettingsEditor view={view} />
        <p className="muted small">
          The service records the target before changing any component. A partial filesystem failure stays visible and can be repaired after the underlying problem is corrected.
          Lower ceilings stop new admissions and let active scans, reservations, and files drain; they do not revoke work already running.
          Per-source reader caps remain independent and are never changed here. The individual controls below remain available when no coordinated repair is pending.
        </p>
      </>}
  </section>
}
