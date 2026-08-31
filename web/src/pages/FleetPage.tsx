import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  api,
  ApiError,
  type DiscoveredMaster,
  type FleetStatus,
  type JoinRequestView,
  type LocalSourceSync,
  type PeerView,
  type ReplicaSourceSync,
  type SessionView,
} from '../api'
import { ErrorBox, Spinner } from '../components'
import { ago, bytes, count, duration, integerNumber, when } from '../format'

// Everything the fleet API offers, in one place: this node's role, local
// master discovery and approval-based joining, the peer roster, live
// sessions, and per-source sync state. The service
// answers 503 while the fleet runtime is not running, which the page
// reports as a state rather than an error.
export default function FleetPage() {
  const q = useQuery({ queryKey: ['fleet'], queryFn: api.fleetStatus, refetchInterval: 2000, retry: false })
  if (q.isPending) return <Spinner label="Loading nodes…" />
  if (q.isError)
    return (
      <>
        <div className="toolbar">
          <h1 style={{ margin: 0 }}>Nodes</h1>
        </div>
        <ErrorBox error={q.error} />
        {q.error instanceof ApiError && q.error.status === 503 && q.error.kind === 'unavailable' && (
          <p className="muted">
            The fleet runtime is not available. It starts with the service when the catalog opens cleanly; check the
            service log if this persists.
          </p>
        )}
      </>
    )
  const f = q.data
  const connected = f.peers.filter((p) => p.connected).length
  return (
    <>
      <div className="toolbar">
        <h1 style={{ margin: 0 }}>Nodes</h1>
        <div className="spacer" style={{ flex: 1 }} />
        <span className="muted small" title={`node id ${f.node_id}`}>
          {f.name} · {f.fingerprint}
        </span>
      </div>
      {f.degraded.length > 0 && (
        <div className="empty" style={{ marginBottom: 12 }}>
          {f.degraded.map((d) => (
            <div key={d}>
              <span className="badge warn">degraded</span> {d}
            </div>
          ))}
        </div>
      )}
      {f.central && f.join_requests.length > 0 && <JoinRequests requests={f.join_requests} />}
      <div className="stats">
        <div className="stat">
          <div className="label">Role</div>
          <div className="value">{f.central ? 'master' : f.enrolled ? 'joined' : 'standalone'}</div>
          <div className="sub">
            {f.central
              ? f.listening
                ? `listening on ${f.listening}`
                : 'no listener bound'
              : f.enrolled
                ? f.sync_enabled
                  ? 'sync enabled'
                  : 'sync paused'
                : f.pending_join
                  ? f.pending_join.rejected_reason
                    ? 'join rejected'
                    : 'waiting for approval'
                  : 'not part of a fleet yet'}
          </div>
        </div>
        <div className="stat">
          <div className="label">Peers</div>
          <div className="value">{f.peers.length}</div>
          <div className="sub">{connected} connected</div>
        </div>
        <div className="stat">
          <div className="label">Sessions</div>
          <div className="value">{f.sessions.length}</div>
          <div className="sub">{f.join_requests.length} awaiting approval</div>
        </div>
        <div className="stat">
          <div className="label">Sources shipping</div>
          <div className="value">{f.local_sources.filter((s) => s.enabled).length}</div>
          <div className="sub">{f.replica_sources.length} replicas held here</div>
        </div>
      </div>

      <div className="cards">
        {/* Only the listener controls remount when the authoritative value
            changes. Join selection and request feedback live outside this
            keyed boundary and survive an unrelated listener refresh. */}
        <ThisNode key={f.listen ?? ''} f={f} />
        {f.central ? <MasterDiscovery f={f} /> : !f.enrolled ? <JoinCard f={f} /> : null}
      </div>

      {f.peers.length > 0 && (
        <>
          <h2>Fleet members</h2>
          <table className="grid">
            <thead>
              <tr>
                <th>Peer</th>
                <th>Role</th>
                <th>Endpoint</th>
                <th>Sync</th>
                <th>Status</th>
                <th>Last seen</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {f.peers.map((p) => (
                <PeerRow key={p.node_id} p={p} />
              ))}
            </tbody>
          </table>
        </>
      )}

      {f.sessions.length > 0 && (
        <>
          <h2>Live sessions</h2>
          <table className="grid">
            <thead>
              <tr>
                <th>Peer</th>
                <th>Direction</th>
                <th>Since</th>
                <th className="num">Activity</th>
                <th className="num">Credit</th>
                <th>Sources</th>
              </tr>
            </thead>
            <tbody>
              {f.sessions.map((s) => (
                <SessionRow key={`${s.peer}-${s.since}`} s={s} />
              ))}
            </tbody>
          </table>
        </>
      )}

      {f.local_sources.length > 0 && (
        <>
          <h2>Local sources</h2>
          <table className="grid">
            <thead>
              <tr>
                <th>Source</th>
                <th>Policy</th>
                <th>State</th>
                <th className="num">Head</th>
                <th className="num">Compacted</th>
                <th className="num">Backlog</th>
              </tr>
            </thead>
            <tbody>
              {f.local_sources.map((s) => (
                <LocalSourceRow key={s.source_id} s={s} />
              ))}
            </tbody>
          </table>
        </>
      )}

      {f.replica_sources.length > 0 && (
        <>
          <h2>Replicas held here</h2>
          <table className="grid">
            <thead>
              <tr>
                <th>Source</th>
                <th>Node</th>
                <th className="num">Applied</th>
                <th className="num">Reported head</th>
                <th>Freshness</th>
                <th>State</th>
              </tr>
            </thead>
            <tbody>
              {f.replica_sources.map((r) => (
                <ReplicaRow key={r.source_id} r={r} />
              ))}
            </tbody>
          </table>
        </>
      )}
    </>
  )
}

function JoinRequests({ requests }: { requests: JoinRequestView[] }) {
  return (
    <div className="empty" style={{ marginBottom: 12, borderColor: 'var(--warn, #c98b22)' }}>
      <div className="head" style={{ marginBottom: 8 }}>
        <div className="grow">
          <div className="name">
            {requests.length} node{requests.length === 1 ? '' : 's'} waiting for approval
          </div>
          <div className="path">Confirm only machines you recognize. Unapproved nodes cannot sync any data.</div>
        </div>
        <span className="badge warn">action required</span>
      </div>
      {requests.map((request) => (
        <JoinRequestRow key={request.request_id} request={request} />
      ))}
    </div>
  )
}

function JoinRequestRow({ request }: { request: JoinRequestView }) {
  const qc = useQueryClient()
  const decide = useMutation({
    mutationFn: (approve: boolean) => api.decideFleetJoin(request.request_id, approve),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['fleet'] }),
  })
  return (
    <div className="card" style={{ marginTop: 8 }}>
      <div className="head">
        <div className="grow">
          <div className="name">{request.name}</div>
          <div className="path">
            {request.platform} · {request.remote_addr ?? 'network address unavailable'} · requested {ago(request.requested_at)}
          </div>
          <div className="muted small" title={request.fingerprint}>
            node {request.node_id} · certificate {request.fingerprint}
          </div>
        </div>
        <div className="actions">
          <button type="button" className="btn small" disabled={decide.isPending} onClick={() => decide.mutate(false)}>
            Reject
          </button>
          <button
            type="button"
            className="btn small primary"
            disabled={decide.isPending}
            onClick={() => decide.mutate(true)}
          >
            {decide.isPending ? 'confirming…' : 'Approve & join'}
          </button>
        </div>
      </div>
      {decide.isError && <div className="error-text">{decide.error.message}</div>}
    </div>
  )
}

// Role and listener controls for the node this UI talks to. This component is
// keyed by the server's listener value so its draft cannot overwrite a newer
// CLI or browser update; unrelated join state lives outside this boundary.
function ThisNode({ f }: { f: FleetStatus }) {
  const qc = useQueryClient()
  const invalidate = () => qc.invalidateQueries({ queryKey: ['fleet'] })
  const [listen, setListen] = useState(f.listen ?? '')
  const central = useMutation({
    mutationFn: (body: { central?: boolean; listen?: string }) => api.setFleetCentral(body),
    onSuccess: invalidate,
  })
  const sync = useMutation({ mutationFn: (enabled: boolean) => api.setFleetSync(enabled), onSuccess: invalidate })
  const leave = useMutation({ mutationFn: () => api.fleetLeave(), onSuccess: invalidate })
  return (
    <div className="card">
      <div className="head">
        <div className="grow">
          <div className="name">This node</div>
          <div className="path">
            {f.central
              ? 'advertises on the local network, approves nodes, and holds their replicas'
              : 'join a master to make this node’s sources available across the fleet'}
          </div>
        </div>
      </div>
      <div className="form">
        <label className="toggle">
          <input
            type="checkbox"
            checked={f.central}
            disabled={central.isPending}
            onChange={(e) => central.mutate({ central: e.target.checked })}
          />{' '}
          act as fleet master
        </label>
        <label>
          Sync listener (host:port; empty stops listening)
          <span style={{ display: 'flex', gap: 6 }}>
            <input
              type="text"
              value={listen}
              placeholder="0.0.0.0:7710"
              onChange={(e) => setListen(e.target.value)}
            />
            <button
              type="button"
              className="btn small"
              disabled={central.isPending || listen === (f.listen ?? '')}
              onClick={() => central.mutate({ listen })}
            >
              Apply
            </button>
          </span>
        </label>
        {f.listening && f.listening !== f.listen && <div className="muted small">bound to {f.listening}</div>}
        {central.isError && <div className="error-text">{central.error.message}</div>}
      </div>
      {f.enrolled && (
        <div className="actions" style={{ marginTop: 8 }}>
          <label className="toggle">
            <input
              type="checkbox"
              checked={f.sync_enabled}
              disabled={sync.isPending}
              onChange={(e) => sync.mutate(e.target.checked)}
            />{' '}
            sync to master
          </label>
          <button
            type="button"
            className="btn small"
            disabled={leave.isPending}
            onClick={() => {
              if (window.confirm('Leave the fleet? The master keeps this node’s replicas; joining again is a new epoch.'))
                leave.mutate()
            }}
          >
            Leave fleet
          </button>
          {sync.isError && <div className="error-text">{sync.error.message}</div>}
          {leave.isError && <div className="error-text">{leave.error.message}</div>}
        </div>
      )}
    </div>
  )
}

function MasterDiscovery({ f }: { f: FleetStatus }) {
  return (
    <div className="card">
      <div className="head">
        <div className="grow">
          <div className="name">Master discovery</div>
          <div className="path">
            {f.listening
              ? `advertising ${f.name} on the local network; nodes may also enter this host’s IP with port ${f.listening.split(':').at(-1)}`
              : 'set a sync listener to advertise this master and accept join requests'}
          </div>
        </div>
        {f.listening ? <span className="badge ok">advertising</span> : <span className="badge warn">offline</span>}
      </div>
      {f.discovery_error && <div className="error-text">{f.discovery_error}</div>}
    </div>
  )
}

function JoinCard({ f }: { f: FleetStatus }) {
  const qc = useQueryClient()
  const onChanged = () => qc.invalidateQueries({ queryKey: ['fleet'] })
  const firstDiscovered = f.discovered_masters[0]
  const [master, setMaster] = useState(firstDiscovered?.endpoints[0] ?? '')
  const join = useMutation({ mutationFn: () => api.requestFleetJoin(master.trim()), onSuccess: onChanged })
  const cancel = useMutation({ mutationFn: api.cancelFleetJoin, onSuccess: onChanged })
  if (f.pending_join) {
    const pending = f.pending_join
    return (
      <div className="card">
        <div className="head">
          <div className="grow">
            <div className="name">{pending.rejected_reason ? 'Join rejected' : 'Waiting for master approval'}</div>
            <div className="path">
              {pending.master_name} at {pending.endpoint} · requested {ago(pending.requested_at)}
            </div>
            <div className="muted small" title={pending.master_fingerprint}>
              master certificate {pending.master_fingerprint}
            </div>
          </div>
          <span className={`badge ${pending.rejected_reason ? 'bad' : 'info'}`}>
            {pending.rejected_reason ? 'rejected' : 'pending'}
          </span>
        </div>
        {pending.rejected_reason && <div className="error-text">{pending.rejected_reason}</div>}
        <div className="actions" style={{ marginTop: 8 }}>
          <button type="button" className="btn small" disabled={cancel.isPending} onClick={() => cancel.mutate()}>
            {pending.rejected_reason ? 'Clear & try again' : 'Cancel request'}
          </button>
        </div>
        {cancel.isError && <div className="error-text">{cancel.error.message}</div>}
      </div>
    )
  }
  return (
    <div className="card">
      <div className="head">
        <div className="grow">
          <div className="name">Join a fleet</div>
          <div className="path">Choose a discovered master or enter its IP address. The master must approve this node.</div>
        </div>
      </div>
      {f.discovered_masters.length > 0 && (
        <div style={{ marginBottom: 10 }}>
          <div className="muted small" style={{ marginBottom: 6 }}>Found on this network</div>
          {f.discovered_masters.map((found) => (
            <DiscoveredMasterButton key={found.node_id} master={found} onSelect={setMaster} selected={master} />
          ))}
        </div>
      )}
      <form
        className="form"
        onSubmit={(e) => {
          e.preventDefault()
          join.mutate()
        }}
      >
        <label>
          Master IP or host
          <input type="text" value={master} onChange={(e) => setMaster(e.target.value)} placeholder="192.168.1.20" />
        </label>
        {join.isError && <div className="error-text">{join.error.message}</div>}
        <div className="actions">
          <button type="submit" className="btn primary" disabled={join.isPending || !master.trim()}>
            {join.isPending ? 'contacting master…' : 'Request to join'}
          </button>
        </div>
      </form>
    </div>
  )
}

function DiscoveredMasterButton({
  master,
  selected,
  onSelect,
}: {
  master: DiscoveredMaster
  selected: string
  onSelect: (endpoint: string) => void
}) {
  const endpoint = master.endpoints[0]
  return (
    <button
      type="button"
      className={`btn small${selected === endpoint ? ' primary' : ''}`}
      style={{ marginRight: 6, marginBottom: 6 }}
      title={`${master.node_id}\n${master.fingerprint}`}
      onClick={() => onSelect(endpoint)}
    >
      {master.name} · {endpoint}
    </button>
  )
}

function PeerRow({ p }: { p: PeerView }) {
  const qc = useQueryClient()
  const invalidate = () => qc.invalidateQueries({ queryKey: ['fleet'] })
  const update = useMutation({
    mutationFn: (body: { endpoint?: string; enabled?: boolean }) => api.updateFleetPeer(p.node_id, body),
    onSuccess: invalidate,
  })
  const forget = useMutation({ mutationFn: () => api.forgetFleetPeer(p.node_id), onSuccess: invalidate })
  return (
    <tr>
      <td title={`${p.node_id}\n${p.fingerprint}`}>{p.name}</td>
      <td>{p.role}</td>
      <td>
        <input
          type="text"
          className="small"
          defaultValue={p.endpoint ?? ''}
          key={p.endpoint ?? ''}
          placeholder="host:port"
          disabled={update.isPending}
          onBlur={(e) => {
            if (e.target.value !== (p.endpoint ?? '')) update.mutate({ endpoint: e.target.value })
          }}
        />
      </td>
      <td>
        <label className="toggle">
          <input
            type="checkbox"
            checked={p.enabled}
            disabled={update.isPending}
            onChange={(e) => update.mutate({ enabled: e.target.checked })}
          />{' '}
          {p.enabled ? 'on' : 'off'}
        </label>
      </td>
      <td>
        {p.connected ? (
          <span className="badge ok">connected</span>
        ) : (
          <span className="badge warn">offline</span>
        )}
        {p.next_dial_in_ms != null && !p.connected && (
          <span className="muted small"> retry in {duration(p.next_dial_in_ms)}</span>
        )}
        {p.last_error && <div className="muted small">{p.last_error}</div>}
        {update.isError && <div className="error-text">{update.error.message}</div>}
        {forget.isError && <div className="error-text">{forget.error.message}</div>}
      </td>
      <td>{p.last_seen_at ? ago(p.last_seen_at) : '—'}</td>
      <td>
        <button
          type="button"
          className="btn small"
          disabled={forget.isPending}
          onClick={() => {
            if (
              window.confirm(
                `Forget ${p.name}? Its replicated sources are retired and its credential no longer admits it.`,
              )
            )
              forget.mutate()
          }}
        >
          Forget
        </button>
      </td>
    </tr>
  )
}

function SessionRow({ s }: { s: SessionView }) {
  return (
    <tr>
      <td title={s.peer}>{s.peer_name}</td>
      <td>
        {s.direction}
        {s.remote_addr ? <span className="muted small"> {s.remote_addr}</span> : null}
      </td>
      <td>{when(s.since)}</td>
      <td className="num">{duration(integerNumber(s.last_activity_ms_ago))} ago</td>
      <td className="num">{bytes(integerNumber(s.credit_remaining))}</td>
      <td>
        {s.sources.length === 0 && <span className="muted">idle</span>}
        {s.sources.map((src) => (
          <div key={`${src.source_id}-${src.role}`} className="muted small">
            #{src.source_id} {src.role}: {src.phase} · {count(src.cursor)} / {count(src.head)} ·{' '}
            {count(src.rows)} rows
            {src.last_error ? ` · ${src.last_error}` : ''}
          </div>
        ))}
      </td>
    </tr>
  )
}

function LocalSourceRow({ s }: { s: LocalSourceSync }) {
  const backlog = integerNumber(s.backlog_rows) + integerNumber(s.backlog_tombstones)
  return (
    <tr>
      <td>{s.name}</td>
      <td>{s.policy}</td>
      <td>
        {s.degraded ? (
          <span className="badge warn">degraded</span>
        ) : s.enabled && s.ready ? (
          <span className="badge ok">shipping</span>
        ) : s.enabled ? (
          <span className="badge info">preparing</span>
        ) : (
          <span className="badge">off</span>
        )}
      </td>
      <td className="num">{count(s.head_seq)}</td>
      <td className="num">{count(s.compacted_through)}</td>
      <td className="num">
        {count(backlog)}
        {s.backlog_oldest_age_ms != null && backlog > 0 && (
          <span className="muted small"> · oldest {duration(s.backlog_oldest_age_ms)}</span>
        )}
      </td>
    </tr>
  )
}

function ReplicaRow({ r }: { r: ReplicaSourceSync }) {
  return (
    <tr>
      <td>{r.name}</td>
      <td title={r.node}>{r.node_name}</td>
      <td className="num">{count(r.applied_seq)}</td>
      <td className="num">{count(r.reported_head)}</td>
      <td>{r.applied_at ? `applied ${ago(r.applied_at)}` : '—'}</td>
      <td>
        {r.resyncing ? (
          <span className="badge warn">resyncing</span>
        ) : r.connected ? (
          <span className="badge ok">connected</span>
        ) : (
          <span className="badge">offline</span>
        )}
      </td>
    </tr>
  )
}
