# ADR-0017: macOS enumeration and volume facts

Status: accepted
Date: 2026-08-25

## Context

The scanner had one native adapter (Windows, `FileIdExtdDirectoryInfo` plus
volume capability flags) and a portable `readdir` + `lstat` fallback used
everywhere else. Running the agent on macOS therefore meant losing allocation
size, change time, and every volume capability: `volume_info` returned an
empty record with `DriveType::Unknown`, and each child cost a syscall.

The roadmap's macOS agent needs enumeration and identity before it can have a
change feed, and the surrounding contracts have to describe two platforms
without pretending they are the same one. Two contract-level assumptions had
hardened in the meantime:

- `supports_usn` was the only signal for "this volume has a native change
  feed", and `is_native_local()` was defined as NTFS/ReFS with a journal.
- Source classification (`windows_local` / `windows_generic` / `smb`) was
  duplicated in three call sites, each hard-coding Windows kinds.

## Decision

**Enumerate with `getattrlistbulk(2)`.** One syscall returns name, object
type, device, file id, all four timestamps, BSD flags, permissions, mount
status, allocation size, and data-fork length for a batch of children. The
requested attribute list is packed positionally, so the adapter parses common
attributes by position (`FSOPT_PACK_INVAL_ATTRS` reserves a slot for every
one), and the directory and file groups by the mask in
`ATTR_CMN_RETURNED_ATTRS`, because the kernel only packs the group that
applies to an object's type. A group that does not parse cleanly falls back to
the portable lister for that directory instead of trusting shifted values.

**Use the portable lister on remote volumes.** macOS SMB restarts a directory
enumeration when its entry cache is refilled (Apple FB15497909), so a bulk
listing can repeat entries indefinitely; Apple's own measurements also make
`readdir` the faster choice there. The bulk path is therefore gated on
`MNT_LOCAL`.

**Record macOS facts as themselves.** Allocation size covers all forks and can
be smaller than the logical size on a compressed or cloned file, so `SPARSE` is
never inferred from a short allocation. Mount points are surfaced the way
Windows volume mount points are — a directory carrying the reparse attribute,
recorded but not descended into — while firmlinks stay traversable, so the data
volume is reached exactly once through `/Users` rather than twice through
`/System/Volumes/Data`. Dataless (cloud placeholder) objects carry `OFFLINE`
and `RECALL_ON_DATA_ACCESS`, which is what the existing content policy already
uses to refuse a read that would trigger a download.

**Do not claim identity stability.** macOS file ids survive renames within a
volume, but reuse after deletion, restore, and clone behaviour has not been
measured on this project's corpus, so the identity is `Weak` on both macOS
adapters until it is.

**Make the volume record say which feed exists.** `VolumeInfo` gains
`native_feed` (`none` / `windows_usn` / `macos_fsevents`) and
`case_sensitive: Option<bool>`; `is_native_local()` is now "has a native feed
and is not remote". `VolumeInfo::source_kind()` is the single classifier, and
`SourceKind` gains `macos_local` and `macos_generic`. Feeds share the enum,
not their semantics: a USN cursor is a durable journal position and an FSEvents
cursor is a coalescing event id, and each adapter keeps its own.

## Consequences

- The native path is measurably cheaper: over a 2,356-directory, 91,761-entry
  developer tree it walks in 88 ms against 430 ms for `readdir` + `lstat`
  (warm cache, eight threads, identical entry counts and error counts).
- Every scanner adapter a platform can use now runs the same
  temporary-filesystem contract suite, so a native fast path cannot silently
  diverge from the portable reference. Setting `EIDOS_TEST_VOLUME` runs that
  suite on another volume, which is how case-sensitive APFS behaviour is
  covered without assuming such a volume exists.
- Case sensitivity is a recorded per-volume fact rather than an assumption.
  Path equality still must not be assumed either way: the same host can mount
  case-sensitive and case-insensitive APFS volumes at once.
- `volumes` gains `case_sensitive` and `native_feed` columns; rows written
  before the probe existed read back as `unknown`.
- The Windows adapter's `native_feed` is set from the same rule that
  `is_native_local()` used before, so classification is unchanged there.
- Two portability defects surfaced by running the agent on macOS are fixed
  with it: raw OS error codes were classified with a Win32 table on every
  platform (Win32 5 is `ACCESS_DENIED`, `errno` 5 is `EIO`), and the host name
  came only from `COMPUTERNAME`, which no Unix sets, so every macOS host
  identified itself as `unknown`.

## Alternatives considered

- **`readdir` + `lstat` everywhere.** Simplest, and it is what the fallback
  still does, but it costs a syscall per child and cannot report allocation
  size, BSD flags, or mount status at all.
- **`fts(3)` or `FileManager.enumerator`.** Both drive their own traversal;
  the walker already owns parallelism, ordering, and error accounting, and the
  ordering guarantee (a directory's event precedes its children's) is what lets
  the catalog writer resolve parents without buffering.
- **Claiming `ATTR_CMNEXT_LINKID` as a stable identity.** It distinguishes
  hard links on HFS+, but the stability question is the same one that is still
  unmeasured, and requesting extended common attributes changes the packing
  rules for every entry.
