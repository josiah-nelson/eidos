# eidos Windows installer

WiX v7 authoring for the eidos setup:

| Project | Output | Role |
|---|---|---|
| `Eidos.Msi` | `eidos.msi` | The Windows Installer package (dual-scope) |
| `Eidos.Setup.Ui` | `eidos-setup-ui.exe` | The guided UI: a .NET Framework 4.7.2 WPF bootstrapper application |
| `Eidos.Bundle` | `eidos-setup.exe` | The product: Burn engine + UI + core MSI + optional collector MSI (+ .NET prerequisite) |
| `Eidos.Collector.Msi` | `eidos-collector.msi` | The observatory collector package (per-machine) |
| `Eidos.Collector.Bundle` | `eidos-collector-setup.exe` | The collector setup: Burn engine + standard BA + MSI |

```powershell
.\installer\build.ps1              # web UI + release eidos.exe + installer\out\*
.\installer\build.ps1 -SkipWeb -SkipRust
```

The two packages are independent and share only `eidos.exe`: `eidos.msi`
installs the `eidos` service and the web UI, `eidos-collector.msi` the
`eidos-collector` service. `eidos-setup.exe` carries both and offers the
collector as an advanced option; `eidos-collector-setup.exe` remains for
fleet installs that want only the collector. A host can have either, both,
or neither. Each `-Skip` switch turns off exactly one artifact; the unified
bundle needs both MSIs built first.

## The setup (`eidos-setup.exe`)

Double-click for the guided install: scope (just me / all users as a
service), program and data folders, port and listen address, service
account, options, then progress and a launch button. Run it again (or use
Settings › Apps) for repair or removal; removal offers to delete the data
folder and shows its size. A newer installed version is reported, an older
one is upgraded in place with settings and data kept.

*Advanced: profiling collector* on the options page adds the collector
package (all-users scope: it is a LocalSystem service). The setup UI plans
that package per action: `Present` when chosen on install or upgrade,
`Repair` when installed on repair, `Absent` on removal unless the operator
keeps it, and `None` otherwise - an empty choice never removes an existing
collector, and the checkbox reflects the installed state on maintenance
(`DetectPackageComplete`). The package is not vital, so a host that
refuses it still gets a healthy core. A `RelatedBundle` upgrade of the
separate collector setup's bundle code lets a host set up with
`eidos-collector-setup.exe` upgrade into this setup; the collector MSI is
major-upgraded in place, so the study key, spool and configuration stay.

Unattended:

```powershell
eidos-setup.exe /quiet EIDOS_SCOPE=perMachine EIDOS_PORT=7700 EIDOS_SERVICE_ACCOUNT_KIND=local-service
eidos-setup.exe /passive                           # progress only, per-user defaults
eidos-setup.exe /quiet EIDOS_SCOPE=perMachine EIDOS_INSTALL_COLLECTOR=1 EIDOS_STUDY_KEY=<64 hex> EIDOS_LANES=usn
eidos-setup.exe /quiet /uninstall EIDOS_REMOVE_DATA=1
eidos-setup.exe /quiet /uninstall EIDOS_REMOVE_COLLECTOR=1 EIDOS_COLLECTOR_REMOVE_DATA=1
eidos-setup.exe /log setup.log ...                 # explicit log path
```

`EIDOS_INSTALL_COLLECTOR` (`1`/`0`/empty = keep as installed),
`EIDOS_REMOVE_COLLECTOR` (`0` keeps the collector service on removal) and
`EIDOS_COLLECTOR_REMOVE_DATA` drive the collector package; the collector's
own properties (`EIDOS_STUDY_KEY`, `EIDOS_LANES`, `EIDOS_UPLOAD`,
`EIDOS_UPLOAD_HOUR`, `EIDOS_COLLECTOR_INSTALLDIR`, `EIDOS_COLLECTOR_DATADIR`,
`EIDOS_COLLECTOR_START`) pass through unchanged.

Every `EIDOS_*` MSI property listed below is also a bundle variable that a
`NAME=value` argument overrides; `EIDOS_SCOPE` (`perUser` | `perMachine`)
selects the scope without the UI. Logs go to `%TEMP%\eidos_*.log`.

Requirements: Node.js, Rust, and a .NET SDK (8 or later). WiX itself is
restored from NuGet (`WixToolset.Sdk/7.0.0`); the Open Source Maintenance Fee
EULA is accepted in the project file (`<AcceptEula>wix7</AcceptEula>`).

## The package

`eidos.msi` is **dual-scope** (`Package/@Scope="perUserOrMachine"`): the same
file installs either

- **per-machine** (elevated): `%ProgramFiles%\eidos`, data in
  `%ProgramData%\eidos`, and the `eidos` Windows service (delayed automatic
  start, restart on failure, 3-minute pre-shutdown window, description). The
  service account is LocalSystem, `LocalService`, `NetworkService`, or a
  named account, which is granted *Log on as a service* and Full Control of
  the data directory. The service is started at the end of the install;
  a failed start rolls the install back.
- **per-user** (no elevation): `%LocalAppData%\Programs\eidos`, data in
  `%LocalAppData%\eidos`, no service. eidos runs as a background process
  of the signed-in user (`eidos serve --detach`: no window, logs in the data
  folder): started by the setup, at every sign-in through an HKCU `Run`
  entry when *start automatically* is chosen, and by the *Start eidos*
  shortcut. Upgrade and uninstall close a running `eidos.exe` first
  (`util:CloseApplication`; the catalog is crash-safe) so files and, on
  request, the data can be removed.

Both scopes write the chosen paths and port to `HKMU\Software\eidos` so
repair, upgrade and uninstall find the same directories, and add an `eidos`
Start-menu link to the web UI. Uninstall keeps the data directory unless
`EIDOS_REMOVE_DATA=1`. Major upgrades remove the previous version first
(service stopped, data untouched) and install the new one.

Properties the bootstrapper passes (all public, all remembered):
`EIDOS_INSTALLDIR`, `EIDOS_DATADIR`, `EIDOS_BIND`, `EIDOS_PORT`,
`EIDOS_SERVICE_ACCOUNT_KIND`, `EIDOS_SERVICE_DOMAIN`, `EIDOS_SERVICE_USER`,
`EIDOS_SERVICE_PASSWORD` (hidden), `EIDOS_START_SERVICE`, `EIDOS_START_MENU`,
`EIDOS_REMOVE_DATA`.

Running the MSI directly (without the bundle) takes the defaults, which
means a per-user install:

```powershell
msiexec /i eidos.msi                                   # per-user
msiexec /i eidos.msi ALLUSERS=1 EIDOS_PORT=7700        # per-machine (elevated)
msiexec /x eidos.msi EIDOS_REMOVE_DATA=1               # uninstall and delete data
```

## The collector setup (`eidos-collector-setup.exe`)

For putting the observatory collector on hosts that have no build tree. It is
per-machine only, has no prerequisites and no interactive choices, and uses
the WiX standard bootstrapper application: a fleet install passes what it
needs on the command line.

```powershell
eidos-collector-setup.exe /quiet EIDOS_STUDY_KEY=<64 hex> `
    EIDOS_LANES=usn,etw EIDOS_UPLOAD=\\fileserver\share\eidos EIDOS_UPLOAD_HOUR=3
eidos-collector-setup.exe /quiet /uninstall                              # keeps the data
eidos-collector-setup.exe /quiet /uninstall EIDOS_COLLECTOR_REMOVE_DATA=1
msiexec /i eidos-collector.msi /qn EIDOS_LANES=usn                       # for a management tool
```

`eidos-collector.msi` installs `eidos.exe` into `%ProgramFiles%\eidos-collector`,
registers the `eidos-collector` service (LocalSystem, delayed automatic start,
restart three times at 30-second intervals, three-minute pre-shutdown window),
and runs two deferred commands between `InstallFiles` and `InstallServices`:

- `eidos observe init` — creates the data directory, restricts it to SYSTEM
  and Administrators, and creates or imports the DPAPI machine-scope study
  key. `EIDOS_STUDY_KEY` is `Hidden`, and the custom action hides its target,
  so the key reaches neither a verbose log nor the ARP registry.
- `eidos observe configure` — writes `config.json`, and only when at least one
  of `EIDOS_LANES`, `EIDOS_UPLOAD` and `EIDOS_UPLOAD_HOUR` was given: the
  command line is assembled from the properties that were actually passed, so
  an upgrade that names none of them leaves a host's runtime `observe lanes`
  choices alone. Only the install and data directories are remembered in
  `HKLM\Software\eidos-collector`; lanes and upload deliberately are not,
  because after the first install `config.json` is the authority.

`EIDOS_UPLOAD=none` turns the daily upload off and clears the destination;
an empty property cannot mean that, because Windows Installer cannot tell an
empty property from an absent one, and absent has to keep meaning "leave this
host alone". A destination must not end in a backslash: it would escape the
closing quote of the command line the package builds, and `observe configure`
rejects what arrives rather than storing a destination that never resolves.

A study key passed to a host that already has a different one fails the
install instead of rotating it. The key is kept out of the installer log
(`Hidden`, plus `HideTarget` on the action that consumes it — verified by
grepping a `/l*v` log for it), but it is still visible for the moment it is
an argument to anything that can read another process's command line.

Uninstall stops and removes the service and keeps the data directory —
study key included, so a reinstall keeps earlier object tokens comparable —
unless `EIDOS_COLLECTOR_REMOVE_DATA=1`, which runs `eidos observe purge`
after `DeleteServices`. That is a command rather than `util:RemoveFolderEx`
for a reason: a Windows Installer file list is built before the service
stops, so the spool's write-ahead log, a log line, or a moved cursor written
in between outlives the list and keeps the directory alive.

## Authoring notes

- `eidos.exe` appears in two components with mutually exclusive conditions
  (`ALLUSERS=1` with the service, `NOT ALLUSERS=1` without). Windows Installer
  installs exactly one; ICE30 is suppressed for that reason.
- ICE105 ("invalid for a per-user application") is suppressed: the rows it
  flags belong to per-machine-only components.
- Directory properties end in a backslash, which would escape a closing
  quote on a command line, so service and shortcut arguments are written as
  `"[DATAFOLDER]."`; `eidos` normalises that spelling.
- The web UI is embedded in `eidos.exe`; the package carries three files.
- `build.ps1` deletes `Eidos.Msi\obj` before building: WiX's incremental
  build does not notice a changed `BinDir` and would reuse the previously
  bound executable. The same applies to the collector projects.
- The collector package is `Scope="perMachine"` with a `Privileged` launch
  condition: the collector is a LocalSystem service and there is nothing to
  install for a single user.
- Service and purge arguments spell the data directory `"[DATAFOLDER]."` for
  the same reason as above, and `observe` normalises that spelling before
  anything else touches the path.
- `EIDOS_COLLECTOR_DATADIR` and `EIDOS_COLLECTOR_INSTALLDIR` are remembered in
  component-owned registry values, so an uninstall forgets them even when it
  keeps the data. A host installed with a custom data directory needs the same
  property again on a reinstall, or it starts a fresh study in ProgramData.
