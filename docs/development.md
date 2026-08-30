# Development

Reproducible build, test, lint, and benchmark commands. Windows is the
primary development platform. macOS builds and tests the whole workspace and
has native enumeration (`getattrlistbulk`, APFS volume capabilities) and a
native change feed (FSEvents, with a cursor validated against the volume's
event store). A volume that keeps no event history stays on periodic
reconciliation. Other platforms fall back to the portable `readdir` lister
with no change feed.

## Toolchain

| Tool | Version used | Install |
|---|---|---|
| Rust (stable, `x86_64-pc-windows-msvc`) | 1.98 (floor 1.88) | `rustup` (https://rustup.rs); components `clippy`, `rustfmt` |
| MSVC Build Tools + Windows SDK | VS 2026 Build Tools, SDK 10.0.26100 | Visual Studio Installer, "Desktop development with C++" |
| Node.js / npm | 24.x / 11.x | https://nodejs.org |

`%USERPROFILE%\.cargo\bin` must be on `PATH`.

## Commands

Install the pre-push hook once per clone. CI lints Windows but does not test
it (see `docs/adr/0019`), so on a Windows machine this hook is the only
automated check that runs the suite on Windows before code lands:

```powershell
git config core.hooksPath scripts/hooks
```

It runs `check.ps1 -SkipWeb -SkipRelease` on pushes that touch Rust, skips
pushes that do not, and is bypassed for one push with `git push --no-verify`.

```powershell
# Windows. One-shot: format check, clippy (deny warnings), all tests,
# release build, web lint+test+build
.\scripts\check.ps1            # -SkipWeb / -SkipRelease to shorten

# Individually
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
cd web; npm ci; npm run lint; npm test; npm run build
```

```bash
# macOS or another Unix host: the same steps in the same order.
scripts/check.sh               # --skip-web / --skip-release to shorten
```

`scripts/macos/build-agent.sh` builds the `Eidos.app` bundle the macOS agent
is installed from; see [installing-macos.md](installing-macos.md).

CI runs the Rust gate on both Windows and macOS, because the enumeration and
change-feed adapters differ per platform and the contracts they share are only
proven when both run them. The Windows lane also checks formatting and that
the generated API contract in `web/src/generated/api.ts` is not stale.

Tests never touch user data: every integration test builds its own fixture
under a `tempfile::tempdir()`. USN-journal tests need an elevated session
and skip themselves otherwise.

The scanner's filesystem contract suite runs over every enumeration adapter
the current platform can use, so a native fast path cannot diverge from the
portable reference. Point it at another volume to cover behaviour the boot
volume does not have — a case-sensitive filesystem, exFAT, or a mounted
share:

```bash
# macOS: a scratch case-sensitive APFS volume
hdiutil create -size 64m -fs "Case-sensitive APFS" -volname Scratch -type SPARSE scratch
hdiutil attach scratch.sparseimage
EIDOS_TEST_VOLUME=/Volumes/Scratch cargo test -p eidos-scanner --test filesystem_contract
hdiutil detach /Volumes/Scratch
```

### Property and fuzz tests

Normal `cargo test` runs deterministic, shrinking property suites for query
parse/render round trips, AST limits, Unicode trigram candidate soundness, and
cursor encoding/decoding. Their RNG seeds are fixed in source so local and CI
runs exercise the same baseline cases. When a generated case exposes a bug,
keep the minimized input as a named ordinary regression test before removing
the generated failure file.

The libFuzzer targets cover arbitrary bounded UTF-8 at the parser and regex
planner entry points. `cargo-fuzz` requires nightly Rust, LLVM sanitizers, and
an x86-64 or AArch64 Unix-like host; the scheduled GitHub workflow supplies a
short deterministic smoke budget and uploads `fuzz/artifacts` on failure.

```bash
rustup toolchain install nightly
cargo install cargo-fuzz --version 0.13.2 --locked

# Five-minute local passes over the checked-in seed corpora.
cargo +nightly fuzz run query_parser fuzz/corpus/query_parser -- \
  -seed=3771723815 -max_len=16384 -max_total_time=300 -timeout=5 -rss_limit_mb=2048
cargo +nightly fuzz run regex_plan fuzz/corpus/regex_plan -- \
  -seed=3771730215 -max_len=5122 -max_total_time=300 -timeout=5 -rss_limit_mb=2048

# Longer exploratory run: omit -seed for new paths and raise the time budget.
cargo +nightly fuzz run regex_plan fuzz/corpus/regex_plan -- \
  -max_len=5122 -max_total_time=3600 -timeout=5 -rss_limit_mb=2048
```

The target-side caps mirror the production regex/text limits (1,024/4,096
bytes); parser fuzzing permits up to 16 KiB so Boolean syntax and many clauses
fit while memory stays bounded. Minimize a failure with `cargo fuzz tmin`, add
the minimized input to the corresponding corpus, and preserve its behavior in
a normal Rust regression test.

### Deterministic sync soak

The ordinary suite runs focused `eidos-sync` fault storms. The scheduled
`sync DST soak` workflow runs one million release-mode protocol universes,
uploads versioned replay expressions for failures, and opens one deduplicated
issue per recorded failing seed. Run the same gate locally with:

```powershell
cargo run --locked --release -p eidos-sync --bin dst-soak -- 1_000_000 sync-soak-failures.jsonl
```

The third optional argument caps recorded failures (default 20) so a broad
regression cannot create an issue storm.

### HTTP integer contract

Public schema version 2 represents every Rust `i64`/`u64` in ordinary JSON
API responses as a decimal string. This includes opaque IDs, byte/count
fields, generations, and Unix-nanosecond timestamps. Rust `u32`/`i32`,
floating-point values, and `usize` bounds remain JSON numbers. Error details
stored through `serde_json::Value` also stringify integer values because the
original Rust width is no longer available at serialization time.

Request decoders accept both the decimal-string form and legacy JSON numbers
for opaque IDs and timestamps. New clients must emit strings. The web UI
retains IDs as strings, formats counts through `BigInt`, and only converts to
`number` at display-only boundaries such as dates, durations, and chart
geometry. Identity and ordering never depend on that lossy conversion.

### Generated TypeScript contract

Rust request/response structs, enums, completeness/progress records, and the
base error body are the source of truth for the web contract. `ts-rs` walks
the endpoint roots in `eidos-service::api_contract` and writes their complete
dependency graph to `web/src/generated/api.ts`. Regenerate it with:

```powershell
cargo test -p eidos-service export_api_contract --lib
```

The generated file is checked in but must never be edited by hand. Both CI
and `scripts/check.ps1` rerun the export and fail if it changes. CI checks it
after the suite rather than before, so contract drift shows up at the end of
the macOS lane. Rust-owned
types are imported from that file; view state and other UI-only types stay in
`web/src/api.ts`.

Adding an optional response field or enum variant is compatible within a
schema version. Removing or renaming a field/variant, changing its meaning,
or changing a wire representation requires a public schema-version bump and
a migration note. Cursors remain opaque strings: clients may store and return
them but must not parse or synthesize them.

## Running the service

```powershell
.\target\release\eidos.exe source add projects D:\Projects        # register a source
.\target\release\eidos.exe source add share \\fileserver\share     # SMB roots work too (no live feed)
.\target\release\eidos.exe serve --data-dir data                     # http://127.0.0.1:7700
```

The built web UI (`web/dist`) is embedded in `eidos.exe` at compile time, so
one file serves both the API and the UI; `build.rs` re-embeds it whenever
`web/dist` changes. A checkout without a web build still compiles (API only,
with a build warning); `EIDOS_REQUIRE_WEB=1` turns that into a build error
and is set by the release workflow. `--web-dir web\dist` serves the UI from
disk instead (edit-refresh during UI work) and `--no-web` serves the API
only.

`serve --detach` starts the same service as a background process of the
current user (no console window, logs under `--log-dir`, defaulting to
`<data-dir>\logs`), waits until `/api/health` answers, prints the URL, and
returns; it does nothing when something already answers there. This is the
per-user counterpart of the Windows service below.

### Windows service

```powershell
# Register (elevated). Every serve flag is stored on the service command line.
.\eidos.exe service install --data-dir C:\ProgramData\eidos --bind 127.0.0.1:7700
.\eidos.exe service install --data-dir D:\eidos --account user            # prompts for the password
.\eidos.exe service start | stop | restart | status [--json] | uninstall
```

`install` registers `eidos.exe service run …` with the service control
manager: delayed automatic start (`--start auto|manual|disabled`), restart
on failure (5 s, 30 s, 2 min; reset daily), a 3-minute pre-shutdown window,
and logs in `<data-dir>\logs\eidos.log.<date>` (`--log-dir` to move them).
`--account local-system` (default) sees local disks only; `local-service` /
`network-service` are least-privilege; `user` runs as a named account
(default: the current user; `--user DOMAIN\name`, `--password-stdin` for
scripts), validates the credentials, grants *Log on as a service*, and gives
the account Modify on the data and log directories. `--replace` updates an
existing registration in place. `status` shows the registration, state, and
whether `GET /api/health` answers. `uninstall` removes the registration and
never deletes indexed data.

The `service_scm` integration test drives the whole cycle through the real
SCM under a throwaway name; it needs an elevated session and
`EIDOS_SCM_TESTS=1`, and skips itself otherwise.

The service scans new sources, keeps local NTFS sources live through the USN
journal, rescans feed-less sources on their reconcile interval, rebuilds the
search index whenever a source's published generation changes, and follows
the catalog outbox every 500 ms (`GET /api/index` shows follower state). Each
successful projection iteration commits and reloads the in-process Tantivy
reader before recording its catalog position, so API searches see the update
as soon as the iteration returns.

`serve` flags / environment: `--data-dir` (`EIDOS_DATA_DIR`), `--bind`
(`EIDOS_BIND`, default loopback — the API has no authentication yet and warns
when bound elsewhere), `--web-dir` (`EIDOS_WEB_DIR`, overrides the embedded
UI), `--no-web`, `--log-dir` (`EIDOS_LOG_DIR`, daily files in addition to
stderr), `--scan-threads`, `--no-auto-reconcile`, `--no-content` (metadata only),
`--content-workers N` (`EIDOS_CONTENT_WORKERS`, default 4),
`--export-page-size N` (`EIDOS_EXPORT_PAGE_SIZE`, default 500) and
`--export-max-rows N` (`EIDOS_EXPORT_MAX_ROWS`, default 100000) and
`--export-concurrency N` (`EIDOS_EXPORT_CONCURRENCY`, default 2) — see
[Exporting results](query-syntax.md#exporting-results) — plus the
admission-control flags below.

### Operational limits

Search, browse, archive listing and count endpoints run blocking
SQLite/Tantivy work. The service admits a bounded number of them at once and
sheds the rest rather than letting latency grow without bound:

| Flag | Environment | Default | Meaning |
|---|---|---|---|
| `--max-concurrent-queries` | `EIDOS_MAX_CONCURRENT_QUERIES` | 4 | Expensive operations running at once |
| `--query-queue-depth` | `EIDOS_QUERY_QUEUE_DEPTH` | 32 | Requests allowed to wait for a slot |
| `--query-queue-wait-ms` | `EIDOS_QUERY_QUEUE_WAIT_MS` | 5000 | How long a queued request waits |
| `--search-timeout-ms` | `EIDOS_SEARCH_TIMEOUT_MS` | 30000 | Response deadline for search |
| `--operation-timeout-ms` | `EIDOS_OPERATION_TIMEOUT_MS` | 60000 | Response deadline for browse/counts/archive/maintenance |
| `--max-body-bytes` | `EIDOS_MAX_BODY_BYTES` | 1048576 | Largest accepted JSON request body |

Beyond the queue bound, or after the queue wait, a request is answered
`503` with `{"kind":"busy"}` and a `Retry-After` header. Past its deadline it
is answered `504` with `{"kind":"timeout"}`. Health, activity and index
status never enter the gate, so they stay responsive while queries saturate
it. An export takes a permit per page rather than one for its whole stream,
so a long export interleaves with interactive queries instead of holding a
slot for minutes, and `--export-concurrency` bounds how many exports stream at
once — clamped below `--max-concurrent-queries` whenever the gate has more
than one slot. Export pages never join the shared queue and yield when
interactive work is waiting, which preserves priority even with a one-slot
gate. Exports past their own bound are refused with the same `503`.

A deadline ends the *response*, not the query: a running SQLite or Tantivy
call cannot be cancelled mid-call, so the permit is released by the blocking
task when it finishes, not when the client gives up. The catalog and index
therefore never see a half-applied operation, and `GET /api/index` (and
`/api/activity`) report the difference: `admission.in_flight` counts work
still holding a permit and `admission.detached` counts the part of it whose
client has already gone — the deadline passed, or the connection went away.
Both fall back to zero as the abandoned work drains; `rejected_busy` and
`timed_out` are cumulative.

Defaults assume the loopback default bind: one browser plus a CLI on a
machine that is also scanning and extracting content. Before binding
anywhere else, note that the API still has no authentication or per-client
rate limiting, so these limits protect the process, not the deployment — the
gate is global, and any client may fill it. A remote-facing deployment needs
a fronting proxy that terminates TLS, authenticates, and applies per-client
rate limits, and should raise `--max-concurrent-queries` only as far as the
disk backing `catalog.db` can serve concurrent readers.

Content extraction runs in the service for every source whose content
policy is enabled (the default). Turn it off or bound it per source before
the first crawl of a slow or remote root:

```powershell
.\target\release\eidos.exe source content share --disable          # SMB: metadata only
.\target\release\eidos.exe source content projects --concurrency 1  # HDD: one reader
.\target\release\eidos.exe activity --watch 5                        # queues, workers, throughput
```

The same controls are on the Activity and source pages of the web UI and at
`POST /api/sources/{id}/content` / `GET /api/activity`.

Automatic reconciliation of a feedless or SMB source yields while that
source has queued or running content work, so a long rescan does not compete
with the crawl already in progress. An explicit `eidos source scan` remains
the operator override. A durable open scan generation also counts as running,
even if the current process has no in-memory progress handle for it. The
source and Activity views show `reconciliation_deferred.reason` and its
`next_eligible_at` scheduler check; the same fields are returned by
`GET /api/sources` and `GET /api/activity`. Native change-feed recovery keeps
priority over content work because restoring its checkpoint closes a live
catalog-consistency gap; it still never overlaps another scan generation.

Catalog writers also share a fair in-process gate. Enumeration commits after
at most 4,000 observed rows or 500 ms, checking those bounds between individual
entries even when one directory contains thousands of children. A waiting
content or catalog update therefore gets the next transaction turn instead of
depending on SQLite's busy timeout. The final tombstone, aggregate, source-state
and checkpoint publication remains one atomic transaction so readers cannot
observe a half-published generation. Cancelling, returning an error, or dropping
a scan session rolls its current batch back and marks the open generation
aborted; startup recovery covers the separate case where the process itself
ended before cleanup could run.

`GET /api/activity`, `eidos activity`, and the Activity page report current
waiters, total and contended acquisitions, and cumulative/max wait and hold
times. A rising maximum hold identifies a long transaction; a rising total
wait with stable maximum hold identifies sustained contention. SQLite's
five-second busy timeout is only a fallback for an unexpected writer outside
the coordinating process.

Service open also completes its durable restart repair before background
threads start. It aborts crash-open scan generations, requeues jobs left
`running`, and requeues content records left `indexing` before their Tantivy
commit. `startup_recovery` on `GET /api/activity` records all three counts for
the lifetime of the process; `eidos activity` and the Activity page render the
same summary. The recovered source's last published generation stays visible,
with a degraded reason for the interrupted scan, while queued work remains
available to the normal bounded workers.

The deterministic `restart_retention` integration test exercises one combined
fixture with published catalog/index state, committed content search, archive
virtual paths, queued/running jobs, an unfinished content publication, and an
open scan. It asserts a healthy catalog projection and content index reopen
without rebuild, then checks the exact recovery counters and Activity JSON.

### Pausing content extraction

Extraction is the one background job that reads the source volumes
continuously. `eidos content pause` stops it without stopping the service:

```powershell
eidos content pause      # stop claiming new content jobs
eidos content status     # flow, content-search state, and why
eidos content resume     # claim again
```

A pause stops **claiming**, nothing else. A batch a worker already holds runs
to completion, commits, and publishes normally, so pausing never abandons
work in flight or costs the extraction already paid for — which is why the
state right after a pause on a busy service is `draining`, not `stopped`.
Topping the queue up stops with claiming (that catalog load is part of what
an operator is pausing); commits do not, because a draining worker's output
must still reach search. The queue itself is durable and waits for the
resume.

Pause/resume transitions, worker claims, queue top-up, and scan registration
share one admission gate. This makes a completed pause authoritative: neither
a worker nor the coordinator can act on an earlier check after the response.
If reconciliation starts against a paused backlog, resuming leaves that source
idle until its scan finishes, so both readers do not hit the same disk or share
at once.

**The pause survives a restart.** It is recorded in `content-pause.json` in
the data directory and removed on resume; the file's presence is the flag.
An operator who pauses because a volume is busy should not get the load back
because the service was restarted or crashed. `eidos content resume` is the
only way back — or deleting the marker with the service stopped.
`--no-content` is the separate *process* switch: chosen at start-up, not
persisted, and it outranks a pause in what the status reports, because
resuming a `--no-content` process would change nothing. See
[ADR-0025](adr/0025-durable-operator-content-pause.md).
An active content-index rebuild likewise outranks the pause reason: resume
cannot restore claiming until the rebuild releases the writer, although the
durable pause remains visible in the status.

`content_status` on `GET /api/health` and `GET /api/activity`,
`GET /api/content/status`, `eidos activity`, and the Activity page all render
one derived value, so they cannot disagree about whether the pipeline is
`disabled`, `stopped`, `draining`, `waiting`, or `running`, or about whether
content search is `ready`, `rebuilding`, `failed`, or `disabled`. `waiting`
distinguishes "nothing queued, or every source at its concurrency budget"
from a pipeline that is actually stopped. `ActivityView.content_enabled` is
retained unchanged for compatibility and is the same bit as
`content_status.enabled`.

Transient failures (share offline, sharing violation) retry on their own with
exponential backoff. Deterministic, corrupt, unsupported, and resource-limit
failures are terminal on purpose: they come back only through an explicit
operator retry, once the extractor, the limit, or the share is fixed. A retry
covers both places a failure can live — a job left `failed` after its
transient budget ran out, and an object whose extraction failed for good
(the failure is on the content record and the job finished; the object goes
back to `pending` with a fresh content job).

```powershell
.\target\release\eidos.exe content retry --source share --preview            # how many, how many bytes
.\target\release\eidos.exe content retry --source share --class resource_limit
.\target\release\eidos.exe content retry --source share --reason-prefix "extract:" --limit 500
.\target\release\eidos.exe content retry 4213                                # one job id
```

Retrying keeps the diagnosis: `attempts`, `last_error`, and `failure_class`
stay on the job, `requeue_count`/`requeued_at` record the action, and the
automatic transient budget starts again from the requeue. A retry never
touches a running job, never creates a second active job for one object, and
skips objects that were deleted, superseded by a newer generation, retired,
or whose source has content extraction disabled — the response reports
`accepted`, `skipped`, `rejected`, and `bytes` with per-reason counts. The
Activity page has a retry button per recent failure and a two-step bulk retry
per source that shows the preview count before acting. The confirmation
carries that preview's `as_of`, count, and opaque exact-set token. Failures
that appear later wait for the next round; if an older previewed failure
becomes ineligible, the action returns `409` and asks the operator to preview
again instead of substituting a different pre-existing failure. The endpoints
are `POST /api/jobs/{id}/retry` and
`POST /api/sources/{id}/content/retry`.

### Stored-text preview

`GET /api/objects/{id}/content?generation=G&ordinal=N&before=B&after=A`
returns the text the indexer stored for one object generation — the catalog's
extraction cache, never a read of the source file, and never addressed by
filesystem path.

- `ordinal` (default 0) picks the chunk; `before`/`after` add neighbouring
  chunks, clamped to 4 per side.
- `generation` is optional. When it is supplied and does not match the
  generation the stored chunks belong to, the request fails with `409` and
  `{"kind": "stale_generation", "requested_generation", "current_generation"}`
  so a client holding an old search hit refetches instead of rendering text
  from a different version of the file. Search hits carry the value to send
  as `content.generation`.
- A successful response reports the content record's `state`, `coverage`,
  `indexed_bytes`/`total_bytes`, `reason`, each chunk's byte and line ranges,
  and `stale: true` when the object has moved on to a newer generation than
  the stored text.
- Responses are bounded: 4 neighbouring chunks per side, 256 KiB of text, and
  4000 lines. `truncated` (per response and per chunk) says when the limits
  cut something; `has_more_before`/`has_more_after` drive paging outwards. The
  requested chunk is always returned (cut down if it alone is too big) and
  neighbours only whole, so a client paging outwards steps the window rather
  than widening it once the budget binds.
- The blocking read goes through the same admission gate as browsing, so a
  burst of previews cannot starve the blocking pool.
- Text is sanitised before serialisation: C0/C1 control characters other than
  tab, CR and LF, and bidirectional overrides, become U+FFFD (one for one, so
  offsets still line up) and the chunk is flagged `sanitized`.

Searching through the running service:

```powershell
.\target\release\eidos.exe search "readme ext:md mtime:>=30d"           # exit 2 = some source incomplete
.\target\release\eidos.exe search --mode directories "has:idb has:cs" --explain
.\target\release\eidos.exe search --json "ext:dmp" | ConvertFrom-Json
```

Exporting a whole result set (streamed from the service, never buffered):

```powershell
.\target\release\eidos.exe search "ext:dmp" --export csv --out dumps.csv
.\target\release\eidos.exe search "ext:md mtime:>=30d" --export ndjson | Select-Object -First 5
```

`--out -` (or omitting `--out`) writes to stdout, `--export-limit N` lowers the
row cap, and `--bom` prefixes a UTF-8 BOM for Excel. The command prints the
match count and the cap on stderr, marking `(TRUNCATED)` when the cap bites.

`EIDOS_URL` overrides the service address. Query syntax:
[query-syntax.md](query-syntax.md).

### Interaction capture

`POST /api/interactions` records what a search presented and what happened to
it — `presented`, `opened_preview`, `opened_file`, `copied_path`, `exported` —
into the catalog's `interaction_events` table. It is data collection only:
nothing reads the table back on the search path, and no ranking, ordering, or
response depends on whether an event was recorded, refused, or dropped.

- **No query text is stored.** A client posts the query it ran; the service
  parses it, stores a digest of its canonical rendering (`query_hash`) and a
  coarse label (`query_shape`: `metadata`, `name`, `content_ranked`,
  `content_regex`), and discards the text. The digest groups the events of one
  query; it is not an anonymity guarantee, since a guessed query can be hashed
  and compared.
- **The service stamps the time.** A client clock cannot place rows outside the
  retention window.
- **Growth is bounded.** Events older than 90 days are deleted, and the oldest
  rows beyond 1,000,000 are deleted. Retention runs inside the insert
  transaction every 32nd batch and once at startup.
- **Requests are cheap and refusable.** At most 500 events per request (larger
  is `400 bad_request`); the response is returned before the write happens, and
  when too many batches are already waiting on the catalog writer the batch is
  dropped and the response says so (`{"accepted": 0, "dropped": n}`).

The web UI queues events per tab session and flushes them every two seconds and
when the page is hidden; failures are silently discarded. `eidos search` posts
one `presented` batch for the hits it printed, with a short deadline and no
retry, so scripted use never blocks. `EIDOS_NO_INTERACTIONS` turns the CLI's
capture off.

## Benchmarks

Benchmark commands are explicit and read-only. They append one JSON line per
run (`eidos-bench/1` schema, `crates/eidos-domain/src/bench.rs`) to the file
named by `--bench-out`, optionally with a full report via `--report-out`.
`bench-results/` is git-ignored; curated numbers go into
[benchmarks.md](benchmarks.md).

```powershell
# Metadata-only profile of a tree (never opens file contents)
.\target\release\eidos.exe profile D:\ --threads 16 --label D: `
    --bench-out bench-results\profile.jsonl --report-out bench-results\profile-D.json
.\target\release\eidos.exe profile $env:TEMP\some-tree --json

# Catalog import without the service (writes data\catalog.db)
.\target\release\eidos.exe source scan projects --threads 16 --bench-out bench-results\scan.jsonl

# Query latency over the catalog index; --concurrent-scan rescans a source
# while measuring. Stop `serve` first: only one process may hold the index writer.
.\target\release\eidos.exe bench search --data-dir data --iterations 30 --bench-out bench-results\query.jsonl
.\target\release\eidos.exe bench search --data-dir data --concurrent-scan projects
# One family plus ad-hoc queries; and the chunk-store cost on the candidate
# chunks a content clause actually touches (fetch serial/parallel + matcher).
.\target\release\eidos.exe bench search --data-dir data --family content-regex --query "content:/colou?r/"
.\target\release\eidos.exe bench chunks --data-dir data --threads 16 --query "content:/timed? ?out after \d+/"
```

`.cargo/config.toml` builds the bundled SQLite with a raised
`SQLITE_MAX_MMAP_SIZE` so the catalog can be memory-mapped in full
(ADR-0009); a build without it still works but chunk fetches fall back to
read calls and content verification is ≈2–3× slower.

Benchmark records carry workload labels, never file names or secrets.

## Rules for sources used in benchmarks

- Everything eidos does to a source is read-only: handles are opened with
  read-only access and full sharing, and nothing writes to a source root.
- Credentials for SMB roots are never stored in the repository or in
  configuration. Provision them in the OS (`cmdkey`, `net use`) or through an
  environment variable that names a credential rather than containing it.
- Do not run full content crawls of large personal corpora just to exercise
  scaffolding; use synthetic fixtures and the explicit benchmark commands.

## Logging

`EIDOS_LOG` (or `--log`) accepts `tracing` filter syntax, e.g.
`EIDOS_LOG=info,eidos_scanner=debug`. The default is `info,tantivy=warn`:
tantivy's segment bookkeeping is noise at INFO and was ~80% of a day's log
lines. `--log-json` emits JSON lines to stderr.

The service keeps 14 daily log files and deletes older ones.

## Layout

```text
crates/
  eidos-domain    IDs, states, query AST, result/completeness contracts, bench format
  eidos-scanner   enumeration contracts, Windows lister, parallel walker, USN journal
  eidos-catalog   SQLite catalog: migrations, scan generations, changes, aggregates, jobs, content records
  eidos-content   sniff/decode/chunk/hash
  eidos-archive   archive member inventories from container metadata (ZIP central directory)
  eidos-sync      deterministic sync simulation, fenced shipper/applier, Merkle repair
  eidos-search    Tantivy catalog index, projection follower, executor
  eidos-query     parser/renderer for the query syntax
  eidos-service   Axum API, scanner/watcher/reconciler/follower threads, composition
  eidos-cli       the `eidos` binary
web/              Vite + React + TypeScript UI
docs/             public documentation and ADRs
scripts/          check.ps1, check.sh, hooks/
```

## Conventions

- Decisions with lasting consequences are recorded as ADRs in `docs/adr/`.
- Clippy runs with warnings denied; `rustfmt` settings are in `rustfmt.toml`.
- Tests that need a real filesystem build fixtures in a temp directory and
  clean up after themselves; nothing may depend on a developer's own paths.
- User-facing capabilities ship with their web UI wiring in the same
  release. CLI/API-only is an explicit, recorded exception (ADR or release
  note saying why), not a default; see the definition of done in
  [overview.md](overview.md).
