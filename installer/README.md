# Windows installer

The recovery build packages only the core:

| Project | Artifact | Purpose |
|---|---|---|
| Eidos.Msi | eidos.msi | Dual-scope core package |
| Eidos.Setup.Ui | eidos-setup-ui.exe | Guided setup UI |
| Eidos.Bundle | eidos-setup.exe | Core MSI, UI and .NET prerequisite |

No collector crates, payloads, services, installer choices or release assets
are built. Old collector installations must be retired through their original
uninstall entry before upgrade; both setup and the bare MSI enforce this
without deleting core or study data. See [migration instructions](../docs/installing.md#retiring-an-existing-collector)
and [ADR-0026](../docs/adr/0026-retire-profiling-collector.md).

## Build

Requires Node.js, Rust and .NET SDK 8+. WiX 7 is restored from NuGet with its
OSMF EULA accepted in the project. Run from the repository root:

```powershell
.\installer\build.ps1
.\installer\build.ps1 -SkipWeb -SkipRust -BinDir target\debug
```

Output is in installer/out. The SkipWeb, SkipRust, SkipMsi, SkipUi and
SkipBundle flags each skip one stage. Release signing builds and signs the
executable and UI, then the MSI, then the detached Burn engine and final
bundle. See [releasing](../docs/releasing.md).

## Lifecycle gate

installer.yml is callable by the release workflow and manually. It checks
per-user startup choices, machine install/upgrade/repair, stable node identity,
retirement detection, rejected unattended values, data-preserving removal,
reinstall and explicit core-data purge. The retirement test uses a synthetic
legacy registration; a real old-MSI retirement rehearsal is also required
before rollout. Passing a WiX build alone does not qualify deployment.

Core Package Id and Bundle Id are unchanged, preserving upgrade identity.
Each WiX bind starts from validated project-local bin/obj directories so a
new executable path cannot silently reuse an older payload.
