# Shared-device reader admission

Activity → **Shared-device reader limits** controls the combined scan enumerators
and content readers admitted against each backing OS disk. This ceiling applies
in addition to the content pool, per-source limits, metadata scan ceiling and
data-volume reserve. It does not replace any of them.

The initial limit is two readers, adjustable from 1–64. This is a conservative
admission setting, **not a measured performance preset**. It persists in
`device-limits.json`. Invalid/corrupt settings fail startup; a failed save leaves
the effective limit unchanged. Existing reservations are never revoked when a
limit is lowered: new work waits until enough capacity drains.

## What shares a budget

Windows local roots are resolved through mount points/junctions, then the
volume's disk extents identify its backing OS disks. Multiple partitions or
roots on the same disk share capacity. A spanned volume charges **every** backing
disk atomically. The probe accepts at most 64 extents and rejects partial,
invalid or failed results instead of treating a subset as independent storage.
See Microsoft's [mount-point contract](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getvolumepathnamew)
and [disk-extents contract](https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-ioctl_volume_get_volume_disk_extents).

These are OS disk identities, not proof that hardware RAID, SAN or virtual disks
have independent physical media. Remote shares/mapped network drives,
unsupported namespaces/storage stacks and failed probes have unknown topology.
macOS/Linux currently use the unknown fallback; an APFS volume ID or `st_dev`
is not asserted to be an independent physical store.

If **any source scanned by this service** is unresolved, all such sources share
one explicitly labeled conservative budget. This can reduce throughput, but it
does not invent independence for unknown storage. Fleet replicas are excluded:
their source I/O belongs to the origin service. Known disjoint OS disks can
admit independently when there are no unresolved roots.

## Accounting and freshness

A content worker reserves its source slot and device slot before claiming a
job. The device ceiling is checked first, because a refusal there holds every
source sharing the budget: Activity's per-source peak reservation therefore
counts work this service actually admitted, not attempts this ceiling refused.
Empty/failed claims, errors, cancellation and unwind release the guards.
Scans reserve their granted enumeration width: a request for eight threads
with one unit available runs with one thread. The scan reservation covers the
native scan sequence through replay/publication. Queued scans remain cancellable
and do not suppress their own source's content claims just because they queued.

Topology probes run on one single-flight background worker, on demand during
work or diagnostics requests, at most once per 30 seconds for unchanged roots.
There is no unconditional topology timer during quiet idle. Root changes reject
old probe results. Samples older than 90 seconds use the unknown fallback;
age starts at the beginning of the batch, not its eventual completion. A stuck
syscall retains its one worker rather than spawning replacements.

When topology changes, existing guards retain their original keys. New claims
wait until all old guards drain before the new mapping is installed. Activity
shows this transition separately from capacity exhaustion, along with root
membership, probe errors, current content/scan reservations and peak combined
reservations. Peaks are per retained budget in this process; obsolete groups
are discarded, not kept forever.

The ceiling does **not** cover catalog/index/fleet writes, native change feeds,
query/file-serving I/O or the one background topology metadata probe. It is not
an IOPS, bandwidth, physical-disk or RAM quota. A long/stuck file read still holds
its reservation; fairness and mid-syscall cancellation are not promised.

## API and CLI

`GET /api/devices` returns the budget, sample age/staleness, cached source roots
and probe errors. It does not query the catalog or synchronously probe roots.
`POST /api/devices` accepts `{"readers_per_device":2}`. It leaves the separate
`/api/resources` settings unchanged. Source IDs and other 64-bit numbers retain
the API's exact decimal-string convention.

```powershell
eidos resources --devices
eidos resources --devices --json
eidos resources --device-readers 2
```

The Activity editor preserves dirty drafts across polling, follows remote saves
when untouched, and discloses conflicts and persistence failures. See
[resource controls](resource-controls.md), [memory diagnostics](memory.md) and
[recovery gates](recovery.md). Synthetic admission tests are not installed or
real-media performance qualification.
