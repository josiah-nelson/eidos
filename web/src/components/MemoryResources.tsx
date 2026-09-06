import { useQuery } from '@tanstack/react-query'
import { api } from '../api'
import { bytes, count } from '../format'
import { memoryPollInterval } from '../memory-polling'
import { ErrorBox, Spinner } from '../components'

export default function MemoryResources() {
  const q = useQuery({
    queryKey: ['memory'],
    queryFn: api.memory,
    refetchInterval: (query) => memoryPollInterval(query.state.data),
  })
  const memory = q.data
  const value = (n: string | null | undefined) => n == null ? 'unavailable' : bytes(n)
  return <section aria-labelledby="memory-heading">
    <h2 id="memory-heading">Memory usage and budgets</h2>
    {/* A failed refetch must not blank diagnostics that already loaded: report
        the request failure above the last successful sample and its budgets. */}
    {q.isError && <ErrorBox error={q.error} />}
    {q.isPending ? <Spinner label="Sampling process memory…" /> : memory && <>
      <p role="status">
        {memory.process ? `Process ${memory.process.pid}` : 'Process memory pending or unavailable'}
        {memory.sample_age_s !== null && ` · sampled ${memory.sample_age_s}s ago`}
        {memory.stale && ' · awaiting a fresh sample'}
        {q.isError && ' · last successful response; the service is not answering'}
      </p>
      {memory.error && <p className="banner warn" role="alert">Memory sample unavailable: {memory.error}</p>}
      <div className="stats">
        <div className="stat"><div className="label">Resident RAM / working set</div><div className="value">{value(memory.process?.resident_bytes)}</div></div>
        <div className="stat"><div className="label">Peak resident RAM</div><div className="value">{value(memory.process?.peak_resident_bytes)}</div></div>
        <div className="stat"><div className="label">Private committed bytes (Windows)</div><div className="value">{value(memory.process?.private_commit_bytes)}</div></div>
      </div>
      <table>
        <thead><tr><th>Configured budget</th><th>Target / limit</th><th>Scope</th></tr></thead>
        <tbody>
          <tr><td>Catalog page caches</td><td>{bytes(memory.catalog.page_cache_baseline_target_bytes)}</td>
            <td>{count(memory.catalog.baseline_connections)} baseline connections × {bytes(memory.catalog.page_cache_per_connection_bytes)} each</td></tr>
          <tr><td>Additional scan connection</td><td>{bytes(memory.catalog.page_cache_per_connection_bytes)}</td><td>Per open scan session, in addition to baseline</td></tr>
          <tr><td>Catalog memory mapping</td><td>{bytes(memory.catalog.mmap_per_connection_limit_bytes)}</td><td>Effective maximum mapped file range per connection; not allocated RAM</td></tr>
          <tr><td>Name-index writer</td><td>{bytes(memory.catalog_writer_budget_bytes)}</td><td>Shared across its indexing threads</td></tr>
          <tr><td>Content-index writer</td><td>{bytes(memory.content_writer_budget_bytes)}</td><td>Shared across its indexing threads</td></tr>
        </tbody>
      </table>
      <p className="muted small">Budgets are not current consumption or a hard process memory limit. Do not add them to resident RAM.
        SQLite allocation tracking remains disabled to avoid its global allocation lock. Shared/file-backed memory, index readers, in-memory temporary tables,
        extraction buffers, policy caches and other runtime allocations are not individually budgeted here.
        Private committed bytes may be resident or paged out. Unsupported counters remain unavailable, not zero.</p>
    </>}
  </section>
}
