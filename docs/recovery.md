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

## First recovery slice: local evidence

Collector retirement and role-first onboarding are implemented. The local
format/lint/API/Rust/web gate passes, including four source/API regressions
and eight behavioral page tests. The core MSI and setup bundle build without
collector payloads, and the MSI service table contains only eidos. Workflow
lint and macOS script syntax checks pass. This is not yet evidence of an
installed, signed recovery candidate; lifecycle and real-machine gates below
remain required.

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
