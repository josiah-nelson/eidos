# ADR-0001: Implementation stack and workspace layout

Status: accepted  
Date: 2026-08-22  
Milestone: 0

## Context

ARCHITECTURE.md section 3 proposes Rust, SQLite WAL, Tantivy, BLAKE3, Axum,
and a TypeScript/React web UI, and asks that concrete libraries be verified
against current documentation at implementation time. Section 4 suggests a
crate layout but warns against creating crates merely to mirror the diagram.

## Decision

### Versions verified on 2026-08-22 (crates.io / npm)

| Component | Choice | Version | Notes |
|---|---|---|---|
| Toolchain | Rust stable, MSVC target | 1.98.0 | `rust-version = 1.88` floor |
| Async runtime | tokio | 1.53 | service coordination only; filesystem and parsing use dedicated blocking pools |
| HTTP | axum + tower-http | 0.8 / 0.6 | tower-http 0.6 line is the one paired with axum 0.8 |
| Catalog | rusqlite (bundled SQLite) | 0.40 | WAL, single writer, read pool |
| Lexical search | tantivy | 0.26 | custom trigram tokenizer for substring/regex candidates |
| Hashing | blake3 | 1.8 | hashed while streaming content |
| Windows API | windows-sys | 0.61 | raw bindings; no `windows` crate to keep compile times low |
| CLI | clap (derive) | 4.6 | |
| Logging | tracing + tracing-subscriber | 0.1 / 0.3 | JSON lines optional |
| Web | Vite 8, React 19, TypeScript 6, TanStack Query 5, TanStack Virtual 3, react-router 8 | | `oxlint` from the Vite template |

### Workspace layout

Eight crates, fewer than the architecture diagram's twelve:

- `eidos-domain` merges "domain" and the query AST (the AST *is* domain
  contract and has no backend dependencies).
- `eidos-scanner` merges "scanner-core" and "scanner-windows"; platform code is
  `#[cfg(windows)]`-gated inside one crate. A separate macOS/Linux crate can
  be split out in v1/v1.5 if dependency isolation becomes concrete.
- `eidos-catalog`, `eidos-content`, `eidos-search`, `eidos-query`, `eidos-service`,
  `eidos-cli` as proposed. `archive` and `scheduler` live inside `eidos-content`
  and `eidos-service` respectively until they have independent dependency sets.

### Enumeration primitive

The generic Windows lister uses
`GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)` rather than
`FindFirstFileEx`. One call returns up to 64 KiB of entries including the
128-bit file ID, allocation size, change time, and reparse tag, which
`FindFirstFileEx` does not provide. The lister falls back to
`FileIdBothDirectoryInfo` (64-bit IDs, weak confidence) and then
`FileFullDirectoryInfo` (no IDs) for servers that reject the richer class,
and records which class succeeded so identity confidence is explicit.

### Timestamps

All timestamps are `i64` nanoseconds since the Unix epoch (`UnixNanos`).
Windows `FILETIME` converts losslessly; the type sorts and diffs as a plain
integer in SQLite and Tantivy.

### Benchmark format

`eidos-bench/1`: one JSON object per line with `name`, `target` label,
`metrics` (f64) and `counters` (u64) maps, build info, and host. Targets are
labels like `G:`; secrets and user file names never go into records.

## Consequences

- `cargo build` on a clean machine needs only rustup and the MSVC Build
  Tools; SQLite is bundled, and no C++ search library is required.
- The extended-length path prefix `\\?\` is applied to every native call so
  long paths work without manifest settings; display code strips it.
- Adding a Linux/macOS scanner means a new `cfg` module or a new crate; the
  `DirectoryLister` trait and `RawEntry` contract stay unchanged.
