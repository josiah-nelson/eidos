# Private fleet (experimental, v0.5)

v0.5 can run as a small, explicitly enrolled dogfood fleet: several
standalone Windows installations replicate their catalog metadata into one
manually configured central, and the central searches the union. Every
node keeps working on its own when the central is absent; the central is
never a prerequisite for local readiness. The boundary, identity, and
protocol contract are in [ADR-0023](adr/0023-experimental-fleet-boundary.md);
content is not replicated ([ADR-0024](adr/0024-content-transfer-bakeoff.md)).

## What replicates

Every eligible local source of an enrolled node - published, not retired,
not marked `local_only` - ships its metadata (objects, entries, timestamps,
sizes, attributes, archive membership) continuously. On the central each
one appears as a source named `<node>/<source>` of kind `remote`, owned by
a host row for the node, and is searchable like any other source:

- hits carry the origin host and source;
- per-source completeness carries the origin, the applied sequence, the
  head the node last reported, when the last batch landed, and whether the
  node is connected;
- freshness is *live* while the node is connected and *unknown* otherwise,
  with results preserved as of the last applied batch (`offline` in the
  coverage envelope, with a watermark);
- content is never replicated: files say `not_replicated`, and content
  queries report `ContentNotReplicated` with a pointer at the origin node;
- `host:<name>` (or `host:<id>`) narrows a query to one node's sources.

## Setting up a central

On the installation that will hold the fleet's replicas:

```powershell
eidos fleet central --listen 0.0.0.0:7710     # accept sync sessions on port 7710
eidos fleet invite                             # prints a single-use invitation
```

The same setup is available in the web UI: the Fleet page's "This node"
card turns on the central role and binds the listener, and "Invite a node"
mints the single-use code.

The sync listener is a dedicated TLS endpoint; keep the separate web API on
loopback. Open port 7710 on the private network the nodes share (the
installer does not open it). The invitation embeds the central's certificate
fingerprint, its endpoint (the host name and the listener port; pass
`--endpoint host:port` to choose), and a single-use secret that expires
after 24 hours.

## Enrolling a node

On each node, with the service running:

```powershell
eidos fleet enroll eidos-fleet-v1:...          # paste the invitation
eidos fleet status                             # sessions, cursors, backlog
```

Or in the web UI: paste the code into the Fleet page's "Enroll with a
central" card. The same page carries the roster (per-peer endpoint, sync
on/off, forget), live sessions with per-source cursors, and each local
source's ledger state — everything `eidos fleet status` prints.

Enrollment connects to the central pinned to the fingerprint in the code,
redeems the invitation, and records the central in the node's roster. The
service picks it up at its next tick: it enables the ledger for every
eligible source, backfills, dials the central, and streams. Nothing needs
a restart.

Exclusions and pausing:

```powershell
eidos fleet policy <source name> --local-only  # never leave this host
eidos fleet policy <source name> --inherit     # follow enrollment again
eidos fleet pause                              # stop transfer, keep cursors
eidos fleet resume
eidos fleet leave                              # forget the central, drop the ledgers
```

Pausing keeps the ledger and cursors, so resuming needs no resync; leaving
removes the ledgers, and rejoining is a new epoch (a full stream of the
source, applied while the previous copy stays searchable).

## Central-initiated sessions

Either side may dial. A node dials its central by default; a central dials
a node once it knows where to reach it:

```powershell
# on the node: listen without enabling the central role
eidos fleet central --disable --listen 0.0.0.0:7710
# on the central: tell it where the node is
eidos fleet peer <node id> --endpoint node-host:7710
```

Both directions carry the same protocol; the initiator gains nothing.
Cursors, credits and acknowledgements belong to the peer identity, so
changing direction across reconnects neither duplicates nor loses work.
When both sides dial at once, both keep the connection with the smaller
`(initiator, nonce)` key and close the other.

## Status

`eidos fleet status` (or `GET /api/fleet`) shows:

- identity: node id, name, certificate fingerprint, role;
- peers: role, fingerprint, endpoint, enabled, connected, last error, next
  dial;
- sessions: direction, peer, credit remaining, and per source the phase
  (`offered`, `in sync`, `batch through N in flight`, `repairing`,
  `fenced: <reason>`), cursor, head, batches and rows;
- local sources: policy, ledger state, epoch, head, compaction point,
  backlog rows and tombstones with the oldest age, and a degraded flag when
  the backlog is over its ceiling;
- replicated sources (central): origin, epoch, applied sequence versus
  reported head, resync in progress, connected;
- counters: connections by direction, duplicates closed, batches and rows
  shipped/applied, acknowledgements, duplicates acknowledged, stale
  batches, fences, full resyncs, repairs, bytes by message family
  (control, catalog, repair), materialization and apply time, backfill
  steps, collections, tombstones collected.

`degraded` lists what an operator should act on: a backlog over its
ceiling, a listener that failed to bind, a fenced source.

## Bounds

`fleet/config.json` in the data directory (written by `eidos fleet
central`, editable by hand; re-read every two seconds):

| key | default | meaning |
|---|---|---|
| `central` | `false` | accept enrollments and replicas |
| `listen` | `null` | sync listener address |
| `max_frame_bytes` | 16 MiB | largest frame accepted or sent |
| `credit_bytes` | 16 MiB | bytes a peer may have in flight towards this side |
| `batch_rows` / `batch_bytes` | 2000 / 4 MiB | batch bounds (a larger batch is halved) |
| `reconnect_max_secs` | 60 | reconnect backoff ceiling |
| `backlog_ceiling_rows` / `backlog_ceiling_tombstones` | 5 000 000 / 1 000 000 | per-source backlog above which status says degraded |
| `repair_leaf_bits` | `null` (by size) | Merkle leaves for repair offers |

A backlog over its ceiling is reported, never dropped: the ledger holds one
row per live object plus unacknowledged tombstones, so a node that is
offline for weeks grows by its deletions, not by its edits.

## Failure matrix

Evidence for the sprint's required matrix. *Automated* rows are tests that
run in CI and locally (`cargo test`); *soak* rows are exercised on the
private fleet with the release candidate and recorded in the release notes.

| Scenario | Evidence |
|---|---|
| Agent offline for at least 72 hours | automated checks cover durable cursor restart (`a_node_that_restarts_with_unacknowledged_work_resumes_from_the_same_cursor`) and the fifty-edit catch-up shape; soak: disconnect the intermittently connected machine for at least 72 hours |
| Central stops before apply commit | by construction: effects and cursor are one SQLite transaction; automated: `a_central_that_stops_before_acknowledging_is_caught_up_by_the_resend` |
| Central stops after commit but before ACK | automated: same test (durable apply, resend answered `AlreadyApplied`, node acknowledges) |
| Agent stops with unacknowledged work | automated: `a_node_that_restarts_with_unacknowledged_work_resumes_from_the_same_cursor` |
| Both peers initiate simultaneously | automated: `simultaneous_initiation_leaves_exactly_one_session_on_both_sides` |
| Connection direction changes | automated: `either_side_may_initiate_and_the_cursor_survives_a_direction_change` (no resync, no duplicate registration) |
| Duplicate, overlapping, delayed, reordered frames | automated: `duplicate_and_overlapping_batches_are_idempotent`; the protocol simulator's million-universe soak |
| Journal replacement or deliberate rebuild | automated: `an_epoch_change_streams_a_full_resync_and_retires_rows_the_new_epoch_lacks`; runtime mints a new epoch on a USN journal-id change |
| Same-epoch rewind/restore | automated: `a_same_epoch_rewind_is_fenced_and_a_rewritten_history_is_a_fork` |
| Retained suffix no longer covers central | automated: `a_cursor_below_the_compaction_floor_is_repaired_by_merkle_leaves` |
| Central unavailable during heavy local churn | automated: local search and scans never wait on the fleet (separate tasks, bounded blocking calls); soak: churn with the central stopped, backlog within its ceiling |
| Old/new protocol versions meet | automated: `unknown_peers_bad_invitations_and_foreign_versions_fail_closed_before_any_payload` |
| Core or collector upgraded while busy | installer workflow: busy upgrade of the collector, repair and reinstall keep the fleet identity and the study key |

## Soak checklist (release candidate)

1. Install the release candidate with the unified setup on the central,
   one stable workstation, and one intermittently connected machine, with
   the collector selected on each.
2. Record the sync-off baseline on every host (`observe status`, the
   product's `/api/activity`, and search p95 from `eidos bench search`).
3. `eidos fleet central --listen`, `eidos fleet invite`, `eidos fleet
   enroll` on both nodes; give the central the workstation's endpoint so
   one session is central-initiated.
4. Run normal work for the soak window; keep `eidos fleet status` output
   and the collector bundles.
5. Exercise the matrix rows marked *soak*: stop the central during churn,
   disconnect the laptop for the longest window available, restore a node's
   catalog from a backup and confirm the fence, replace a USN journal.
6. Compare search p95 with sync on against the baseline (gate: within 15 %),
   central apply capacity against the fleet's aggregate arrival rate (gate:
   5x), and backlog growth over the longest disconnection.
7. Record every result in the release notes; a failed fleet gate ships the
   same installer with sync disabled and the gate documented.
