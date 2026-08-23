# ADR-0008: Page-driven verification for content clauses

Status: accepted (amends ADR-0005 and ADR-0006)  
Date: 2026-08-22  
Milestone: 4

## Context

ADR-0005 verifies every substring, exact, and regex content clause against
the catalog's stored chunk text: trigram candidates come from the content
index, each candidate chunk is fetched from SQLite and run through the
matcher, and only then are the surviving objects joined with the rest of the
query. On the full catalog (1.82 M chunks) a fetch costs ≈70 µs and SQLite
readers scale only ≈2× across threads on the reference host, so a clause
with 6.7 k true-match chunks cost ≈540 ms and one that reached the 20 k
candidate cap ≈650 ms — against a 250 ms gate — regardless of how many
files the page would show or how selective the metadata clauses were.

ADR-0006 solved the same shape for name and path clauses by verifying
candidates lazily in sort order. This record applies that idea to content.

## Decisions

- A verified content clause whose candidate set is at most 2,000 chunks is
  still verified eagerly: every chunk is fetched, totals are exact, and the
  cost is bounded (≈70 ms on the reference host).
- Above that the clause is **deferred**. Candidates are grouped per object
  (newest generation wins) and the object set is handed to the catalog-index
  query unverified, so metadata clauses (`ext:`, `mtime:`, scope) filter it
  before any chunk is fetched. The executor then walks the candidate rows in
  result order and verifies each object only as far as the page needs.
- Verifying one object fetches its candidate chunks in growing batches and
  stops as soon as eight of them match, every candidate is examined, or the
  budget runs out. A verified clause's object score is the number of
  matching chunks capped at eight, on both paths, so eager and page-driven
  results rank alike; the earlier "matches within a chunk" weighting is
  gone. Fetched rows are kept for the page's snippets.
- For relevance order, rows are walked by descending score *bound* — the
  capped candidate count — and the walk stops once the next bound cannot
  beat the `want`-th best verified score. The page order is therefore exact,
  not approximate. Other sort orders walk in key order as names do.
- One query may fetch at most 20,000 chunks during page-driven verification.
  Reaching that budget truncates the result with a warning ("results are a
  subset — narrow the query"); the candidate cap itself rises to 100,000
  chunks because collecting candidates is cheap.
- As for names, every hit returned is verified; the total and facet counts
  are upper bounds (`exact = false`, "N+") unless the walk reaches the end,
  and the response says so.
- Deferral applies only to clauses in positive `AND` context. Inside `OR`
  or `NOT` a content clause is verified eagerly under the fetch budget: a
  set of unverified candidates is not a thing that can be negated.
- Verification of a batch of objects runs on up to eight threads; the fetch
  budget is shared across them.

## Consequences

Measured on the full catalog (4 sources, 4.15 M entries, 1.82 M chunks), 30
iterations, release build, idle machine (`eidos bench search`):

| Query | before p95 | after p95 | total reported |
|---|---:|---:|---|
| `content:~localhost:` (6,681 matching chunks) | 536 ms | **123 ms** | 239+ (173 exact before) |
| `content:/[A-Z][a-z]+Exception: /c ext:log` | 655 ms | **78 ms** | 350+ (55 exact before) |
| `content:/timed? ?out after \d+/` | 655 ms | 532 ms | 5, subset (budget reached) |

- A clause whose candidates are mostly true matches costs the page's worth
  of fetches (≤ 8 per file shown) plus candidate collection, instead of the
  whole candidate set: 4–8× faster here, and independent of how many files
  match.
- A regex whose required literals are weak (`out after`) still has to fetch
  candidate chunks to reject them; with five true files among tens of
  thousands of candidates it spends the fetch budget and reports a subset,
  at about the previous cost and with the same five hits. The remaining
  lever for that family is a faster chunk store, not smarter ordering.
- The price of the speed is the inexact total ("239+") whenever the walk
  stops early — the same trade ADR-0006 made for names.
- Facet counts over a deferred clause include unverified candidates, as
  they already did for lazily verified name clauses; the warning names both.
- `ContentOpts` gains `max_candidates` and `lazy_min`; `max_verify` now
  means the fetch budget. Tests force the page-driven path with
  `lazy_min = 0` on the synthetic fixture and compare it with the eager
  path clause by clause.
