# eidos Windows installer

WiX v7 authoring for the eidos setup:

| Project | Output | Role |
|---|---|---|
| `Eidos.Msi` | `eidos.msi` | The Windows Installer package (dual-scope) |
| `Eidos.Setup.Ui` | `eidos-setup-ui.exe` | The guided UI: a .NET Framework 4.7.2 WPF bootstrapper application |
| `Eidos.Bundle` | `eidos-setup.exe` | The product: Burn engine + UI + MSI (+ .NET prerequisite) |

```powershell
.\installer\build.ps1              # web UI + release eidos.exe + installer\out\{eidos.msi,eidos-setup.exe}
.\installer\build.ps1 -SkipWeb -SkipRust
```

## The setup (`eidos-setup.exe`)

Double-click for the guided install: scope (just me / all users as a
service), program and data folders, port and listen address, service
account, options, then progress and a launch button. Run it again (or use
Settings › Apps) for repair or removal; removal offers to delete the data
folder and shows its size. A newer installed version is reported, an older
one is upgraded in place with settings and data kept.

Unattended:

```powershell
eidos-setup.exe /quiet EIDOS_SCOPE=perMachine EIDOS_PORT=7700 EIDOS_SERVICE_ACCOUNT_KIND=local-service
eidos-setup.exe /passive                           # progress only, per-user defaults
eidos-setup.exe /quiet /uninstall EIDOS_REMOVE_DATA=1
eidos-setup.exe /log setup.log ...                 # explicit log path
```

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
  bound executable.
