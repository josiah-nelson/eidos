# ADR 0022: The observatory does not open the files it observes

## Status

Accepted (2026-08-26).

## Context

The USN lane records a size bucket and a directory depth for every file
closed after a write. It obtained them the obvious way: `snapshot_by_id`,
which opens the object by its reference number and reads its attributes.

That broke other software on the host. Measured here with the collector
installed and an unrelated test suite running beside it:

| collector | `eidos-search` archive tests |
|---|---|
| stopped | 8/8 pass |
| running, USN + content lanes on | 4/8 pass |
| running, content lane **off** | 4/8 pass |
| running, USN lane **off** | 8/8 pass |

The failures were `ERROR_ACCESS_DENIED` when an index writer created files it
had just written and deleted. The content probe was not involved; the
always-on L1 lane was.

Two properties made it as bad as it was. First, a handle held on a file is
never free on Windows: it can turn a concurrent delete into a delete-pending,
and the name stays occupied until the last handle closes. Full sharing flags
do not avoid this - `FILE_SHARE_DELETE` is what permits the delete, not what
releases the name, and an open requesting only `FILE_READ_ATTRIBUTES` is
exempt from sharing checks but not from this. Second, the lane asked at the
worst possible instant: facts are wanted for `REASON_CLOSE` records, so the
collector reached for the file in the moment its writer let go of it -
exactly when a build tool or an index writer is about to delete and recreate
that path.

The deeper cause is not the call. `snapshot_by_id` belongs to
`eidos-scanner`, written for the indexer, where opening files is the entire
job. Importing it into an observer silently imported the indexer's
assumptions, and nothing stopped that: the packaging even claims "Nothing
here reads or writes an observed volume", a statement that was false when it
was written and that nothing could contradict. This is the same shape as the
`Shared` lock discipline recorded in ADR-0021 - an invariant stated in prose,
with no mechanism behind it.

## Decision

**The always-on lanes never open an observed file.** Not with reduced access,
not with permissive sharing, not briefly. There is no way to hold a handle on
Windows without risking the name, so the rule is absolute rather than careful.

**Facts come from the parent directory.** `object_facts::Lookup` opens the
parent by reference number for listing only and enumerates it with
`FILE_ID_EXTD_DIR_INFO`, which returns the file id and size of every child in
one call. The parent's own path gives the depth. This costs one short-lived
handle on a directory instead of one handle per file, and a directory is not
what a churning workload deletes and recreates.

It is also cheaper. One `Lookup` per batch tracks the parents already walked,
so a build directory producing thousands of close records in one batch is
enumerated once rather than opened once per file.

**The lookup happens before the study key is taken.** A directory walk is far
longer than the attribute read it replaced, and the first version of this
change left it inside the `shared.key` lock - which stalled every thread that
needed the key and hung `observe status`. Found by running it, not by reading
it. Facts for the whole batch are now resolved before the key is locked, the
same discipline ADR-0021 applied to the content lane's sampling settings.

**The content probe remains the deliberate exception.** Opening files is its
entire purpose. It is off by default, and the cost of asking for it is
documented rather than hidden.

## Consequences

- Verified on this host: with the USN lane on and the collector recording
  (C: 41 batches / 131 records / 57 logical changes), the previously failing
  suite passes 8/8 three runs in a row.
- **Some sizes are lost.** A file deleted before its parent is walked is no
  longer listed, and its size comes back `None` where the old code would have
  had one. Short-lived build artefacts are exactly that case, so the loss
  falls hardest on the churn-heavy hosts. `size` was already optional and is
  bucketed to twelve power-of-four ranges before it is recorded; trading some
  of that fidelity for not perturbing the host is the right way round for an
  observatory, but it is a real trade and not a free one.
- A directory with more children than `MAX_ENTRIES_PER_DIRECTORY` is
  abandoned part-way rather than walked to the end, so objects in very large
  directories may have no size. The handle should not be held that long.
- The invariant is still not enforced by the compiler. `eidos-scanner`'s
  file-opening primitives remain reachable from the collector; nothing but
  this document and the tests stops the next lane from reaching for one.
  Splitting them into a crate the collector cannot depend on is the mechanism
  this ADR does not yet have.

## Testing

The class of defect is invisible in a diff - `snapshot_by_id` is correct
code, called correctly, and the failure appears in a *different process*. It
survived a Greptile pass, a Codex review, and a green suite. Every collector
test asked "did it record the right thing?" and none asked "what did it cost
the host?", so no test could have failed.

`a_churning_workload_is_never_disturbed` asks the second question. It runs
create/write/close/delete/recreate against the same paths - the pattern that
exposed this - while the fact lookup walks that directory as hard as the
reader ever would, and requires **every** workload operation to succeed. What
the lookup learns is not the assertion; what it costs is.

One trap is worth recording, because the first version of this test fell into
it. An attempt to state the invariant as "the size of a file held open
exclusively by someone else still resolves" passes on *both* implementations:
an open requesting only `FILE_READ_ATTRIBUTES` bypasses share-mode checks, so
the old code answered too. A test that cannot fail against the defect is not
a test of the invariant, however much it reads like one. The behavioural
version is the one that discriminates.

A perturbation test belongs to every lane, not to this one. The harness
should be parameterised over lane configurations so a new lane inherits the
question instead of having to remember it, and it should run against the
installed service in the installer lane, which is where it bites.
