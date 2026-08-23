# eidos

A Windows-first, eventually cross-platform filesystem catalog, content
indexer, storage analyzer, and search service.

*eidos* (εἶδος, "form") is the thing itself as distinct from where it happens
to sit: the project keeps a canonical catalog of file **objects** with stable
identity, and treats paths, search indexes, aggregates, hashes, and snippets as
rebuildable projections of that catalog.

> **Status: early development.** The first release target (v0.5, a
> trustworthy single-Windows-host indexer) is partly implemented: the
> catalog, native NTFS enumeration and USN change feed, catalog search, and
> the web UI are working; streaming literal-text content indexing is in
> progress. There is no packaged release yet, and schemas, APIs, and the query
> syntax can still change. See [docs/roadmap.md](docs/roadmap.md).

## What it is for

Existing tools each solve part of the problem: Everything is superb at names
and metadata on one Windows host, dtSearch at content, WinDirStat and friends
at size. eidos combines them behind one catalog and one query language:

- **Everything-class responsiveness** for names and metadata: batched native
  NTFS enumeration, the USN journal for live changes (sub-second visibility),
  and a Tantivy catalog index with millisecond metadata queries.
- **Trustworthy catalog**: object identity is separate from path identity, so
  renames, moves, and hard links never look like new content; scans publish
  atomically; interrupted or offline sources are preserved, never silently
  emptied; every search response states per-source completeness.
- **Storage analytics**: apparent and allocated subtree sizes, file counts,
  and sparse per-directory extension counts, maintained incrementally; a
  treemap and tree browser in the web UI.
- **Directory-aware search**: directories are first-class results, with
  predicates over descendants (`has:idb has:cs`, `files:>1000`,
  `subtree:>1G`) and a documented ranking rule.
- **Exact and lexical search**: case-insensitive by default, case-sensitive
  exact and substring modes verified against original text, glob, and regex
  over names and paths; literal-text content search with line-aware snippets
  (in progress).
- **Generic SMB crawling** with explicit, weaker freshness semantics for
  roots that have no change feed.
- **Web UI, CLI, and HTTP API** sharing one typed query AST.

Planned later: ZIP member inventory, multi-host agents with offline
preservation, macOS and Linux agents, recursive archives and Office/PDF text,
exact directory copies and diffs, MCP tools, natural-language query
compilation, and directory similarity. See the roadmap for sequencing and the
non-goals of each release.

## Quick start (Windows)

Prerequisites: Rust stable (MSVC target), Visual Studio Build Tools with the
Windows SDK, Node.js 24+. Details in [docs/development.md](docs/development.md).

```powershell
cargo build --release
cd web; npm ci; npm run build; cd ..

# Register a source and run the service (API + web UI on http://127.0.0.1:7700)
.\target\release\eidos.exe source add projects D:\Projects
.\target\release\eidos.exe serve --data-dir data --web-dir web\dist

# In another shell: search through the running service
.\target\release\eidos.exe search "readme ext:md mtime:>=30d"
.\target\release\eidos.exe search --mode directories "has:idb has:cs" --explain
.\target\release\eidos.exe search --json "ext:dmp size:>100M"
```

The service scans registered sources, keeps NTFS sources live through the USN
journal, rescans feed-less sources periodically, and serves the web UI. The
CLI exits with status 2 when any requested source was incomplete, so scripts
never mistake a partial answer for a complete one.

Everything eidos does to a source is read-only.

## Documentation

| Document | Contents |
|---|---|
| [docs/overview.md](docs/overview.md) | Goals, product thesis, representative scenarios, functional and reliability requirements |
| [docs/architecture.md](docs/architecture.md) | Invariants, identity model, catalog and search architecture, scheduler, evolution to a fleet |
| [docs/roadmap.md](docs/roadmap.md) | Releases, milestones, acceptance gates, non-goals |
| [docs/query-syntax.md](docs/query-syntax.md) | The query language accepted by the UI, CLI, and API |
| [docs/development.md](docs/development.md) | Toolchain, build/test/lint commands, benchmarks, layout |
| [docs/releasing.md](docs/releasing.md) | Signed Windows release workflow and Azure Artifact Signing configuration |
| [docs/benchmarks.md](docs/benchmarks.md) | Measured results on the reference corpus |
| [docs/adr/](docs/adr/) | Architecture decision records |

## Layout

```text
crates/
  eidos-domain    IDs, states, query AST, result/completeness contracts, bench format
  eidos-scanner   enumeration contracts, Windows lister, parallel walker, USN journal
  eidos-catalog   SQLite catalog: scan generations, change application, aggregates, jobs
  eidos-content   bounded sniffing, decoding, line-aware chunking, BLAKE3
  eidos-search    Tantivy catalog index, projection follower, query executor
  eidos-query     query syntax parser and renderer
  eidos-service   Axum HTTP API, watchers, reconciler, composition
  eidos-cli       the `eidos` binary
web/              Vite + React + TypeScript UI
docs/             public documentation and ADRs
```

## Contributing

The project is developed in the open but is still at the stage where the
architecture is being proven milestone by milestone; expect churn. Bug
reports with reproducible synthetic fixtures are welcome. Run
`.\scripts\check.ps1` before opening a pull request — it runs formatting,
clippy with warnings denied, all tests, and the web build.

## License

eidos is free software: you can redistribute it and/or modify it under the
terms of the [GNU Affero General Public License](LICENSE) as published by the
Free Software Foundation, either version 3 of the License, or (at your
option) any later version. If you run a modified version as a network
service, the AGPL requires you to offer its source to the users of that
service.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you shall be licensed as above,
without any additional terms or conditions.
