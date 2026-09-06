# Process RAM and configured budgets

Activity → **Memory usage and budgets** separates current process consumption
from configuration targets. The same information is available from
`GET /api/memory` and `eidos resources --memory [--json]`. This is read-only;
it does not change resource limits or enable a new background collector.

The process probe refreshes on demand at most once every five seconds. A
single-flight worker prevents concurrent requests from multiplying probes.
A request that finds no sample, or one older than 30 seconds, waits up to two
seconds for the refresh it just started, so a one-shot CLI call after a long
idle period still answers with a current sample instead of asking the caller
to retry. A usable sample never waits. If the probe is still running when that
deadline passes, the response says so — pending or stale — rather than
inventing a number, and no second probe is started. During a refresh the
previous sample remains available with its age. Errors and unsupported
counters are unavailable, not zero. No sampling timer runs when the endpoint
is not requested. Activity polls once a second while a sample is pending or
stale and once every five seconds afterwards; a failed poll reports the request
failure without discarding the last successful sample and its budgets.

## What the numbers mean

- **Resident RAM / working set:** Windows working-set bytes or macOS/Linux
  resident-set bytes for the running service, including resident shared and
  file-backed pages. This is not necessarily Task Manager's default private
  working-set column.
- **Peak resident RAM:** the OS-reported lifetime peak when available; absent
  on the current macOS adapter.
- **Private committed bytes:** Windows private commit, which may be resident
  or paged out. It is not private resident RAM. See Microsoft's
  [process memory counters](https://learn.microsoft.com/en-us/windows/win32/api/psapi/ns-psapi-process_memory_counters_ex).

The macOS probe uses the current PID's `PROC_PIDTASKINFO` resident bytes,
defined by Apple's [process information contract](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/proc_info.h).
Linux reads only a bounded `/proc/self/status` buffer. No source tree, process
list, database table or index directory is enumerated to obtain these values.

## Budgets are not usage

The running code supplies these values; the frontend does not duplicate them:

| Component | Current configuration | Meaning |
|---|---|---|
| Catalog page caches | 64 MiB per connection; 832 MiB for 12 pooled readers plus the baseline writer | Suggested cache targets, not allocated upfront |
| Additional scan connections | Another 64 MiB target per open scan session | Outside the baseline connection total |
| Catalog memory mapping | Effective `PRAGMA mmap_size` read at startup | Maximum mapped file range per connection, not allocated RAM |
| Name-index writer | 96 MiB | Writer budget shared by its indexing threads |
| Content-index writer | 256 MiB | Writer budget shared by its indexing threads |

SQLite may clamp the requested mapping range to its build/platform maximum;
the API reports the effective result. SQLite's [cache pragma](https://www.sqlite.org/pragma.html#pragma_cache_size)
is a suggested limit, and [mapped I/O](https://www.sqlite.org/mmap.html) can
share physical pages with the OS cache. Do not add mapped ranges, cache targets
or writer budgets to the resident total or treat their sum as a process ceiling.

Index readers, temporary in-memory SQLite work, extraction buffers, policy
caches, allocator overhead and other runtime allocations remain outside these
individual budgets. SQLite allocator tracking stays disabled to preserve the
existing contention fix; this change does not turn its disabled counter into
a misleading zero or re-enable a global allocation lock.

This visibility is a recovery diagnostic, not a hard memory limit, a performance
profile or deployment qualification. Shared physical-device admission and
measured resource choices remain separate work. See [recovery](recovery.md).
