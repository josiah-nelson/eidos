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

## Content streaming (`eidos bench content`, single file, null sink, 2026-08-22)

The extractor streams `open → sniff → decode → chunk → hash` with a 1 MiB
read buffer and at most one chunk in flight; memory does not depend on file
size.

| Fixture | Size | Encoding | Chunks | Lines | Time | Throughput | Peak working set |
|---|---:|---|---:|---:|---:|---:|---:|
| PostgreSQL log (volume G) | 2.14 GiB | UTF-8 | 138,975 | 9.45 M | 3.0 s | 732 MB/s | 8.6 MiB |
| Application XML log (volume R) | 1.05 GiB | UTF-8 | 68,875 | 45.0 M | 5.0 s | 216 MB/s | 8.6 MiB |

Both files were fully covered and hashed (BLAKE3 computed from the same
read pass); the process baseline before extraction was 5.4 MiB. Timings are
warm-cache; the XML's lower rate is its 45 M short lines (per-line
accounting), not I/O.

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

## Content crawl (service workers, volumes G + R, 2026-08-22)

`eidos serve --content-workers 4 --no-auto-reconcile`; G budget 3 readers,
R (HDD) 1 reader for the first 8.5 minutes, then 3. The two SMB shares had
content disabled. Warm metadata cache; file data mostly cold.

| Metric | Value |
|---|---:|
| candidate files (after policy exclusions) | 97,031 |
| indexed (full coverage) | 58,691 |
| sniffed as binary (`unsupported`) | 38,338 |
| failed (access denied: files held by a running VM) | 2 |
| transient retries | 0 |
| literal text read and hashed | 28.84 GiB |
| chunk documents | 1,821,820 |
| index commits | 310 |
| wall time until both sources reported `content_complete` | ≈ 17 min |
| sustained throughput on large files | 45–52 MiB/s |
| content index on disk | 7.1 GB |
| catalog growth (stored zstd chunks + records) | 1.9 GB → 8.5 GB |

Small files are I/O-bound on the HDD: with one reader R processed ≈15 tiny
extensionless files per second (its random-read rate); three readers
cleared the remaining 12 k in two minutes. Large logs stream at the
extractor's rate regardless of reader count.

## Query latency on the full catalog (4 sources, 4.15 M entries, 1.82 M chunks)

Same `eidos bench search` run as above but on the catalog with both SMB
shares loaded (30 iterations per query, release, idle machine):

| Family | Queries | p50 | p95 | p99 | max |
|---|---|---:|---:|---:|---:|
| metadata | `ext:cs` (27,660), `ext:dmp`, `size:>1G`, `mtime:>=30d ext:log`, `ext:log size:>100M`, `state:excluded` (2,154,278) | 17 ms | 87 ms | 91 ms | 98 ms |
| directory | `has:idb has:cs`, `kind:dir subtree:>1G`, `kind:dir files:>1000` | 23 ms | 31 ms | 35 ms | 35 ms |
| name | `readme` (12,306), `name:config` (31,436), `*.json` (148,482), three ranked terms, `name:=README.md` (8,484) | 80 ms | **1,878 ms** | 1,931 ms | 1,977 ms |
| regex (name/path) | `name:/postgresql-.*\.log$/` (191), `name:/^[A-Z]{3}-\d{4}/c`, `path:/\\bin\\/ ext:dll` (6,188) | 1,810 ms | **4,191 ms** | 4,198 ms | 4,258 ms |
| content (ranked/phrase) | `content:error` (234), `content:"connection refused"` (30), `content:exception ext:log mtime:>=365d` (55) | 37 ms | **54 ms** ✓ | 57 ms | 60 ms |
| content-exact | `content:=Exception` (72), `content:~localhost:` (173) | 1,454 ms | **1,502 ms** | 1,535 ms | 1,564 ms |
| content-regex | `content:/timed? ?out after \d+/` (5), `content:/[A-Z][a-z]+Exception: /c ext:log` (117) | 1,352 ms | **1,403 ms** | 1,416 ms | 1,422 ms |

Reading: ranked content retrieval meets its gate at first try. Everything
that walks a term dictionary does not scale from 125 k to 4.15 M entries:
an unanchored name/path regex or substring visits every term of the folded
dictionary (≈3 M unique names), so 40 ms became 1.9 s. Verified content
clauses are bounded by verification cost: common trigrams (`exception`)
yield far more than the 20,000-candidate cap, and fetching and checking
20 k stored chunks serially costs ≈1 s. Both are addressed next (name/path
trigram candidates with fast-field verification; parallel chunk
verification and a token pre-filter for whole-word literals). An
IPv4-shaped regex with nested repetitions also exceeded the FST automaton's
1,000-state limit and was rejected rather than executed.

## Gates status (v0.5)

| Gate | Target | Measured |
|---|---|---|
| complete local metadata scan | < 30 s | 3.5–4.5 s warm (cold not yet measured) |
| incremental metadata visibility | < 2 s | 0.5 s |
| metadata query p95 (G+R catalog) | < 50 ms | 3.9 ms (87 ms with 4.15 M entries, driven by exact counts of 2 M-hit queries) |
| selective name/path regex p95 | < 250 ms | 73 ms on G+R; 4.2 s on 4.15 M entries — being fixed |
| multi-GB files with bounded memory | required | 2.14 GiB at 8.6 MiB peak working set |
| candidate content indexed | 30 min | ≈ 17 min for 28.8 GiB |
| ordinary content query p95 | < 150 ms | 54 ms |
| selective content regex p95 | < 250 ms | 1.4 s — being fixed |
