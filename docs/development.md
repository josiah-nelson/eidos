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
# One-shot: format check, clippy (deny warnings), all tests, release build, web lint+build
.\scripts\check.ps1            # -SkipWeb / -SkipRelease to shorten

# Individually
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
cd web; npm ci; npm run lint; npm run build
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
`--content-workers N` (`EIDOS_CONTENT_WORKERS`, default 4).

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
```

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
