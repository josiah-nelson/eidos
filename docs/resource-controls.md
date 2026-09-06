# Runtime resource controls

Activity → Resource limits saves the metadata enumeration width, concurrent
scan ceiling, and free-space reserve on the **data directory's**
filesystem. These limits survive service restarts in `resource-limits.json`.
Invalid settings are rejected; a failed write leaves the effective settings
unchanged. An unreadable/corrupt saved configuration prevents startup with an
error instead of silently restoring more aggressive defaults.

Defaults are the launch-time `--scan-threads` value (bounded to 1–64), one
concurrent scan, and a 1,024 MiB free-space reserve. These are explicit
admission ceilings, **not measured optimal performance profiles**. New scans
request the saved width; shared-device availability can grant fewer threads.
Active enumeration keeps its granted starting width; lowering
concurrency admits nothing further until reservations drain. Queued scans
show their waiting reason and can be cancelled without opening a generation.
The running service reserves before native capability/cursor probes and holds
the slot through replay/publication. Queue time therefore does not inflate the
journal overlap window. Long-lived native watchers remain separate.

The content worker pool remains a separate control. Each worker now claims
one file at a time. Pause/shrink finishes at most that current file per worker
and publishes normally; it no longer drains a sixteen-file preclaimed batch.
A large file or a stalled filesystem read can still take time. This is not
mid-file cancellation or a hard I/O deadline. Pause remains durable.

Activity also offers a [coordinated custom tuple](coordinated-resource-settings.md)
covering this pool, the three metadata settings and the shared-device ceiling.
It journals the target before changing the existing component files, reports
partial outcomes explicitly and repairs them after a persistence failure or
restart. It does not change source-specific caps or claim that a custom tuple
is a measured preset.

Source caps are per source, **not per physical disk**. The additional
[shared-device reader ceiling](device-budgets.md) combines scan/content readers
across roots on known backing OS disks, with a conservative shared fallback for
unknown topology. Raising the pool does not bypass it. Measured
normal/background/initial-index profiles remain recovery work.

## Disk pressure and publication faults

Free space is sampled every five seconds by a single-flight background probe.
New scans, content claims and periodic content queue top-up are held below the
reserve, on a failed probe, or when the last sample is over 30 seconds old.
Work resumes automatically after a healthy sample. Activity shows the sampled
free bytes, sample age and waiting reason. Setting the reserve to zero
explicitly disables this check.

Already running files/scans, index publication, native changes and fleet writes
are not interrupted. Consequently the reserve is an admission threshold, not
a guaranteed remaining-space quota. It does not protect separately configured
log volumes or every other writer. Memory usage and configured cache budgets
are visible in [Activity and the CLI](memory.md); they are not a hard RAM quota.
Cross-stage self-store protection is described in [exclusions](exclusions.md).

When an index commit or its catalog acknowledgement fails, the content pipeline
retains pending IDs, holds new extraction and retries publication at the normal
two-second interval. A successful acknowledgement resumes admission without a
restart or source re-extraction. Activity's last error remains diagnostic history.
Source completion uses indexed existence checks rather than summing the entire
source on each finalization.

## API and CLI

`GET /api/resources` returns `limits`, `active_scans`, `free_bytes`,
`disk_sample_age_s` and `admission_blocked`. The last three fields may be null;
64-bit values use the API's decimal-string convention. `POST /api/resources`
replaces all three limits; invalid ranges return 400 and persistence failures
return an actionable error.

```json
{"scan_threads":2,"concurrent_scans":1,"minimum_free_mib":2048}
```

```powershell
eidos resources --json
eidos resources --scan-threads 2 --concurrent-scans 1 --minimum-free-mib 2048
eidos resources --memory            # process RAM and configured budgets
eidos content workers 4
eidos content pause
eidos content resume
eidos resources coordinated
```

`GET /api/memory` and `eidos resources --memory` report observed process memory
alongside the configured cache and index-writer budgets. They set nothing; see
[memory diagnostics](memory.md) for the freshness contract and for why a budget
is not resident RAM.

For discovery, `/api/volumes` uses a five-second single-flight cache outside
the operator thread pool, with a two-second cold-response deadline. The
single-flight guard, not the cache lifetime, is what prevents a pile-up, so
the lifetime stays short and setup's **Rescan drives** picks up media attached
mid-onboarding. Retrying a stuck probe never starts a second OS thread.
Windows offers remote/removable/optical roots without opening them for
capacity/capability information.
Unavailable metadata is unknown, not evidence that a drive is healthy.
The advisory release check has a 15-second overall deadline including the body.

## Reproducible checks

The focused `recovery_controls` tests inject catalog-acknowledgement failure
after index commit, exercise disk-pressure admission/cancel/recovery, and drive
the resource API. Component tests cover explicit saves, polling-safe drafts,
persistence errors and waiting/recovery visibility. The completion-query
regression runs against the real catalog schema at 1, 1,000 and 100,000 files.

For a bounded Windows measurement using only new temporary fixtures:

```powershell
cargo build -p eidos-cli
./scripts/recovery-smoke.ps1
```

The script launches the candidate on loopback with its own data/log/source
directories, fleet/reconciliation/update checks disabled, and two content
workers. It records initial drain, HTTP query latency, CPU, working set and
process I/O, followed by a short unpolled idle window. It stops only its own
process and retains the temporary report/fixture for inspection. Process I/O
counters include cached/file/network operations; they are not physical-disk
measurements. This smoke check does not qualify production throughput, shared
devices, long idle behavior, installed lifecycle or signed deployment.
