# ADR 0021: Collector fleet packaging, and the stop path it exposed

## Status

Accepted (2026-08-26).

## Context

The Windows observatory collector (ADR-0020) is installed by hand: an
elevated `eidos observe init`, an `eidos observe install --start-now`, and a
`config.json` edited in place for anything beyond the defaults. That is
workable on the machine it was built on and nowhere else. The study needs it
on several hosts for at least thirty days (a cohort that shares one study
key, so content fingerprints compare across hosts), and the hosts have no
build tree, no Rust, and no reason to acquire either.

The indexer already ships a WiX v7 installer (ADR-0013): a dual-scope MSI
driven by a WPF bootstrapper application. The collector is a different
product with a different shape — always per-machine, no web UI, no port, no
user-visible choices — and a fleet installs it unattended.

## Decision

**A separate package, not a feature of the indexer's.** `Eidos.Collector.Msi`
produces `eidos-collector.msi`, `Eidos.Collector.Bundle` wraps it as
`eidos-collector-setup.exe`. Its own `UpgradeCode`, its own ARP entry, its
own `HKLM\Software\eidos-collector`. The two products share only `eidos.exe`,
and a host may run either, both, or neither. Folding the collector into the
dual-scope indexer package would have tied a fleet rollout to a per-user /
per-machine decision that means nothing for a LocalSystem service.

**The WiX standard bootstrapper application.** The chain is one per-machine
MSI with no prerequisites, and every choice a fleet makes it passes on the
command line. Reusing the indexer's WPF BA would have put a .NET Framework
prerequisite on every collector host to render a screen nobody reads.

**Install-time study set-up through the product's own commands.** Between
`InstallFiles` and `InstallServices` the package runs two deferred,
non-impersonated commands: `eidos observe init` (data directory, its ACL, and
the DPAPI machine-scope study key — imported from `EIDOS_STUDY_KEY` when a
cohort key is given, generated otherwise) and `eidos observe configure`, a new
command that writes `config.json` before the service first starts. The
alternative — teaching the installer to write the collector's configuration
format — would have duplicated a schema that already has one owner.

`EIDOS_STUDY_KEY` is a `Hidden` property and its custom action hides its
target, so the key reaches neither a verbose log nor the ARP registry. It is
still an argument on a command line for the moment it is used, so it is
readable by anything on the host that can read another process's command
line: on Windows, administrators and SYSTEM. Importing is never forced —
`observe init` keeps a key that already matches and fails on one that does
not, so an unattended install that reaches the wrong host costs a rollback
rather than a host whose tokens silently split in two.

**Configuration is written only when it is named.** The `configure` command
line is assembled from the properties actually passed, and lanes and upload
are deliberately *not* remembered in the registry. After the first install
`config.json` is the authority, so an upgrade that names nothing leaves a
host's runtime `observe lanes` choices alone.

**Service registration is MSI-native** (`ServiceInstall`, `ServiceControl`,
`ServiceConfig`, `util:ServiceConfig`), matching ADR-0013 and mirroring what
`observe install` registers by hand, so rollback and removal are the
installer's. The data directory's ACL is not: it is set by `observe init`,
which is where the key is written, and where it belonged even before there
was an installer.

**Deleting the data on uninstall is a command, not a file list.**
`util:RemoveFolderEx` builds its list of files while the collector is still
running, so the spool's write-ahead log, a log line, or a cursor moved in
between outlives the list and keeps the directory alive — observed, not
theorised. `EIDOS_COLLECTOR_REMOVE_DATA=1` instead runs a new
`eidos observe purge` after `DeleteServices`, which refuses a directory
holding none of the collector's own files.

## What the upgrade path exposed

An upgrade must stop the running service, and Windows Installer gives that
seconds before it fails with error 1921 and rolls the whole thing back. The
first upgrade attempt did exactly that, and the second, and the fault was
never in the packaging. Three defects in the collector, in descending
severity:

1. **A re-entrant lock on the study key.** The USN reader holds `shared.key`
   across a batch and, for every create or update it nominates, called
   `content_probe::selected`, which reached for the same key through
   `Shared::with_key`. `std::sync::Mutex` is not reentrant: with the content
   lane on, the reader wedged on the first file it nominated. The health
   thread then blocked taking `key` while holding `config` — a lock-order
   inversion against the reader's `key` then `config` — and the lane
   supervisor blocked behind it. Collection stopped silently and the service
   could never be stopped again. `selected` now takes the key it needs as an
   argument, the content lane's settings are read once per batch before the
   key is held, and the health thread holds one lock at a time.
2. **A shutdown flag nothing sets.** The pipe server was constructed with a
   freshly created `AtomicBool` rather than the daemon's own, so it never saw
   a stop: `poke` woke its accept loop, the loop read `false`, and parked
   again. The final `join` never returned.
3. **Supervisor loops that check the flag once a tick.** The enumeration
   scheduler slept thirty seconds at a stretch and the USN supervisor five,
   which on its own is longer than an installer will wait. Both now wait
   through the bounded helper the upload lane already used.

None of the three is visible in a collector that is asked to stop the moment
it starts, which is why they survived the review that merged them. The
regression test in `daemon.rs` therefore gives the collector a key, turns the
content lane on, makes churn for the reader to analyse, lets every thread
settle, and only then asks for a stop — and requires it within ten seconds.
Each of the three defects fails that test. It asks for that status off-thread
and bounded, because the first defect wedges the very threads a status
request waits on: a test written the obvious way hangs on the fault it exists
to name. The CI installer lane does the same against the real service, and
now also upgrades a busy collector in place and checks that its runtime lane
change, upload settings and study key came through untouched.

## Consequences

- Installing a host is one command, and the same command with `/uninstall`
  removes it; the study key survives an uninstall unless asked otherwise, so
  a reinstalled host's tokens stay comparable.
- The collector can now be stopped, which the service could not be before —
  and a clean stop writes the marker whose absence made every restart declare
  a capture gap that never happened.
- Two more artifacts to sign per release; the release workflow signs both
  MSIs, both Burn engines, and both bundles.
- The two study set-up commands and the purge sit outside Windows Installer's
  rollback: they change files the package does not own. A failed install can
  leave a data directory and a study key behind, which is the same thing a
  cancelled uninstall leaves, and harmless — the data directory is kept by
  default anyway. A failed *purge* is the sharp edge: the product can roll
  forward to "still installed" over data that is already partly gone. It runs
  last, only when asked, and only on the way out.
- The launch conditions catch what they can before
  `RemoveExistingProducts` runs — an out-of-range upload hour, a non-elevated
  or 32-bit host — so the common fleet typo stops the upgrade instead of
  unwinding it. A malformed study key is caught later, by the command that
  imports it, and costs a rollback rather than a running collector: verified
  by installing a bad one and finding the previous product still serving.
- The lock discipline is a comment on `Shared`, not a mechanism. The mutexes
  it names are independent, and nothing enforces that no two are held at
  once; a future lane that nests them will not be caught by the compiler.
