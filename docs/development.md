# Development

Reproducible build, test, lint, and benchmark commands. Windows is the
primary development platform; the workspace also builds on other platforms
with a reduced (std-only) lister and no change feeds.

## Toolchain

| Tool | Version used | Install |
|---|---|---|
| Rust (stable, `x86_64-pc-windows-msvc`) | 1.98 (floor 1.85) | `rustup` (https://rustup.rs); components `clippy`, `rustfmt` |
| MSVC Build Tools + Windows SDK | VS 2026 Build Tools, SDK 10.0.26100 | Visual Studio Installer, "Desktop development with C++" |
| Node.js / npm | 24.x / 11.x | https://nodejs.org |

`%USERPROFILE%\.cargo\bin` must be on `PATH`.

## Commands

```powershell
# One-shot: format check, clippy (deny warnings), all tests, release build, web lint+test+build
.\scripts\check.ps1            # -SkipWeb / -SkipRelease to shorten

# Individually
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
cd web; npm ci; npm run lint; npm test; npm run build
```

Tests never touch user data: every integration test builds its own fixture
under a `tempfile::tempdir()`. USN-journal tests need an elevated session
and skip themselves otherwise.

## Running the service

```powershell
.\target\release\eidos.exe source add projects D:\Projects        # register a source
.\target\release\eidos.exe source add share \\fileserver\share     # SMB roots work too (no live feed)
.\target\release\eidos.exe serve --data-dir data --web-dir web\dist   # http://127.0.0.1:7700
```

The service scans new sources, keeps local NTFS sources live through the USN
journal, rescans feed-less sources on their reconcile interval, rebuilds the
search index whenever a source's published generation changes, and follows
the catalog outbox every 500 ms (`GET /api/index` shows follower state).

`serve` flags / environment: `--data-dir` (`EIDOS_DATA_DIR`), `--bind`
(`EIDOS_BIND`, default loopback — the API has no authentication yet and warns
when bound elsewhere), `--web-dir` (`EIDOS_WEB_DIR`, empty for API only),
`--scan-threads`, `--no-auto-reconcile`, `--no-content` (metadata only),
`--content-workers N` (`EIDOS_CONTENT_WORKERS`, default 4), plus the
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
it.

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

Searching through the running service:

```powershell
.\target\release\eidos.exe search "readme ext:md mtime:>=30d"           # exit 2 = some source incomplete
.\target\release\eidos.exe search --mode directories "has:idb has:cs" --explain
.\target\release\eidos.exe search --json "ext:dmp" | ConvertFrom-Json
```

`EIDOS_URL` overrides the service address. Query syntax:
[query-syntax.md](query-syntax.md).

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
`EIDOS_LOG=info,eidos_scanner=debug`. `--log-json` emits JSON lines to stderr.

## Layout

```text
crates/
  eidos-domain    IDs, states, query AST, result/completeness contracts, bench format
  eidos-scanner   enumeration contracts, Windows lister, parallel walker, USN journal
  eidos-catalog   SQLite catalog: migrations, scan generations, changes, aggregates, jobs, content records
  eidos-content   sniff/decode/chunk/hash
  eidos-archive   archive member inventories from container metadata (ZIP central directory)
  eidos-search    Tantivy catalog index, projection follower, executor
  eidos-query     parser/renderer for the query syntax
  eidos-service   Axum API, scanner/watcher/reconciler/follower threads, composition
  eidos-cli       the `eidos` binary
web/              Vite + React + TypeScript UI
docs/             public documentation and ADRs
scripts/          check.ps1
```

## Conventions

- Decisions with lasting consequences are recorded as ADRs in `docs/adr/`.
- Clippy runs with warnings denied; `rustfmt` settings are in `rustfmt.toml`.
- Tests that need a real filesystem build fixtures in a temp directory and
  clean up after themselves; nothing may depend on a developer's own paths.
