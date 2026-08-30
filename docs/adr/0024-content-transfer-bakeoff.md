# ADR-0024: Content transfer ships metadata-only in v0.5; FastCDC ~64 KiB is the one strategy to build next

Status: accepted
Date: 2026-08-29
Milestone: v0.5 dogfood-fleet sprint (track D)

## Context

Metadata replication is the v0.5 release floor. Content transfer was an
experimental slice whose strategy the sprint required to be selected by
measurement, with two independent decisions - protocol batch framing and
content payload reuse - and an explicit rule that ambiguous results do not
justify carrying several production strategies
([sprint section 6](../v0.5-dogfood-fleet-sprint.md#6-content-and-chunking-bakeoff)).

`eidos bench chunking` (`eidos-fleet::bakeoff`) measures four strategies -
whole compressed content, fixed 64 KiB chunks, FastCDC 16 KiB (4-64 KiB),
FastCDC 64 KiB (16-256 KiB), all zstd level 3, chunk identity BLAKE3, a
40-byte manifest entry per chunk - over deterministic synthetic fixtures:
identical files on two hosts, append, prepend, localized insertion,
deletion and replacement, truncation, complete rewrite, a 64 MiB first ship,
an incompressible binary with a 4 KiB edit, sparse content, already
compressed input, and fifty offline edits before one catch-up. For every
strategy it records source, compressed and transferred bytes, reused versus
novel bytes, hashing/chunking/compression/staging/apply CPU, the largest
buffer held, durable staging, chunks and frames per version (4 MiB frames),
and the bytes resent after an interruption at half the transfer. The
machine-readable report is `bench-results/chunking-bakeoff.json` (private
measurements directory); the summary below is the run of 2026-08-29 on the
development host, release build.

| strategy | scenarios | source MiB | wire MiB | ratio | cpu ms | frames | recovery MiB |
|---|---:|---:|---:|---:|---:|---:|---:|
| whole_compressed | 13 | 151.4 | 37.2 | 0.246 | 950 | 18 | 37.2 |
| fixed_64k | 13 | 151.4 | 19.7 | 0.130 | 1747 | 16 | 9.7 |
| cdc_16k | 13 | 151.4 | 17.9 | 0.118 | 2576 | 16 | 8.9 |
| cdc_64k | 13 | 151.4 | 17.6 | 0.116 | 1412 | 16 | 8.6 |

Per scenario, the shapes that decide it:

| scenario | whole MiB | fixed 64K MiB | cdc 16K MiB | cdc 64K MiB |
|---|---:|---:|---:|---:|
| identical on two hosts | 1.59 | 0.00 | 0.01 | 0.00 |
| append / prepend | 1.64 / 1.64 | 0.06 / 0.06 | 0.06 / 0.06 | 0.09 / 0.05 |
| localized insertion / deletion | 1.59 / 1.59 | 1.01 / 1.01 | 0.01 / 0.01 | 0.07 / 0.07 |
| localized replacement | 1.59 | 0.02 | 0.02 | 0.05 |
| complete rewrite | 1.59 | 1.61 | 1.63 | 1.58 |
| 64 MiB first ship | 12.76 | 12.89 | 13.10 | 12.65 |
| binary, 4 KiB edit | 8.00 | 0.07 | 0.04 | 0.03 |
| already compressed | 2.87 | 2.87 | 2.88 | 2.87 |
| fifty offline edits, one catch-up | 1.59 | 0.08 | 0.09 | 0.10 |

Time from local publication to central search visibility and per-host
storage amplification are measured by the fleet soak, not by this harness.
The approved read-only corpora were not sampled: the harness accepts a
`--corpus` directory, but no bounded sample was run for this record.

## Decisions

1. **v0.5.0 ships metadata-only fleet replication.** Central results report
   content as `ContentNotReplicated`; files replicated from a node carry
   `content_state = not_replicated`; content queries degrade the coverage
   envelope with a warning and a remediation pointing at the origin node.
   No content bytes, chunk manifests, or chunk stores exist on the wire or
   in the central catalog.

   The bakeoff is not ambiguous about strategy, so this is not the
   "ambiguous results" clause. It is a scope decision: a content path
   needs a chunk store, manifest exchange, staging with a ceiling,
   reassembly, and central indexing, and none of that has been built or
   soaked in this sprint. Shipping an unsoaked content path under a
   "trustworthy" release promise is worse than a truthful boundary.

2. **The single strategy to implement after v0.5 is FastCDC with a 64 KiB
   target (16-256 KiB), zstd per chunk, BLAKE3 chunk identity.** On every
   edit shape it is within a few percent of the best and it is the cheapest
   chunked strategy in CPU (1.4 s versus 2.6 s for 16 KiB over the same
   151 MiB) with the fewest chunks and manifest entries. Fixed 64 KiB
   chunks lose the localized insertion/deletion cases (1.01 MiB versus
   0.01-0.07 MiB) because a shift invalidates every later boundary. Whole
   content is only competitive on complete rewrites, first ships, and
   already-compressed inputs, where every strategy ships everything.

3. **Batch framing stays row- and byte-bounded** (2 000 rows, 4 MiB,
   halving on overflow), which the bakeoff shows produces the same frame
   counts across chunked strategies; no time-bounded flush is added, the
   session's one-second tick already bounds latency.

4. **Alternative strategies do not become schema variants.** The bakeoff
   harness remains a benchmark; `Strategy::Fixed64K` and `Cdc16K` exist
   only there.

## Consequences

- Central search over a node's files answers metadata questions completely
  and says, per source, that content was not consulted here.
- The next content slice has a fixed shape: FastCDC 64 KiB chunks, a
  central chunk store keyed by BLAKE3, exists-check before transfer,
  bounded staging released on durable apply, reassembly of the logical
  extracted content, and indexing through the ordinary content pipeline.
  Its economics on real corpora are to be recorded with `--corpus` over a
  bounded read-only sample before it ships.
