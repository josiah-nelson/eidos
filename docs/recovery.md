# Recovery after v0.5.0

Status: implementation in progress; not deployment-qualified.
Updated: 2026-09-05.

v0.5.0 failed in use. Passing tests did not establish acceptable disk load,
initial throughput, idle behavior or a usable fleet workflow.

## Baseline and sequence

The merged runtime fixes include the idle follower wake-up loop, interrupted
scan recovery, content completion state, integer wire compatibility, worker
resizing and storage visibility. Nodes supports advertised/manual-master
selection and durable approval. Those repairs are preserved.

1. Retire the collector throughout code, UI and packaging. Finish explicit
   role-first setup, source selection, partial outcomes and scan retry.
   Add page tests and enforce installer lifecycle before release publication.
2. Finish writable directory/regex exclusions, protection of the application's
   own stores, device-aware scan/content budgets, responsive pause, resource
   profiles and disk-pressure behavior. Fix whole-source aggregation in the
   publication hot path and retry publication acknowledgements.
3. Deliver actual signed, master-initiated updates: version/publisher checks,
   staging, drain, install, restart, health verification and canary rollout.
   Existing Azure signing is the starting point. The current version badge
   is only advisory and does not install anything.

Authentication remains deliberately deferred for this trusted deployment.
It is not a prerequisite substituted for operational recovery.

## Current evidence

Collector retirement and role-first onboarding are implemented. The local
format/lint/API/Rust/web gate passes, including four source/API regressions
and eight behavioral page tests. The core MSI and setup bundle build without
collector payloads, and the MSI service table contains only eidos. Workflow
lint and macOS script syntax checks pass. This is not yet evidence of an
installed, signed recovery candidate. PR #123 is merged; the unsigned Windows
installer lifecycle passed on a disposable runner. Its macOS CI failed on a
stale nextest filter naming the removed collector package; this follow-up
removes that obsolete configuration. Signed and real-machine gates below
remain required.

The next runtime-safety vertical adds durable metadata scan ceilings and a
data-volume reserve through Activity/API/CLI, single-file content claims,
publication-failure backpressure/retry, and indexed source-completion checks.
It also bounds discovery/update checks and fixes standalone setup advancing
without saving when fleet status is unavailable. See
[resource controls and limits](resource-controls.md).

The Windows full local gate passed, followed by focused native-admission/
restart tests after moving admission ahead of journal cursor capture. Twelve
rendered web tests pass. A bounded development-build fixture indexed 1,024
files in 7.058 seconds; its 30-second idle observation used 0.125 CPU seconds
but still performed process I/O. [Raw-counter summary](benchmarks.md) records
the limits of that evidence. No signed/installed recovery claim follows.

Chunk B is **not complete**: writable exclusions and cross-stage self-store
protection form the next policy vertical. Shared-device admission, RAM/cache
visibility, measured profiles and real-workload qualification also remain.
The current source cap is not a physical-device cap, and disk admission is
not a hard quota. Signed pushed updates remain chunk C.

## What must pass before rollout

- Rust, generated API, frontend type/build and behavioral page tests.
- Real per-user and machine install, upgrade, repair, uninstall/reinstall,
  stable fleet identity and retained catalog. Old collector removal must leave
  no running/registered collector and must preserve study data by default.
- A two-host UI workflow: choose roles, approve/reject joining, add selected
  roots, search replicated results, interrupt/restart and recover.
- Controlled initial scan and content work with recorded throughput, CPU,
  process memory, disk reads/writes, queue drain and UI responsiveness.
- A quiet-idle observation window after queues drain, plus pause/resume and
  restart measurements. No unexplained sustained work and no self-indexing.
- A signed canary upgrade, health confirmation and tested recovery path
  before broader deployment.

Use temporary synthetic fixtures first. Record platform, build, duration and
raw counters alongside each result. A protocol simulation, an empty health
check or a successful installer build is not evidence of acceptable real I/O.

Work in cohesive, reviewable changes with one implementation agent. Keep
previous repairs and avoid repeated full CI runs for small follow-ups.
