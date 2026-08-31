# ADR-0019: A lean per-commit CI gate

Status: accepted
Date: 2026-08-25
Amended: 2026-08-31 (remove the full-suite pre-push hook)

## Context

The per-commit gate ran the whole test suite twice, once on Windows and once
on macOS, and the Windows lane set the pace at seven to nine minutes. At the
current commit rate that is the single largest recurring cost in the loop, and
waiting behind it discourages small commits — the thing the workflow otherwise
depends on.

Measuring where the Windows lane spent its time made the shape clear. Of about
250 seconds of test execution:

| crate | Windows test time | platform-gated source files |
|---|---|---|
| eidos-search | 108s | none |
| eidos-service | 85s | 2 windows, 2 unix |
| eidos-catalog | 49s | none |
| eidos-sync | 5s | none |
| eidos-scanner, eidos-diskimg, eidos-cli | 0.4s | 10 windows |

The crates that actually contain Windows-specific code account for less than
half a second of it. The rest is portable code, and the macOS lane runs the
same 37 test binaries and the same 461 tests in well under half the time.

So the Windows lane was spending roughly three minutes of build to reach
fourteen seconds of genuinely Windows-specific testing, duplicating a suite
already covered per-commit elsewhere.

## Decision

**Windows keeps the lint lane, not the test lane.** `cargo clippy
--all-targets` type checks and lints every `cfg(windows)` file, test targets
included, so Windows *compile* breakage still fails the gate in about a
minute. What it cannot catch is Windows *runtime* behaviour.

**macOS is the functional gate.** It runs the full suite and owns the
generated API contract check, which is generated from Rust types and is
identical on every platform.

**Windows runtime behaviour is covered by the scheduled and release gates:**

- `nightly.yml` runs it every night and files a task when it fails, because a
  nightly nobody reads is not a gate.
- `release.yml` runs it on a tag before anything is built or signed.

Developers may run `scripts/check.ps1` when useful, but the repository does not
install or carry a hook that runs the suite on every push. Repeated pushes are
not worth repeated full Windows runs.

## Consequences

Windows-only behaviour — path separators, case folding, file locking,
reserved names — is now caught within a day rather than within a commit. That
class of bug is real for this codebase, and the trade is deliberate: the cost
of finding one at nightly or release time is bounded and rare, while the cost
of the slow gate was paid on every commit.

If the nightly starts failing for Windows-specific reasons more than
occasionally, the answer is to move the offending tests back onto the
per-commit Windows lane by name — not to restore the whole suite, which was
never what earned its place there.
