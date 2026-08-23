# ADR-0011: Committing content deletions and rejecting superseded generations

Status: accepted (amends ADR-0005 and ADR-0008)  
Date: 2026-08-23  
Milestone: 4

## Context

ADR-0005 reindexes an object by queueing a deletion of every chunk document
it owns and then adding the documents of the new generation, and the commit
coordinator commits "every 2 s or 20,000 documents". Both triggers were
measured from a counter of *documents added*, so a reindex that produces no
documents left the writer looking idle: a text file that turned binary,
became empty, disappeared, or failed extraction deterministically queued a
deletion that nothing committed. Its old text stayed searchable until some
unrelated file happened to be indexed — indefinitely, on a catalog that had
finished its first pass.

The same class of leftover is visible on the read side. ADR-0005 records that
"hits whose object generation moved past the retrieved generation get no
snippets rather than wrong ones", but the hit itself was still returned: the
executor composed content candidates into file hits and only then compared
generations while choosing snippets. The result was a snippet-less
false-positive — a file returned for text it no longer contains, with nothing
to show for it — for the whole window between a file changing and its
re-extraction, and for any leftover document a missed deletion left behind.

## Decisions

- The content index tracks *writer mutations* since the last commit, not
  documents added. Every mutation — an add, an object deletion, a source
  deletion, `delete_all_documents` — marks the writer dirty
  (`ContentIndex::is_dirty`), and the commit coordinator's "is there
  anything to commit" test asks that instead of the document counter. The
  document counter still drives the 20,000-document threshold, since only
  adds grow a segment. A failed commit restores both counters, so the writer
  stays dirty and the next round retries rather than going quiet.
- Publication is unchanged and stays idempotent: only the *commit* trigger
  moved. A deletion-only reindex publishes through the same path it always
  did (`store_content` with the terminal outcome), and `mark_content_indexed`
  still ignores anything not left `indexing`, so a restart or a retry after a
  crash re-runs the same work to the same end state.
- A content candidate whose generation is not the object's current generation
  in the catalog cannot produce a file hit. The executor drops such
  candidates where the clause's object set is built — before that set reaches
  the catalog-index query — so stale objects are absent from hits, totals,
  and facet counts alike, on both the eager path and the page-driven path of
  ADR-0008 (whose deferred candidates are grouped per generation too). An
  object that has left the catalog is treated the same way. The plan step
  reports how many objects were dropped.

## Consequences

- A file that stops being indexable text loses its old content from the index
  within one commit interval, like any other reindex.
- Content clauses no longer match a file between its change and its
  re-extraction; `state:stale` and the completeness counters remain the way
  to see that the file is waiting. This is the same trade the snippet rule
  already made, applied to the hit itself.
- Validating candidates costs one catalog statement per content clause (a
  `json_each` join on the primary key), over the objects — not the chunks —
  the clause retrieved.
- Candidates are cut to `top_k` / `max_candidates` before their generation is
  known, so a truncated list can keep a file's superseded chunk and drop its
  current one, and the file is then absent. Truncation already reports the
  result as a subset; when a stale candidate was dropped from a truncated
  list the response also says so, naming the second cause. Returning the file
  instead would mean returning it for text it no longer contains, which is
  the defect this record removes.
- Correctness no longer depends on the deletion having been committed: a
  leftover document from an earlier generation is inert because the
  generation check rejects it.
