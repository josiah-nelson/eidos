# ADR-0033: bounded subtree repair after path changes

Status: accepted for recovery implementation; deployment qualification pending.

## Context

Content decisions can depend on a file's source-relative path. A directory
move can therefore change the decision for every file below it without
changing any file bytes. The original implementation reused the full policy
revision pass for this case. That was durable, but it closed content admission
for the entire source. Sustained moves in a small subtree could keep unrelated
files from being processed.

Explicit operator rule edits and protected-path changes still affect an
unknown portion of the source. They require the full revision state machine
and its source-wide admission fence.

## Decision

Keep policy revisions and path repair as separate durable operations.

A path-sensitive move inserts or resets its directory object in a
`policy_repair_frontier` row in the same catalog transaction as entry changes
and the native-feed checkpoint. The frontier is unique by source and object,
so overlapping discovery and repeated pending roots are deduplicated. A
separate per-source row retains phase, checked/changed counts, pending frontier
count and the last error across restart.

The repair coordinator uses the live parent-entry index. Each transaction does
at most 128 object evaluations or direct-child edge expansions. A directory
stores its last expanded `entry_id`; children enter the same deduplicated
frontier. It does not build a recursive subtree in memory or inspect unrelated
source objects. Current-path rendering retains the catalog's 512-component
limit, so the bound depends on affected path depth rather than total source
size.

Path repair does not close source-wide content admission. Content targets are
checked against their current path immediately before extraction. The same
check fences every chunk write, final content-record storage and post-index
publication. If a move makes an in-flight object excluded, its generation and
catalog state are invalidated and a durable derived-index deletion is queued.

Repair and full revision application share the existing `policy_cleanup`
deletion bridge. Neither operation reports complete coverage until content
index deletions commit and their catalog rows are acknowledged. A later move
resets the affected frontier without deleting an existing cleanup row. Full
revision phase, cursor, progress and errors remain independent, so completion
of an older path repair cannot mark a newer operator revision applied.

Missing or tombstoned frontier objects are removed without filesystem access.
Hard-linked files retain the existing canonical-path rule: policy uses the
first live catalog entry, while the frontier deduplicates work by object.

## Consequences

- Unaffected queued files can continue through extraction during subtree
  repair, including while moves continue elsewhere in the source.
- Search coverage reports the path repair and its errors separately from a
  full policy revision. Coverage remains incomplete during repair and purge.
- Restart resumes the exact frontier and child cursor. A failed native change
  rolls back entry changes, checkpoint advancement and frontier insertion
  together.
- Repair cost scales with the affected catalog topology and path depth. One
  very wide directory needs multiple bounded turns.
- Physical aliases and hard links to protected storage retain the limitations
  described in [ADR-0028](0028-versioned-content-exclusions.md). This change
  does not establish filesystem isolation or deployment qualification.
