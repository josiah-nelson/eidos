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
classify clones, hard-link identity observations, sparse allocation, packages,
extended attributes, and iCloud placeholders. It requests metadata keys only;
it never opens file data or calls a download/materialization API. Unsupported
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

Exports use the versioned `eidos-observation/1` format, compressed with zstd.
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

If a Developer ID Installer identity is present, `package.sh` emits a signed,
notarized component package whose postinstall bootstraps the system
LaunchDaemon. If it is absent, the script reports that fact and retains the
notarized app archive, CLI, and `install.sh` fallback.

The system LaunchDaemon runs as root with `KeepAlive`, a 30-second throttle,
and a 10 MiB log file resource limit. The user LaunchAgent maintains the
keychain session handoff. All control uses the Unix socket; neither component
opens a TCP port.

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
