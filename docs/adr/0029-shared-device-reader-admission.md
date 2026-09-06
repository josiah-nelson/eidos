# ADR-0029: Shared OS-device reader admission with explicit unknown fallback

Date: 2026-09-06. Status: accepted; local implementation gate/smoke passed,
cross-platform review and workload qualification pending.

## Context

Source limits do not bound combined enumeration/content reads when several
roots or partitions share a disk. Counting a multi-threaded scan as one reader
also defeats a combined ceiling. Probing storage on claim/HTTP threads would
introduce an unrelated latency and failure path.

## Decision

Add an in-memory RAII device reservation alongside source admission. Content
charges one reader; metadata enumeration consumes its actual granted width.
Multi-disk volumes atomically charge every known backing device. Keep the
source, pool, scan and free-space controls independently enforceable.

Resolve Windows disk extents through one bounded, demand-driven background
probe. Reject partial mappings. Other platforms and unsupported/network storage
use an explicit conservative fallback. If any source is unresolved, combine all
roots scanned here into that fallback rather than guessing independent media.
OS disk identity is not a guarantee of physical independence behind storage
virtualization. Root changes and stale samples invalidate a mapping.

Mapping changes hold new admissions until existing guards drain; never reset
counters or remap a live guard. Lowered limits likewise drain rather than
revoke work. Removed budgets are discarded once no live guard references them.

Persist this limit separately from existing metadata/free-space settings, so
older resource clients cannot silently overwrite it. Ship API, CLI, Activity
editing, membership/error/wait visibility and regression tests together.

## Consequences

Unknown or changing topology can constrain otherwise fast storage, and a stuck
reader can prolong drain. This is an explicit correctness/performance tradeoff.
There is no claim to cap writer/native/query I/O, the single metadata probe,
IOPS or memory. Measured presets, real-workload qualification and signed update
delivery remain separate recovery work. See [operator semantics](../device-budgets.md).
