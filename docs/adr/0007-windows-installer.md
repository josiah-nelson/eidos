# ADR-0007: Windows installer — dual-scope MSI, guided bundle, service and background modes

Status: Accepted (2026-08-24)

## Context

eidos ships as one Windows executable with the web UI embedded (ADR-0001
stack; the embedding landed with the packaging work). Until now the release
was a ZIP that the operator unpacked and ran by hand. v0.5 needs a real
setup that:

- installs either **for one user without administrator rights** or **for
  the machine as a Windows service**, from the same download;
- asks only for what is hard to change later (folders, port, listen
  address, service account) and leaves everything else to the product's
  first run;
- upgrades, repairs and uninstalls without losing the catalog and indexes
  unless the user asks;
- is signed end to end so SmartScreen and UAC show the publisher.

## Decision

**Toolchain.** WiX v7 (`WixToolset.Sdk/7.0.0`, OSMF EULA accepted in the
project files). v7 is the first release whose bundles let the bootstrapper
choose per-user or per-machine at install time ("configurable-scope
bundles"); Heat is gone, which does not matter because the package carries
three files.

**Package.** One MSI, `Package/@Scope="perUserOrMachine"`. The executable is
authored twice in mutually exclusive components — per-machine with the
service tables, per-user without — so a single package serves both scopes
without custom actions (ICE30 and ICE105 are suppressed for exactly that
pattern). Service registration is MSI-native (`ServiceInstall`,
`ServiceControl`, `ServiceConfig` for delayed start and pre-shutdown,
`util:ServiceConfig` for restart-on-failure, `util:User LogonAsService`
for a named account, `util:PermissionEx` on the data folder) so passwords
never touch a command line and rollback is the installer's. Every choice is
remembered in `HKMU\Software\eidos`; repair, upgrade and uninstall read it
back, and `util:RemoveFolderEx` deletes the data only on request and never
during an upgrade.

**Setup UI.** A custom managed bootstrapper application (.NET Framework
4.7.2, WPF; `WixToolset.BootstrapperApplicationApi`). The stock WixStdBA
theme engine has no password control and no room for live validation, and
the questions the setup asks (account verification with `LogonUser`, port
availability, folder picker) need code. .NET Framework is inbox on every
supported Windows, so the bundle carries no runtime; a 1.4 MB web
prerequisite covers the rare host without 4.7.2, and the BA drops it from
the plan whenever it is itself running so an unelevated per-user install
never touches the per-machine package cache.

**Per-user run mode.** `eidos serve --detach` starts the same service as a
windowless background process of the signed-in user, waits for
`/api/health`, and returns. The setup, the Start-menu shortcut and an HKCU
`Run` entry all use it. Upgrade and uninstall close a running `eidos.exe`
(`util:CloseApplication`) before touching files; the catalog is crash-safe
(the `restart_retention` contract), so termination loses nothing durable.

**Service control.** `eidos service install|start|stop|restart|status|
uninstall|run` on the SCM, with the same registration policy the MSI
applies (delayed auto-start, failure actions, pre-shutdown window, account
rights and ACLs), so operators and scripts have one vocabulary and the
installer is not the only way to register the service.

**Signing.** In CI: `eidos.exe` and the setup UI before they are bound; the
MSI; the Burn engine detached, signed and reattached; then the bundle. Each
signature is verified before upload. The release assets are
`eidos-<tag>-setup.exe`, `eidos-<tag>.msi`, and checksums; the ZIP is gone.

## Consequences

- One download, two products: the same `eidos-setup.exe` yields a
  per-user background process or a machine-wide service. Silent installs
  select the scope with `EIDOS_SCOPE`.
- The MSI runs directly (`msiexec`) with per-user defaults, which suits
  administrators and image builders; the guided experience lives in the
  bundle.
- Running the MSI twice with the same version is a no-op, and a same-version
  bundle rebuild does not upgrade: bump the workspace version before
  tagging.
- The per-user process is terminated rather than asked to stop on
  upgrade/uninstall. A graceful stop request (named event or loopback
  endpoint) is a possible refinement; it is not needed for correctness.
- Windows Server rejects per-user MSI installs for standard users by
  default (`DisableMSI`); per-machine is the supported scope there.
- Explorer verbs (PT-2) and portable mode (PT-1) are entry points over this
  same layout; they must not introduce a second copy of these rules.
