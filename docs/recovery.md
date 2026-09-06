# Recovery after v0.5.0

Status: implementation in progress; not deployment-qualified.
Updated: 2026-09-06.

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
stale nextest filter naming the removed collector package; #124 removed
that obsolete configuration. Signed and real-machine gates below
remain required.

PR #124 is merged, with all cross-platform checks passing. It adds durable
metadata scan ceilings and a
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

The policy vertical now implements a writable folder/regex editor, preview,
explicit versioned Apply, resumable catalog-only application and protected
data/index/log boundaries. Focused tests cover old-content removal, preserved
metadata, native changes, unavailable sources and restart after a failed cleanup
acknowledgement. The full Windows local integration gate passed: format,
clippy, generated API, Rust tests/docs, 43 web utility tests, 21 rendered UI
tests and the production web build. PR #125 is now merged with cross-platform
CI passing. Review fixes avoid file-rename whole-source passes, avoid needless
index cleanup commits, preserve policy-pass progress during directory moves,
and keep independent coverage warnings visible. Its final full Rust suite,
14 policy regressions, 20 catalog unit tests, clippy and 24 rendered web tests
passed. Sustained directory moves can still request successive catalog policy
passes and hold that source's content admission; subtree-scoped application
remains a recovery follow-up before qualification under that workload.
See
[exclusion controls](exclusions.md) for semantics and path-alias limitations.

Process RAM and configured cache/index budgets are implemented in the Activity
page, memory API and resource CLI. The values distinguish resident RAM, Windows
private commit, baseline/scan page-cache targets and effective mapped-file
limits. See [memory diagnostics](memory.md). The full Windows local gate passed,
including four memory unit tests, a read-only API regression, 43 web utility
tests, 25 rendered UI tests and the production web build. The required-web
development binary passed a 256-file synthetic crawl, CLI round trip and
15-second unpolled idle observation; see [the recorded smoke limits](benchmarks.md).
Cross-platform review remains pending. This does not introduce a hard memory
limit or measured presets.

Shared-device reader admission is implemented with Windows backing-disk
discovery, an explicit unknown-topology fallback, combined scan/content
reservations, durable limits and Activity/API/CLI controls. The full Windows
local gate passed, including 43 web utility and 32 rendered UI tests. Follow-up
service unit tests (67 passed), 20 integration/export regressions, all-target
service/CLI clippy and the device page tests/build/lint passed after the smoke
caught a populated-map JSON serialization bug. A rebuilt required-web CLI
passed the 256-file device-limit/diagnostics smoke, but its short idle window
still recorded process I/O requiring investigation. Cross-platform review is
pending; see [scope and fallback semantics](device-budgets.md) and the
[measured limits](benchmarks.md).

Chunk B is **not complete**. Device qualification, measured profiles,
sustained-directory-move behavior and real-workload qualification remain.
OS-device admission is not a physical-media guarantee or hard I/O quota.
Signed pushed updates remain chunk C.

The expanded two-root synthetic baseline exposed additional Windows native-feed
idle work. Unknown deletions became source events, and irrelevant journal
progress still caused checkpoint writes. The follow-up filters those deletes
and coalesces irrelevant checkpoint progress while keeping relevant changes
immediate and restart-safe. Ten new USN/cursor/catalog-fault tests pass, as do
the actual native change/restart/overflow tests and full Windows local gate.
A rebuilt two-root candidate's 30-second idle recorded 368,640 process read
bytes and 8,240 write bytes; see the [before/after evidence](benchmarks.md) and
[ADR-0030](adr/0030-bound-irrelevant-journal-checkpoints.md). Review remains pending.
Do not choose performance presets or waive quiet-idle acceptance from the
reader-cap smoke alone.

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
