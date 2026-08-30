import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  api,
  type FleetStatus,
  type InviteView,
  type LocalSourceSync,
  type PeerView,
  type ReplicaSourceSync,
  type SessionView,
} from '../api'
import { ErrorBox, Spinner } from '../components'
import { ago, bytes, count, duration, integerNumber, when } from '../format'

// Everything the fleet API offers, in one place: this node's role, the
// enrollment handshake (invite on the central, code entry on the node),
// the peer roster, live sessions, and per-source sync state. The service
// answers 503 while the fleet runtime is not running, which the page
// reports as a state rather than an error.
export default function FleetPage() {
  const q = useQuery({ queryKey: ['fleet'], queryFn: api.fleetStatus, refetchInterval: 2000, retry: false })
  if (q.isPending) return <Spinner label="Loading fleet…" />
  if (q.isError)
    return (
      <>
        <div className="toolbar">
          <h1 style={{ margin: 0 }}>Fleet</h1>
        </div>
        <ErrorBox error={q.error} />
        <p className="muted">
          The fleet runtime is not available. It starts with the service when the catalog opens cleanly; check the
          service log if this persists.
        </p>
      </>
    )
  const f = q.data
  const connected = f.peers.filter((p) => p.connected).length
  return (
    <>
      <div className="toolbar">
        <h1 style={{ margin: 0 }}>Fleet</h1>
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
      <div className="stats">
        <div className="stat">
          <div className="label">Role</div>
          <div className="value">{f.central ? 'central' : f.enrolled ? 'enrolled' : 'standalone'}</div>
          <div className="sub">
            {f.central
              ? f.listening
                ? `listening on ${f.listening}`
                : 'no listener bound'
              : f.enrolled
                ? f.sync_enabled
                  ? 'sync enabled'
                  : 'sync paused'
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
          <div className="sub">{count(f.pending_invites)} open invites</div>
        </div>
        <div className="stat">
          <div className="label">Sources shipping</div>
          <div className="value">{f.local_sources.filter((s) => s.enabled).length}</div>
          <div className="sub">{f.replica_sources.length} replicas held here</div>
        </div>
      </div>

      <ThisNode f={f} />

      {f.peers.length > 0 && (
        <>
          <h2>Peers</h2>
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

// Role, listener, enrollment, and invites for the node this UI talks to.
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
    <div className="cards">
      <div className="card">
        <div className="head">
          <div className="grow">
            <div className="name">This node</div>
            <div className="path">
              {f.central
                ? 'accepts enrollments and replicates enrolled sources here'
                : 'a central accepts enrollments; a node ships its sources to one'}
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
            act as central
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
          {f.listening && f.listening !== f.listen && (
            <div className="muted small">bound to {f.listening}</div>
          )}
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
              sync to central
            </label>
            <button
              type="button"
              className="btn small"
              disabled={leave.isPending}
              onClick={() => {
                if (window.confirm('Leave the fleet? The central keeps this node’s replicas; rejoining is a new epoch.'))
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
      {f.central ? <InviteCard /> : !f.enrolled ? <EnrollCard onEnrolled={invalidate} /> : null}
    </div>
  )
}

// Central side of the handshake: mint a one-time code for a new node.
function InviteCard() {
  const [invite, setInvite] = useState<InviteView | null>(null)
  const mint = useMutation({ mutationFn: () => api.fleetInvite(), onSuccess: setInvite })
  return (
    <div className="card">
      <div className="head">
        <div className="grow">
          <div className="name">Invite a node</div>
          <div className="path">one-time code; paste it into the other node’s Fleet page</div>
        </div>
        <button type="button" className="btn small primary" disabled={mint.isPending} onClick={() => mint.mutate()}>
          {mint.isPending ? 'creating…' : 'New invite'}
        </button>
      </div>
      {mint.isError && <div className="error-text">{mint.error.message}</div>}
      {invite && (
        <div className="form">
          <label>
            Code
            <input type="text" readOnly value={invite.code} onFocus={(e) => e.target.select()} />
          </label>
          <div className="muted small">
            endpoint {invite.endpoint} · expires {when(invite.expires_at)}
          </div>
        </div>
      )}
    </div>
  )
}

// Node side of the handshake: redeem a code from a central.
function EnrollCard({ onEnrolled }: { onEnrolled: () => void }) {
  const [code, setCode] = useState('')
  const enroll = useMutation({ mutationFn: () => api.fleetEnroll(code.trim()), onSuccess: onEnrolled })
  return (
    <div className="card">
      <div className="head">
        <div className="grow">
          <div className="name">Enroll with a central</div>
          <div className="path">paste an invite code minted on the central’s Fleet page</div>
        </div>
      </div>
      <form
        className="form"
        onSubmit={(e) => {
          e.preventDefault()
          enroll.mutate()
        }}
      >
        <label>
          Invite code
          <input type="text" value={code} onChange={(e) => setCode(e.target.value)} placeholder="eidos-…" />
        </label>
        {enroll.isError && <div className="error-text">{enroll.error.message}</div>}
        {enroll.data && (
          <div className="muted small">
            enrolled with {enroll.data.central_name} at {enroll.data.endpoint}
          </div>
        )}
        <div className="actions">
          <button type="submit" className="btn primary" disabled={enroll.isPending || !code.trim()}>
            {enroll.isPending ? 'enrolling…' : 'Enroll'}
          </button>
        </div>
      </form>
    </div>
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
