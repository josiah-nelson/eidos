# Benchmarks

Measured results on the reference corpus. Numbers are curated from the
git-ignored `bench-results/*.jsonl` records produced by the commands in
[development.md](development.md). Dates are absolute; all runs were read-only.

## Reference corpus

| Name | Medium | Dirs | Files | Logical bytes | Notes |
|---|---|---:|---:|---:|---|
| Volume G | NTFS, SAS SSD, nearly full | 6,866 | 54,810 | 3.42 TiB | dominated by VM disks (≈3.35 TB in 14 VHDX) and archives; ≈25 k `.cs`, 2 k `.json`, 1.5 k `.log`; a 2.29 GB log and a 130 MB JSON file serve as streaming fixtures |
| Volume R | NTFS, 15K SAS HDD | 1,761 | 61,314 | 53.6 GiB | many small files per byte; 34 k extensionless files, ≈5 k files under `node_modules`; a 1.13 GB XML file and many 90–140 MB text listings |
| SMB share C | NVMe system volume over SMB2 | 415,913 | 2,223,603 | 1.27 TiB | a busy daily-driver workstation; WIM-backed WinSxS with ≈105 k hard-linked objects |
| SMB share D | NVMe data volume over SMB2 | 198,353 | 1,279,654 | 1.14 TiB | ≈172 k hard-linked objects |

Extension-based estimate of literal-text candidates on G+R: ≈52.9 k files,
≈26 GB. The 26 GB figure is the basis of the v0.5 content-indexing gate.

Host: Windows Server 2025, 36 logical CPUs, release build. Enumeration and
scan timings below are warm-cache; a cold-cache measurement is still owed.

## Enumeration only (`eidos profile`)

| Target | Entries | Walk time | Entries/s |
|---|---:|---:|---:|
| Volume G | 61,676 | 48.8 ms | 1.26 M |
| Volume R | 63,075 | 26.1 ms | 2.42 M |

## Catalog scans (2026-08-22)

| Target | Path | Entries | Scan time | Note |
|---|---|---:|---:|---|
| Volume G | CLI generic import, 16 threads | 61,675 | 1.71 s | initial |
| Volume R | CLI generic import, 8 threads | 63,074 | 1.68 s | initial |
| Volume G | CLI rescan | 61,675 | 2.11 s | idempotent |
| Volumes R + G | service native sequence, concurrent | 63,074 / 61,675 | 3.51 s / 4.50 s | USN checkpoint established, 0 records replayed |
| Volume G | rescan concurrent with query benchmark | 61,675 | 2.2 s | see query table |

Catalog size for G+R: 44 MB (≈350 B/entry). Search index: 124,751
documents, built in 0.9 s (G) + 0.8 s (R).

### SMB (generic crawler, read-only)

| Share | Entries | Logical | Errors | Scan time | Entries/s |
|---|---:|---:|---:|---:|---:|
| D | 1,480,757 | 1.14 TiB | 0 | 55.9 s | 26.5 k |
| C | 2,639,516 | 1.27 TiB | 0 | 162.1 s | 16.3 k |

`FileIdExtdDirectoryInfo` worked over SMB2 (128-bit IDs, no fallback);
identities were verified consistent (max 41 links on one DLL, no zero IDs).
Catalog after all four sources: 1.87 GB.

## Change feed (synthetic fixture on a local NTFS temp volume)

| Metric | Value |
|---|---|
| file create → catalog visible | 510 ms (500 ms poll interval) |
| restart catch-up (2 changes made while stopped) | 52 ms |
| journal overflow → reconcile → live again | < 15 s |

## Query latency (`eidos bench search`, G+R catalog index, 30 iterations per query)

| Family | Queries | p50 | p95 | p99 | max | p95 during concurrent rescan of G |
|---|---|---:|---:|---:|---:|---:|
| metadata | `ext:cs` (24,942 hits), `ext:dmp`, `size:>1G`, `mtime:>=30d ext:log`, `ext:log size:>100M`, `state:excluded` (17,748) | 1.0 ms | 3.9 ms | 4.3 ms | 5.7 ms | 4.6 ms (p99 7.6) |
| directory | `has:idb has:cs`, `kind:dir subtree:>1G`, `kind:dir files:>1000` | 3.1 ms | 5.0 ms | 6.2 ms | 7.4 ms | 4.8 ms |
| name | `readme`, `name:config` (3,209), `*.json` (5,802), a three-word ranked term query, an exact case-sensitive `name:=` | 0.7 ms | 40 ms | 47 ms | 48 ms | 52 ms (p99 109) |
| regex | an anchored IP-like `name:/^…/`, `name:/postgresql-.*\.log$/` (141), `name:/^[A-Z]{3}-\d{4}/c`, `path:/\\bin\\/ ext:dll` (556) | 36 ms | 73 ms | 80 ms | 95 ms | 77 ms |

Name substring/glob and regex cost is the automaton walk over the folded
term dictionary (≈60 k names / ≈125 k paths); exact, term, and range queries
are sub-millisecond. The HTTP round trip adds ≈1 ms (`eidos search "ext:dmp"`
end-to-end: 3.4 ms including completeness and catalog joins).

An early run showed a flat ≈40 ms floor per query caused by grouped object
counts in the completeness join; completeness now reads the root aggregate
(O(1)).

## Gates status (v0.5)

| Gate | Target | Measured |
|---|---|---|
| complete local metadata scan | < 30 s | 3.5–4.5 s warm (cold not yet measured) |
| incremental metadata visibility | < 2 s | 0.5 s |
| metadata query p95 | < 50 ms | 3.9 ms |
| selective name/path regex p95 | < 250 ms | 73 ms |
| content query p95 / 26 GB in 30 min | < 150 ms / 30 min | not yet measured (milestone 4) |
