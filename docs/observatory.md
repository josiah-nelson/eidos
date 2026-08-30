# Observatory collector

The macOS observatory collector is a privileged, local-only measurement lane
for an explicitly initialized workload study. It is part of the
`eidos observe init|run|status|mark|export|inspect` command family. It does not
enroll the machine, listen on a network socket, upload automatically, accept
remote control, or start the separate cross-host sync agent.

## Measurement lanes

The always-on lane records:

- OS build and physical/virtual/unknown classification;
- collector start, clean or unclean prior shutdown, heartbeat, sleep/wake,
  clock discontinuity, and process resource buckets;
- an opaque, versioned FSEvents cursor plus coalescing, kernel/user drop,
  overflow, root-change, mount, and unmount counters;
- capture gaps with monotonic and UTC anchors; and
- coarse workload counts.

The detailed FSEvents lane records keyed object and subtree tokens, logical
create/update/delete operations, best-effort adjacent rename pairing, hot edit
counts, delete/recreate age, depth, fan-out, extension class, size class, and
backlog age. FSEvents paths exist only transiently while these values are
formed.

The APFS lane uses FSEvents flags and read-only Foundation resource metadata to
classify volume identity, clones, hard-link identity observations, sparse
allocation, packages, extended attributes/resource forks, snapshot mounts,
external volumes, and iCloud placeholders. It requests metadata keys only; it
never opens file data or calls a download/materialization API. Unsupported
providers remain unclassified rather than being probed in a way that could
hydrate an object.

The optional Endpoint Security lane counts notify-only open, close, mmap, and
exec events. It never subscribes to an authorization event and never responds
to, delays, or blocks an operation. This lane is compiled into release builds
but is runtime-disabled by default.

## Privacy contract

`eidos observe init` creates a random 32-byte study key in the current user's
login keychain. The key is not included in a spool or export. The user-session
LaunchAgent retrieves it through Security.framework and hands it directly to
the root daemon over the local Unix socket; the daemon keeps it in memory.

Before a durable write:

- object, subtree, volume, signing, rename-pair, and mark identities become
  domain-separated keyed BLAKE3 tokens;
- processes become a coarse class or keyed signing-identifier token;
- sizes, ages, depths, extensions, fan-out, and counts become fixed buckets;
  and
- raw paths, filenames, host names, user names, IP addresses, arguments, file
  bytes, and raw system-wide event messages are discarded.

There is no durable schema field for those raw values. The detailed ring is
bounded to 10 GiB and 14 days by default; summaries expire after 90 days.
SQLite WAL durability is used locally. The spool directory is root-owned and
mode `0750`; the command socket is root-owned, group `admin`, and mode `0660`.

Exports use the versioned `eidos-observation/2` format, compressed with zstd.
Version 2 adds the explicit observatory-collector process class; it is not
labelled as version 1 because older v1 readers cannot decode that enum value.
Current readers continue to accept additive version 1 bundles.
Each bundle contains build/config hashes, capabilities, capture gaps, drop
counters, UTC and monotonic anchors, units, and the bounded records. Run:

```console
eidos observe inspect observation.eidos-observation.zst
```

The command lists exactly the fields actually present in that bundle.

## Build, sign, and install

The release layout follows Apple's guidance for a daemon with a restricted
entitlement:

```text
Eidos Collector.app/
  Contents/
    Info.plist
    MacOS/eidos-collector
    Resources/
    embedded.provisionprofile  # approved mode only
```

Build, sign, notarize, and package with:

```console
scripts/macos/build-collector.sh
scripts/macos/sign-notarize.sh
scripts/macos/package.sh
sudo scripts/macos/install.sh
eidos observe init
eidos observe status
```

The scripts prefer a Developer ID Application identity already in the login
keychain. When none exists, the signing script imports the base64 P12 into a
temporary keychain under `umask 077` and removes it on exit. The App Store
Connect API key is likewise decoded only into a trapped temporary directory.
Decoded certificates, keys, profiles, and secret values are never printed.
The app and session CLI are signed together and included in the same notary
submission so keychain authorization remains stable across CLI upgrades.

If a Developer ID Installer identity is present, `package.sh` emits a signed,
notarized component package whose postinstall bootstraps the system
LaunchDaemon. If it is absent, the script reports that fact and retains the
notarized app archive, CLI, and `install.sh` fallback.

The system LaunchDaemon runs as root with `KeepAlive` and a 30-second throttle.
The daemon writes through an in-process 10 MiB capped log writer; launchd's
standard streams go to `/dev/null`, so the log bound cannot constrain the much
larger spool. The user LaunchAgent maintains the keychain session handoff. All
control uses the Unix socket; neither component opens a TCP port.

To enable the runtime Endpoint Security lane for a pending-entitlement test:

```console
sudo scripts/macos/install.sh --endpoint-security
```

## Restricted entitlement transition

Two entitlements files are checked in. `collector-pending.entitlements` omits
the restricted key. `collector.entitlements` adds
`com.apple.developer.endpoint-security.client`.

Every build validates the decoded profile without dumping it. Validation
requires all of the following:

- valid CMS;
- an unexpired expiration date;
- the Endpoint Security client entitlement;
- the `com.jnel.eidos.collector` application identifier; and
- a developer certificate matching the selected Developer ID Application
  signing certificate.

Pending entitlements are selected unless validation passes **and**
`EIDOS_ES_ENTITLED=1` is set. After Apple approves the capability, the release
path is:

```console
EIDOS_ES_ENTITLED=1 scripts/macos/sign-notarize.sh
scripts/macos/package.sh
sudo scripts/macos/install.sh --endpoint-security
```

The sign script rebuilds first, so the one flag controls profile embedding and
entitlement choice together.

`es_new_client` failures do not stop the daemon. Status records these outcomes
separately:

- `not_entitled`: the executable lacks an authorized entitlement;
- `not_permitted`: TCC Full Disk Access is not approved; and
- `not_privileged`: the client is not running as root.

The status also records the entitlement claim, TCC result, and effective-root
fact independently. None is inferred from another. L0/L1 collection continues
when L2 is unavailable.

This follows Apple's [Endpoint Security client result guidance](https://developer.apple.com/documentation/endpointsecurity/client),
[restricted-entitlement daemon layout](https://developer.apple.com/documentation/xcode/signing-a-daemon-with-a-restricted-entitlement),
[provisioning profile model](https://developer.apple.com/documentation/technotes/tn3125-inside-code-signing-provisioning-profiles),
and [Developer ID notarization flow](https://developer.apple.com/developer-id/).

## Uninstall

```console
sudo scripts/macos/uninstall.sh
```

The default preserves `/var/db/eidos-collector` and the login-keychain study
key so an interrupted study can be recovered. To remove the spool as well:

```console
sudo scripts/macos/uninstall.sh --purge-data
```

The keychain item is intentionally preserved in both cases. Remove it through
Keychain Access only when prior study tokens no longer need to remain stable.

## Windows collector

The Windows lane set runs as the `eidos-collector` service (LocalSystem,
delayed automatic start, restart on failure) and is controlled over a local
named pipe by `eidos observe` from an elevated prompt. It has no listener and
no remote control; its only outbound path is the optional scheduled upload
described below, which the collector initiates and which is off by default.
Install and run:

```powershell
eidos observe init                 # DPAPI machine-scope study key + default config
eidos observe configure --lanes usn,etw   # lanes, upload: written to config.json
eidos observe install --start-now  # register and start the service
eidos observe status               # capabilities, feeds, lanes, ring usage
eidos observe mark vm-snapshot     # keyed phase marker
eidos observe lanes --etw on       # switch lanes at run time
eidos observe probe                # one read-only enumeration of each fixed volume
eidos observe export -o study.eidos-observation.zst
eidos observe inspect study.eidos-observation.zst
eidos observe uninstall            # keeps the data directory and key
```

`eidos observe run` runs the same daemon in the foreground for testing.
`eidos observe init --key-hex <64 hex>` imports a cohort-shared key so
content fingerprints compare across hosts; otherwise the key is random.
`eidos observe configure` writes the same choices the installer writes —
lanes, upload destination and hour, excluded volumes — into `config.json`,
which the collector reads at its next start.

Data lives in `C:\ProgramData\eidos-collector` (ACL: SYSTEM and
Administrators only): the DPAPI-protected key, `config.json`, the SQLite
spool ring, per-volume feed cursors, staged exports, and seven days of logs.

### Fleet install

`eidos-collector-setup.exe` installs the collector on a host with no build
tree: one signed executable carrying a per-machine MSI, and no prerequisites
to install first. It reaches the same end state as `observe init` followed by
`observe install --start-now` — executable in `%ProgramFiles%\eidos-collector`,
data directory created and locked down, study key in place, `config.json`
written, service registered and started.

```powershell
eidos-collector-setup.exe                     # one screen, then install
eidos-collector-setup.exe /quiet EIDOS_STUDY_KEY=<64 hex> `
    EIDOS_LANES=usn,etw EIDOS_UPLOAD=\\fileserver\share\eidos EIDOS_UPLOAD_HOUR=3
eidos-collector-setup.exe /quiet /uninstall   # keeps the data directory
eidos-collector-setup.exe /quiet /uninstall EIDOS_COLLECTOR_REMOVE_DATA=1
```

| Variable | Meaning |
|---|---|
| `EIDOS_STUDY_KEY` | cohort key, 64 hex characters; a per-host key is generated when omitted. Hidden, so it stays out of the installer log |
| `EIDOS_LANES` | lanes to enable: `usn,etw,content,enumeration`, or `all` / `none` |
| `EIDOS_UPLOAD` | daily upload destination, typically a UNC share; `none` turns the upload off and clears it. Give it without a trailing backslash — one escapes the closing quote of the command line the installer builds, and the install fails rather than storing a broken destination |
| `EIDOS_UPLOAD_HOUR` | local hour, 0-23, at or after which that upload runs |
| `EIDOS_COLLECTOR_INSTALLDIR`, `EIDOS_COLLECTOR_DATADIR` | program and data directories |
| `EIDOS_COLLECTOR_START` | `0` registers the service without starting it. Not remembered: pass it again on an upgrade, or the upgrade starts it |
| `EIDOS_COLLECTOR_REMOVE_DATA` | `1` deletes the data directory on uninstall |

Give every host in a cohort the same study key, and enable the L2 lanes only
on the hosts whose role calls for them. Passing the key again on a later
install is safe: a host that already has that key keeps it, and one that has
a *different* key fails the install rather than rotating quietly and splitting
its own tokens in two. Replacing a key on purpose is
`eidos observe init --force --key-hex ...` on the host.

The key is hidden from the installer log, but it is still an argument on the
command line of the setup process and of the `observe init` it runs, so it is
visible for that moment to anything on the host that can read another
process's command line — which on Windows means administrators and SYSTEM.
On a shared administrative host, prefer initialising the key by hand.

If a host is installed with a custom `EIDOS_COLLECTOR_DATADIR`, pass it again
on any later reinstall: the remembered value lives in the package's own
registry key, which an uninstall removes even when it keeps the data. Without
it a reinstall starts a fresh study in the default location. Lanes and upload are written only
when they are named, so a later upgrade that names neither leaves the
configuration a host is running with alone, including runtime
`observe lanes` changes; after the first install, `config.json` is the
authority.

The MSI inside the setup can also be deployed on its own, which is what a
management tool wants:

```powershell
msiexec /i eidos-collector.msi /qn EIDOS_LANES=usn EIDOS_UPLOAD=\\fileserver\share\eidos
msiexec /x eidos-collector.msi /qn EIDOS_COLLECTOR_REMOVE_DATA=1
```

It is per-machine and must be installed elevated. The collector package and
the indexer package (`eidos.msi`, `eidos-setup.exe`) are independent: a host
can run either, both, or neither.

Uninstall stops and removes the service and keeps the data directory —
including the study key, so a reinstall keeps earlier tokens comparable —
unless `EIDOS_COLLECTOR_REMOVE_DATA=1` says otherwise.

### Lanes

Always on (L0): OS build and update revision, hypervisor bit, processor and
memory shape, uptime, cumulative sleep (interrupt-time bias), power source,
elevation and SYSTEM facts, clock jumps and resumes, clean/unclean shutdown,
upgrade detection, per-interval collector and host resource samples tagged
with the lanes in force, and a volume inventory (filesystem, drive and bus
kind, seek penalty, capacity and free buckets, feature flags, journal
shape) diffed every scan for mount, unmount, and journal recreation.

USN change lane (L1, on by default): one reader per journaled local volume.
Object identity is the keyed token of `(volume GUID, file reference
number)`; no path is resolved for files, and names are used only to pick an
extension bucket and to key a delete/recreate lookup. Records carry
create/update/delete/rename/metadata/hard-link/stream operations, rename
pairing with the old parent subtree, hot edit counts, fan-out, size and
depth buckets (one non-reparse parent-directory enumeration per changed
parent, bounded per directory and batch, skipped for oversized batches), and
backlog age. Per-interval
summaries add per-second histograms, operation counts, distinct objects at
1 s/10 s/60 s/10 min/1 h coalescing windows (the shadow-sync row saving),
tombstones, hot objects, recreates, reason-bit combinations, and feed
health with lag, fill, and backlog histograms. Overflow and journal
recreation become capture gaps; the cursor survives restarts.

ETW access lane (L2, off by default): a real-time session over
`Microsoft-Windows-Kernel-File` and `Kernel-Process`, decoded through TDH
metadata, run in randomized windows (`minutes_per_hour`, 60 for
continuous). Events are attributed to coarse process classes (system,
indexer, collector, security, build, development, shell, productivity,
browser, media, cloud sync, backup, virtualization) or a keyed image token;
the collector's own process is classed `collector` by process identity, so
the cost of observing is separable from the cost of eidos even though both
run from `eidos.exe`, and summarised
per class: opens, reads, writes, closes, deletes, renames, byte totals,
I/O-size histograms, distinct and read-then-written objects, and extension
buckets learned at open. Lost events count as kernel drops; access denied
and session conflicts are recorded as capability facts.

Content economics probe (L2, off by default): a deterministic sample of
files closed after a write is read under an hourly byte budget and size
cap after a settle delay. Placeholder, offline, and reparse-point objects
are recognised from a non-hydrating attribute snapshot and skipped. Each
measured object records FastCDC chunk count and size histogram, a keyed
whole-content fingerprint, chunk reuse against the previous observation of
the same object (edit locality) with run lengths, zstd ratio, and a text
heuristic.

Enumeration probe (L2, off by default; also on demand): a read-only walk
of a fixed volume with the production lister, timed with CPU cost, giving
file and directory counts, fan-out and depth, size and extension buckets,
and reparse/placeholder/sparse/compressed/encrypted/offline counts.

### Scheduled upload

A fleet is easier to collect from than to visit. With `upload.enabled`, the
collector stages a bundle once a day at or after `upload.hour` (local time)
and copies it to `upload.destination` — an ordinary directory path, typically
a UNC share:

```json
"upload": {
  "enabled": true,
  "destination": "\\\\fileserver\\share\\eidos",
  "hour": 3,
  "attempts": 3,
  "remove_after_upload": true
}
```

The service runs as LocalSystem, so a share must grant write access to the
**machine account** (`DOMAIN\HOST$`), not to the operator who configured it;
this is the usual reason a first upload fails with access denied.

Each bundle is copied under a temporary `.part` name and renamed into place,
so a reader on the share never sees a half-written file. Names are prefixed
with a keyed per-host token, so many collectors can share one directory
without colliding and without putting host names on the share. A bundle that
was already delivered is not copied again.

Delivery is unhurried and forgiving: the day's upload is retried up to
`attempts` times, and any bundle still staged locally — including one left by
an earlier failure — is retried on the next run, so a share that is offline
for a day catches up rather than losing that day. A run that delivers only
part of a backlog counts as a failure, so the rest is retried instead of
waiting until tomorrow behind a success.

Upload requires a study key and the machine's own name, because the per-host
prefix is the keyed token of the name; run `eidos observe init` first. The
name is the distinguishing input precisely because a cohort may share one
study key so content fingerprints compare across hosts — the key alone cannot
tell two collectors apart. The name never leaves the host; the share sees only
the token. The copying runs on its own thread so a
stalled share cannot delay the service stopping. `remove_after_upload`
deletes the local copy once it has been delivered; leave it off to keep
bundles on the host as well. `eidos observe status` reports the destination,
the last successful upload, how many bundles are waiting, and why the last
attempt failed.

`upload.destination` is local configuration and is never exported, and the
upload path changes nothing about what is collected: it copies the same
bundle `eidos observe export` produces, so the privacy contract below applies
to it unchanged. Upload settings are excluded from the manifest's
configuration hash for the same reason — where a bundle is delivered says
nothing about how it was collected.

### Privacy contract on Windows

Before a durable write: object, subtree, volume, image, content, chunk, and
marker identities become domain-separated keyed BLAKE3 tokens; sizes,
ages, depths, extensions, counts, and ratios become buckets or aggregate
histograms; raw USN records, ETW payloads, paths, image names, and file
bytes are dropped. Aggregate counts (events per interval, files per volume)
are exact because they name no object. `eidos observe status` may show
drive letters to the operator; exports never do.
