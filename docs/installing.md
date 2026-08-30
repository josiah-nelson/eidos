# Installing eidos on Windows

Download `eidos-<version>-setup.exe` from the
[releases page](https://github.com/josiah-nelson/eidos/releases) and run
it. 64-bit Windows 10 or later (Windows Server 2019 or later) is required.

## What the setup asks

**Who is eidos for?**

- *Just me* — no administrator rights. eidos installs under your profile
  (`%LocalAppData%\Programs\eidos`, data in `%LocalAppData%\eidos`), runs
  as a background process while you are signed in, and indexes what your
  account can open.
- *All users on this computer* — installs to `Program Files` and runs as
  the `eidos` Windows service, which starts with the computer and keeps
  indexing when nobody is signed in. Windows asks for administrator
  approval when the installation starts.

**Where should eidos live?** — the program folder, the data folder
(catalog, search indexes and logs; it grows to a few percent of the indexed
data), and the port of the web interface. *Advanced* sets the listen
address: keep `127.0.0.1` unless the network is trusted — eidos has no login
yet, and on any other address everyone who can reach the computer can search
and read what it indexes.

**Which account runs the service?** (all-users only)

- *The system account* — full access to local drives, no network identity:
  mapped drives and `\\server\share` paths are invisible to it.
- *A Windows account* — the service sees exactly what that account can
  open, including network shares. The setup verifies the password, grants
  the account *Log on as a service*, and gives it full control of the data
  folder. Windows stores the password for the service; eidos never does.

**Ready to install** — start automatically (service at boot, or background
process at sign-in), Start-menu shortcuts, and whether to open eidos in the
browser when setup finishes. *Advanced: profiling collector* (all-users
installs) adds the observatory collector: a separate privileged service
that records bounded, privacy-preserving workload measurements
([observatory.md](observatory.md)). It has its own data directory
(`%ProgramData%\eidos-collector`), identity, and removal; installing,
repairing or removing eidos never silently changes it. On a later run of
the setup the checkbox shows whether the collector is installed, and leaving
it as it is never removes one.

Adding sources, content policies and everything else happens in the web
interface once eidos is running.

## After installation

- The web interface is at `http://127.0.0.1:<port>/` (the Start-menu
  `eidos` entry opens it).
- *Just me*: `Start eidos` in the Start menu starts the background process
  if it is not running. Logs are in `%LocalAppData%\eidos\logs`.
- *All users*: `eidos service status` (elevated) shows the registration,
  state and health; `eidos service stop|start|restart` control it. Logs are
  in `%ProgramData%\eidos\logs`.

## Upgrading, repairing, removing

Run the newer setup: it upgrades in place, keeping every setting and the
indexed data, and restarts the service. Running the installed version's
setup again (or *Settings › Apps › eidos*) offers *Repair* and *Remove*.
Removal keeps the data folder unless you tick *Also delete the indexed
data*; the files that were indexed are never touched. When the collector is
installed, removal offers to remove its service too (on by default) and,
separately, to delete its study data (off by default, so a later reinstall
keeps its object tokens comparable). A host set up earlier with the
separate collector setup is upgraded in place by this setup; its study key,
spool and configuration are kept.

## Unattended installs

```powershell
eidos-setup.exe /quiet EIDOS_SCOPE=perMachine EIDOS_PORT=7700
eidos-setup.exe /quiet EIDOS_SCOPE=perMachine EIDOS_SERVICE_ACCOUNT_KIND=user EIDOS_SERVICE_DOMAIN=CORP EIDOS_SERVICE_USER=svc-eidos EIDOS_SERVICE_PASSWORD=...
eidos-setup.exe /passive                               # per-user, progress only
eidos-setup.exe /quiet EIDOS_SCOPE=perMachine EIDOS_INSTALL_COLLECTOR=1 EIDOS_STUDY_KEY=<64 hex> EIDOS_LANES=usn
eidos-setup.exe /quiet /uninstall EIDOS_REMOVE_DATA=1
eidos-setup.exe /quiet /uninstall EIDOS_REMOVE_COLLECTOR=1 EIDOS_COLLECTOR_REMOVE_DATA=1
```

Variables: `EIDOS_SCOPE` (`perUser` | `perMachine`), `EIDOS_INSTALLDIR`,
`EIDOS_DATADIR`, `EIDOS_BIND`, `EIDOS_PORT`, `EIDOS_SERVICE_ACCOUNT_KIND`
(`local-system` | `local-service` | `network-service` | `user`),
`EIDOS_SERVICE_DOMAIN`, `EIDOS_SERVICE_USER`, `EIDOS_SERVICE_PASSWORD`,
`EIDOS_START_SERVICE`, `EIDOS_START_MENU`, `EIDOS_REMOVE_DATA` (`1`/`0`).
Collector: `EIDOS_INSTALL_COLLECTOR` (`1` installs, `0` leaves it out of a
fresh install, empty keeps what is installed), `EIDOS_REMOVE_COLLECTOR`
(`0` keeps the collector service when eidos is removed),
`EIDOS_COLLECTOR_REMOVE_DATA` (`1` deletes its study data), and the
collector's own `EIDOS_STUDY_KEY`, `EIDOS_LANES`, `EIDOS_UPLOAD`,
`EIDOS_UPLOAD_HOUR`, `EIDOS_COLLECTOR_INSTALLDIR`, `EIDOS_COLLECTOR_DATADIR`,
`EIDOS_COLLECTOR_START` (see [observatory.md](observatory.md)).
Setup logs are written to `%TEMP%\eidos_<timestamp>.log` (`/log <path>`
to choose).

Administrators who prefer the bare package can use `eidos-<version>.msi`
with the same properties (`msiexec /i eidos.msi ALLUSERS=1 EIDOS_PORT=7700`
for a machine install; without `ALLUSERS=1` it installs per-user).

## Fleet

Several installations can replicate their catalog metadata into one
central and search the union; see [fleet.md](fleet.md). Nothing in the
setup enrolls a host: enrollment is an explicit `eidos fleet enroll` on a
running installation, and the sync listener (port 7710 by default) is only
opened by `eidos fleet central --listen`.

## Troubleshooting

- *"Setup did not finish"* — the failure page links the log; the MSI log
  next to it (`…_000_EidosMsi.log`) has the failing action.
- The service does not start — `eidos service status` shows the exit
  reason; the service log is in the data folder's `logs`. A port already in
  use or a data folder the service account cannot write are the usual
  causes.
- Standard users on Windows Server cannot install per-user (Windows
  Installer policy); use *All users*.
