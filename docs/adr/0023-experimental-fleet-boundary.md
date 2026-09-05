# ADR-0023: The experimental v0.5 fleet boundary

Status: accepted
Date: 2026-08-29
Amended: 2026-08-31 (operator-approved joining and LAN discovery replace invitation codes)
Milestone: v0.5 dogfood-fleet sprint (tracks A-C)

## Context

The synchronization protocol ([ADR-0013](0013-sync-core-under-deterministic-simulation.md))
and the catalog's source ledger ([ADR-0015](0015-source-sync-ledger-in-the-catalog.md))
were built and verified without a socket. The
[v0.5 dogfood-fleet sprint](../v0.5-dogfood-fleet-sprint.md) puts them
behind real catalog, storage, identity, and transport adapters so a small,
explicitly joined private fleet can test the architecture before the v1
control plane exists. This record fixes the contract those adapters
implement: what a node is, how peers authenticate, where sync lives, how
concurrent connections are resolved, how versions negotiate, and what
joining, pausing, leaving, and retirement mean. Everything here is
**experimental**: the wire protocol is stable only within its explicit
version negotiation. Relay and multi-master topologies remain non-goals.

## Decisions

### 1. Topology and roles

- The smallest valid installation is standalone. A standalone does no fleet
  work: no ledger, no backlog, no listener, no transfer.
- **Central** is the same installation with `central: true` in
  `fleet/config.json`. It indexes and searches its own local sources
  alongside replicated ones and never becomes a prerequisite for a node's
  local readiness, publication, content work, or shutdown.
- A **node** is an installation joined to exactly one master. All of
  its eligible local sources (published, not retired, `sync_policy =
  inherit`) replicate; `local_only` is the per-source exclusion; newly added
  eligible sources inherit the joined default.
- One central, no failover, no peer-to-peer search, no content transfer
  (see [ADR-0024](0024-content-transfer-bakeoff.md)).

### 2. History chain in the ledger

The ledger extends a per-source history chain on every touch:
`chain(0) = 0`, `chain(n) = blake3(chain(n-1) || object || generation ||
live-or-tombstone)`. Chains are retained from `compacted_through` to the
head and travel in every batch (`after_chain`, `through_chain`) and offer
(`head_chain`). Images are materialized at ship time and are not part of
the chain: two incarnations that minted the same touches in the same order
are interchangeable, and a source restored to an older state diverges at the
first differing touch. This is exactly the fencing the simulated protocol
already required ([ADR-0016](0016-generated-universes-and-history-chain-fencing.md)).

### 3. Central replica in ordinary catalog tables

A replicated source is an ordinary `sources` row of kind `remote`, named
`<node>/<source>`, owned by a `hosts` row for the node. Its `objects` and
`entries` are applied from `sync-row/1` images; remote object ids map to
local ones through `sync_replica_rows`. Admission (the protocol's
`AdmissionState`: epoch, applied sequence, applied chain, pending and
retired epochs) is stored in `sync_replica_sources` and **decided and
committed inside the same transaction as the effects it certifies**; the
acknowledgement is sent only after that transaction returns. Because the
effects are ordinary catalog rows, the existing outbox and index follower
project them and search treats them like any other source.

Two adaptations of the simulated protocol follow from real sizes:

- **Streamed epoch change.** A `FullResync` is answered by batches from
  sequence zero of the new epoch rather than one snapshot message. The
  first such batch installs the epoch (`snapshot_applied`); rows of any
  other epoch are tombstoned once the stream passes the head the source
  reported when it offered the new epoch, because every live object of an
  incarnation is stamped at or below that head before anything ships.
  Search stays available throughout, and completeness reports
  `GenerationReset` until the resync completes.
- **Placeholders.** A batch cut by the row limit can deliver a child before
  its parent (the parent was touched again later). The parent's local
  object is allocated without entries and re-projected as a subtree when
  its own row arrives.

Content is never replicated: files say `not_replicated` and content
queries report `ContentNotReplicated`. Directory aggregates are recomputed
periodically from applied rows.

### 4. Identity

Every installation has one ECDSA P-256 key pair and a long-lived
self-signed certificate under `fleet/` in its data directory, generated on
first fleet use and kept across upgrade, repair, and reinstall-with-data
(the key directory's ACL is restricted to SYSTEM, Administrators, and the
service account). The **fingerprint** is SHA-256 of the DER certificate;
the 16-byte **node id** is derived from it. A different key is a different
node by construction. There is no certificate authority.

### 5. Authentication and the sync endpoint

Sync uses a dedicated TLS 1.3 endpoint (default port 7710), never the
loopback web API. Both sides present certificates. A dialing peer pins the
fingerprint it expects from the roster and the handshake fails on any other
certificate. During first contact only, a joining node observes and retains
the master's certificate fingerprint before sending its identity; all retries
are pinned to it. An accepting peer lets the
handshake complete for any well-formed client certificate and then admits
the peer from its roster **before processing any payload**: a roster
fingerprint may sync, an unknown one may only submit a join request to a
master, and anything else is told `unknown peer` and closed with no inventory
disclosed. Failed admissions are counted and logged by
fingerprint only. Credentials and row images never reach ordinary logs.

Frames are length-prefixed JSON with the length checked against the
receiver's own limit before allocation; a malformed or oversized frame
ends that connection only. The first message in each direction is `Hello`
(node id, name, role, nonce, protocol versions, features, frame limit,
credit); a peer sharing no protocol version is refused before anything
else.

### 6. Joining, pausing, leaving, retiring

- A designated master advertises `_eidos-fleet._tcp.local` on its LAN. A
  joining host may select that advertisement or enter the master's IP address
  or host name directly.
- The joining host sends a random request id and its certificate-backed node
  identity. The master quarantines the request; it cannot sync or inspect any
  catalog data while pending.
- A master operator approves or rejects the notification on the Nodes page (or
  with `eidos fleet approve|reject`). Approval and roster admission commit in
  one catalog transaction. The joining host polls its pinned master and starts
  sync after approval without a restart.
- Request replay is idempotent for the same certificate and rejected for a
  different certificate. Pending and rejected states survive process restarts.
- **Pause** disables the central peer: transfer stops, the ledger and
  cursors stay, resume needs no resync.
- **Leave** forgets the central: the maintenance loop removes the ledgers;
  local indexes are untouched; rejoining is a new epoch.
- **Forget** (central) removes a node's credential and retires its
  replicated sources; the central otherwise preserves the last applied
  state of an offline node and reports its freshness truthfully.

### 7. Duplex sessions, direction, and the tie-break

Either peer may dial: a node dials its central; a central dials nodes that
have an endpoint in its roster. After the hello exchange both sides run the
same loop - the *shipper* role offers this side's ready sources and streams
batches, the *consumer* role applies the peer's - so the initiator gains no
authority. Durable progress belongs to the peer identity (`sync_consumers`
on the source side, `sync_replica_sources` on the central side), never to
the connection.

A connection is keyed by `(initiator node id, initiator nonce)`, which both
peers know. When a second connection to the same peer appears, both sides
keep the one with the **smaller key** and close the other, so simultaneous
dials converge on one session without a negotiation round and without
resetting any cursor.

### 8. Flow control and containment

The consumer's `Hello` grants a byte credit (default 16 MiB); the shipper
keeps at most that many frame bytes in flight and releases them on
acknowledgement. Batches are bounded by rows (2 000) and encoded bytes
(4 MiB; a larger batch is halved and re-materialized). Frames are bounded
at 16 MiB. Backlog is measured per source and consumer (rows, tombstones,
oldest age) and reported as **degraded** above a configured ceiling; it is
never dropped. A native journal replacement mints a new epoch. Sync work
runs on its own tasks and bounded blocking calls; a central that is
unreachable costs a node a reconnect timer (2 s doubling to 60 s, with
jitter) and nothing else.

### 9. Versioning

`Hello.versions` lists the protocol versions a build speaks (this build:
`1`); `features` is reserved for negotiated extensions. Row images carry
`sync-row/1`; a central refuses an image version it cannot apply. Mixed
versions fail closed with no data loss: the node keeps its ledger and
retries after the operator upgrades one side.

## Consequences

- Real-SQLite adapter tests reproduce the simulator's duplicate, crash,
  rewind, epoch, compaction, and repair cases; live loopback-TLS tests cover
  join approval, convergence, both initiation directions with a preserved
  cursor, simultaneous dials, and the fail-closed paths.
- The unified installer must preserve `fleet/` across upgrade and repair,
  which it does by treating it as data.
- Everything the v1 control plane adds (leases, cross-network discovery,
  rotation, reassignment, relay, several masters) builds on these identities and
  cursors rather than replacing them.
