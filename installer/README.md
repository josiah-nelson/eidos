# eidos Windows installer

WiX v7 authoring for the eidos setup. `Eidos.Msi` is the Windows Installer
package; the bootstrapper bundle (setup `.exe` with the guided UI) wraps it.

```powershell
.\installer\build.ps1              # web UI + release eidos.exe + installer\out\eidos.msi
.\installer\build.ps1 -SkipWeb -SkipRust
```

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
  `%LocalAppData%\eidos`, no service; a *Start eidos* shortcut runs the
  indexer in a console.

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
