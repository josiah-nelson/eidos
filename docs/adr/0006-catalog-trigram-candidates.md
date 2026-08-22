# ADR-0006: Trigram candidates and fast-field verification for name and path clauses

Status: accepted (amends ADR-0004)  
Date: 2026-08-22  
Milestone: 4

## Context

ADR-0004 ran name/path substring, glob, and regex clauses as automaton
walks over the folded term dictionary and measured 40–75 ms p95 on the
125 k-entry G+R catalog. With the two SMB shares loaded (4.15 M entries,
≈3 M distinct names) the same queries took 1.9–4.2 s: an unanchored
automaton visits every term, so cost grows linearly with the dictionary.
An IPv4-shaped regex also exceeded the automaton's 1,000-state limit and
could not run at all. The collect-all path additionally fetched every
candidate from the document store before verifying or sorting, which costs
tens of microseconds per document.

## Decisions

- The catalog index (schema 2) gains `name_tri` and `path_tri`: folded
  character trigrams of the entry name and of the full path, doc ids only —
  the same tokenizer the content index uses.
- Substring clauses retrieve candidates as the `AND` of their trigrams;
  regexes use the trigrams of their required literals (the same HIR
  analysis as content regexes); path globs use the literal runs between
  wildcards. Literals shorter than three characters keep the automaton.
- Verification of case-insensitive clauses reads the `name_folded` /
  `path_folded` fast fields; no stored document is fetched. Case-sensitive
  clauses add a second verification against the stored original, as before.
- The collect-all path now reads sort keys (folded name and path, sizes,
  times, ids) from fast fields, verifies, sorts, and fetches stored
  documents for the requested page only. Tight-directory ranking and stored
  verifiers still fetch all survivors, because they need ancestors or the
  original text.
- Fallback chain for regexes with no usable literal: dictionary automaton
  (flagged as broad) → when the automaton is too complex, a full scan of the
  scope verified on fast fields (flagged).
- Verifiers apply only to clauses in positive `AND` context. Inside `OR`
  or `NOT`, substring/glob/regex compile to exact automaton queries (slower,
  always correct), and clauses that cannot be expressed without
  verification — case-sensitive modes, `in:` with a depth, `has:` with a
  count — are rejected with an explanation. Previously they were silently
  wrong there.
- `*.ext` globs compile to the extension filter.
- Verification work is parallel: fast-field passes and stored-document
  fetches run on up to 8 threads, as does chunk verification (reader pool
  of 12 connections).
- Regexes the FST automaton cannot compile (nested repetitions such as an
  IPv4 shape) walk the folded dictionary with the `regex` crate and match
  the resulting terms exactly, instead of scanning documents under a
  candidate cap that could drop matches.
- Content index schema 2 adds `text_cs`, the same tokenisation without case
  folding. A whole-word literal that is a single alphanumeric token
  (`content:=Tq`-style identifiers — the Q-2 case) is an exact term query
  on `text_cs` (or on `text` when case-insensitive) with no verification at
  all; "whole word" therefore means alphanumeric boundaries, matching the
  tokenizer. Multi-word literals keep trigram candidates plus verification.
- An empty content index (schema change, lost directory) is rebuilt from the
  catalog's stored chunks in a background thread — no source file is read —
  and every search reports content as incomplete with a warning until the
  rebuild commits. Likewise a recreated catalog index forces a rebuild of
  every source even when the catalog's projection record still shows the
  current generation (which previously left an empty index looking
  synchronised).
- Explain steps report candidate-selection and verification timings
  separately.

## Consequences

- The index grows by the two trigram fields (paths contribute most); a
  schema-version bump rebuilds every source from the catalog at the next
  service start.
- Common substring queries cost roughly the fast-field pass over their
  candidates (a few microseconds each) instead of a dictionary walk plus
  store fetches; see `docs/benchmarks.md` for the measured effect.
- `OR`/`NOT` around text clauses are now strictly exact or explicitly
  unsupported; users see the restriction rather than wrong results.
