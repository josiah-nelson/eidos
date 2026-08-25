# ADR 0014: Privacy-typed macOS observatory collector

## Status

Accepted.

## Context

A 30-day workload study needs macOS filesystem and process measurements while
preserving source-tree read-only behavior and a strict export boundary.
FSEvents and Endpoint Security have different cursor, privilege, failure, and
privacy properties. Endpoint Security also uses a restricted entitlement that
must be authorized by an embedded Developer ID provisioning profile.

The entitlement may be approved after the rest of the collector is ready. L0
and L1 therefore cannot depend on L2 initialization. File Provider objects add
another hard constraint: observation must not materialize file bytes.

## Decision

Create a platform-neutral `eidos-observe` crate that owns the versioned schema,
privacy types, bounded spool, bundle writer/reader, inspector, capability
declarations, and native-feed trait. Feed cursors contain a feed kind, version,
and adapter-owned opaque value; USN and FSEvents cursor fields are never
pretended to be interchangeable.

Identity-bearing durable fields use an `ObjectToken` type whose public
constructor requires a `StudyKey`. This makes an accidental raw string write a
type error. Exact scalar values use closed bucket enums. The study key lives in
the login keychain and is provided to the daemon only for the active user
session.

Use Rust for schema, ring, IPC, lifecycle, and aggregation. Use the maintained
FSEvents binding for resumable per-file notifications. Use narrow native
bridges for Endpoint Security and Foundation resource metadata:

- the ES bridge subscribes only to `NOTIFY_OPEN`, `NOTIFY_CLOSE`,
  `NOTIFY_MMAP`, and `NOTIFY_EXEC` and returns only coarse counters;
- the Foundation bridge requests size, allocation, ubiquity, and download
  status metadata and never opens a file or requests a download.

Run the privileged collector as a root LaunchDaemon. Run `eidos observe run`
as a per-login LaunchAgent. They communicate over a root-owned Unix socket,
not TCP. The socket protocol bounds requests to 64 KiB. The daemon stages
exports in its strict directory; the unprivileged CLI copies the completed
bundle to its requested destination.

Wrap the daemon in `Eidos Collector.app` because a restricted entitlement
needs an embedded provisioning profile. Keep separate pending and approved
entitlements. Select the approved file only when the profile passes CMS,
expiration, entitlement, application-identifier, and certificate checks and
`EIDOS_ES_ENTITLED=1` is set. Harden, timestamp, notarize, staple, and verify
the app. Produce a signed/notarized component package only when a Developer ID
Installer identity exists; otherwise retain the notarized app and explicit
installer script.

Treat entitlement authorization, TCC Full Disk Access, and effective root as
independent capability facts. An ES initialization failure degrades L2 only.

## Consequences

- The durable Rust API cannot represent a raw object, subtree, volume, signing
  identifier, or mark.
- A restart resumes from the FSEvents event ID and exposes discontinuities via
  drop/root/gap counters; it does not manufacture a common USN/FSEvents cursor.
- Cloud placeholder prevalence is conservative. Unknown providers are left
  unknown instead of being probed with byte-reading behavior.
- The daemon can collect health before a user session supplies the study key,
  but it discards identity-bearing detail during that interval.
- ES can be compiled and tested before entitlement approval, remains off by
  default, and has a one-flag signing transition after approval.
- Manual export and exact-field inspection remain the only path out of the
  local spool.

## Rejected alternatives

Storing raw events and anonymizing during export was rejected because the
spool would itself become a sensitive raw event stream. Unkeyed hashes were
rejected because paths can be guessed offline. A TCP control service was
rejected because no remote control or listener is needed. An ES authorization
subscription that immediately allows operations was rejected because even a
pass-through authorization client can delay or block work. Opening placeholder
files to classify them was rejected because measurement must not cause the
workload it is measuring.
