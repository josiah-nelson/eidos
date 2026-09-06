# ADR-0032: Journal coordinated resource settings

Date: 2026-09-06. Status: implemented and locally validated.

## Context

The content worker pool, metadata limits and shared-device ceiling have
separate runtime owners and persist in three separate files. Applying a custom
resource tuple through three independent HTTP requests can leave a mixed state
after a process exit, filesystem failure or lost client response. Reporting
that sequence as one successful profile would be false. Replacing the existing
files with one new source of truth would also change established manual-control
and restart behavior without a migration need.

The repeated small synthetic comparison established correctness and quiet-idle
bounds for several candidate tuples, but it did not show a repeatable larger-
pool benefit. There is therefore no evidence for named background, normal or
initial-index recommendations yet.

## Decision

Keep the three component files and add a synced operation journal containing a
complete custom target, an ordered component plan and completed-step count.
Apply tighter constraints before relaxed constraints, checkpoint after each
idempotent component replacement and remove the journal only after the full
target is live. Treat component, checkpoint and final cleanup failures as
explicit partial outcomes with the current tuple and repair details.

Replay a valid pending journal before component controls load and before any
configuration-dependent background work starts. Fail startup closed for an
unreadable or invalid journal and for a component/checkpoint recovery failure.
A cleanup-only failure may start because all target settings are consistent;
the retained journal remains visible. Serialize coordinated and individual
HTTP mutations with one outer lock, and reject individual saves while repair
is pending. Component setters keep their own persistence locks and do not
re-enter the coordinator lock.

Compute status from current runtime values on every request. Do not persist a
profile label. Expose a polling-safe custom editor, explicit repair action and
CLI nonzero result for incomplete application. Leave source-specific caps and
pause state outside the tuple. Existing leases drain when ceilings fall.

## Consequences

The operation is recoverable rather than filesystem-atomic. During a failed
live operation, already completed tighter settings can be visible while later
components still have old values; the response says so and preserves the
target. Manual mutation is temporarily unavailable until repair completes.
Startup may refuse service when durable intent cannot be trusted or completed.

Named resource choices require separate, repeatable workload evidence. Adding
them later must map to the same complete request and status must still derive
from the effective tuple, so a later manual edit appears as custom.

## Validation

Four coordinator tests cover tighter-before-relaxed ordering, invalid journal
rejection, an injected partial component write followed by repair, and restart
replay before component controls load. Two HTTP tests cover the complete wire
operation, preserved source-specific caps, explicit partial state, conflicting
manual-write rejection and repair.

The affected service and CLI package gate passed 200 tests with one intentional
OS-topology smoke ignored. All-target clippy passed. The web gate passed 45
utility tests and 36 rendered tests, including polling-safe drafts and partial
repair, followed by lint and the production TypeScript build. The generated
API contract check passed.
