# Installing eidos on Windows

These instructions describe the collector-free recovery build. v0.5.0 does
not contain the recovery fixes; do not treat a source build or a passing CI
run as a qualified replacement release. See [recovery.md](recovery.md).

Download `eidos-<version>-setup.exe` from the
[releases page](https://github.com/josiah-nelson/eidos/releases) and run
it. 64-bit Windows 10 or later (Windows Server 2019 or later) is required.

## What the setup asks

**Who is eidos for?**

- *Just me* — no administrator rights for the core. eidos installs under
  your profile
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
browser when setup finishes. Only the core product is installed.

Open **Sources > Node setup** to choose standalone, master, or joining an
existing master. Then explicitly select drives or add folders. Nothing is
preselected. Each drive retains its success or error result, and **Retry
failed drives** submits only failed creations. If a source was added but its
scan could not start, **Retry scan** uses that source; it does not add it again.
You can also leave scans off and start them later from Sources.

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
If the Windows service runs as a named user, interactive setup asks for that
account's password again before an upgrade; Windows does not make the stored
service password readable. An unattended upgrade must pass
`EIDOS_SERVICE_PASSWORD` again.
Removal keeps the data folder unless you tick *Also delete the indexed
data*; the files that were indexed are never touched.

The disposable Windows installer workflow seeds four synthetic text files and
checks exact content results, source-file hashes, saved source/resource limits
and pause state after machine upgrade, repair and data-preserving reinstall.
Every seeded limit differs from its default and the service must not have
rebuilt its content index, so state lost during one of those operations cannot
satisfy the checks by falling back to a default or being reconstructed.
It also checks stable fleet identity. These checks use unsigned development
packages; they do not establish signed release or real-machine qualification.

### Preparing a newer release

`eidos updates check` records a compatible newer canonical release.
`eidos updates configure --expected-publisher '<certificate subject>'` pins the
required Authenticode publisher, and `eidos updates stage` downloads and
verifies the setup without running it. The Nodes page offers the same check,
configuration, and staging controls. Staged files remain under the Eidos data
directory and interrupted or failed verification is reported after restart.

This is not an upgrade command. Install and fleet rollout remain disabled
until the drain/install/restart/health lifecycle is implemented and qualified.

A release newer than this build but not a compatible upgrade for it is
reported as information; it is never staged.

### Retiring an existing collector

The recovery installer does not carry or adopt the profiling collector.
Before upgrading, remove **eidos observatory collector** from Settings > Apps
using its existing uninstall entry. Keep study data (the default); do not
select its data-purge option. If the entry is missing, use the original
collector installer to uninstall it. Check that `eidos-collector` is no
longer registered in Services before running the new setup. Setup and the
bare MSI block installation while the legacy package/service is detected,
without changing its data or the core catalog. Do not delete registry keys
to bypass this check: that would leave the service or package behind.

Collector spool, configuration and study key are not used by the core. Keep
any required archive independently; recovery does not delete it automatically.

## Unattended installs

```powershell
eidos-setup.exe /quiet EIDOS_SCOPE=perMachine EIDOS_PORT=7700
eidos-setup.exe /quiet EIDOS_SCOPE=perMachine EIDOS_SERVICE_ACCOUNT_KIND=user EIDOS_SERVICE_DOMAIN=CORP EIDOS_SERVICE_USER=svc-eidos EIDOS_SERVICE_PASSWORD=...
eidos-setup.exe /passive                               # per-user, progress only
eidos-setup.exe /quiet /uninstall EIDOS_REMOVE_DATA=1
```

Variables: `EIDOS_SCOPE` (`perUser` | `perMachine`), `EIDOS_INSTALLDIR`,
`EIDOS_DATADIR`, `EIDOS_BIND`, `EIDOS_PORT`, `EIDOS_SERVICE_ACCOUNT_KIND`
(`local-system` | `local-service` | `network-service` | `user`),
`EIDOS_SERVICE_DOMAIN`, `EIDOS_SERVICE_USER`, `EIDOS_SERVICE_PASSWORD`,
`EIDOS_START_SERVICE`, `EIDOS_START_MENU`, `EIDOS_REMOVE_DATA` (`1`/`0`).
Setup logs are written to `%TEMP%\eidos_<timestamp>.log` (`/log <path>`
to choose).

Administrators who prefer the bare package can use `eidos-<version>.msi`
with the same properties (`msiexec /i eidos.msi ALLUSERS=1 EIDOS_PORT=7700`
for a machine install; without `ALLUSERS=1` it installs per-user).

## Fleet

Several installations can replicate their catalog metadata into one
master and search the union; see [fleet.md](fleet.md). Setup does not join a
host automatically. Designate the master with `eidos fleet master`, then
select the advertised master or enter its IP address on the joining host. The
master must approve the request from its Nodes page before synchronization
starts. The dedicated sync listener uses port 7710 by default.

## Troubleshooting

- *"Setup did not finish"* — the failure page links the log; the MSI log
  next to it (`…_000_EidosMsi.log`) has the failing action.
- The service does not start — `eidos service status` shows the exit
  reason; the service log is in the data folder's `logs`. A port already in
  use or a data folder the service account cannot write are the usual
  causes.
- Standard users on Windows Server cannot install per-user (Windows
  Installer policy); use *All users*.
