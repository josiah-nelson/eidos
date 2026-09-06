# ADR-0028: versioned content rules and resumable application

Status: accepted for recovery implementation; deployment qualification pending.

## Context

The recovery requires writable, explainable exclusions that affect existing
content, and protection against indexing the application's own stores. Saving
rules for future scans alone leaves old content searchable. Re-crawling source
media to change a policy adds avoidable I/O during recovery.

## Decision

Store ordered per-source rules with optimistic revision checking. Keep content
policy independent from inventory and retain default classification, including
the absence of a blanket `bin` exclusion. Operator includes do not override
read-safety constraints. Offer the same explicit validation/preview/apply
workflow in the Source UI, API and CLI.

Keep engine version and per-source rule revision independent. Recorded operator
decisions identify the rule revision and rule ID alongside the engine version,
so an engine upgrade cannot collide with an operator edit in policy history.

Apply closes content admission and new scans for the source, drains already
claimed files, then seeks through catalog objects in bounded indexed pages.
Each transaction persists the cursor, decisions, generation invalidations,
aggregate changes and cleanup intent. A dedicated durable deletion queue
bridges catalog transactions and the derived content-index commit; cleanup
acknowledgement precedes reopening admission. Interrupted or failed application
cannot silently report completion or discard pending cleanup.

Configured data/index/log paths are immutable inventory boundaries. Enumeration
does not descend; native changes cannot recreate internal descendants. Existing
content is purged through the same apply mechanism. Boundary metadata and
coverage gaps remain visible, including last-known inventory. A removed
boundary clears only after successful enumeration.

## Consequences

- Applying policy works on an unavailable source, without a new disk crawl.
- Search coverage changes progressively, with explicit progress and errors.
- Directory moves schedule the separate bounded subtree repair defined by
  [ADR-0033](0033-bounded-subtree-policy-repair.md). Unaffected content claims
  continue while coverage reports the pending repair and purge.
- Protected paths are not a sandbox or a proof of arbitrary alias/hard-link
  equivalence. The configured roots must describe the indexed namespace.
- The new seek index is a migration cost on existing catalogs; installed
  upgrades still need disk-space and real-corpus qualification.
- Physical-device admission, process memory visibility, measured resource
  profiles, signed pushed updates and installed acceptance remain open gates.

See [the operator guide](../exclusions.md) for precedence and recovery behavior.
