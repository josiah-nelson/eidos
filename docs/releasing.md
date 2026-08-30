# Releasing

Pushing a tag that starts with `v` builds and signs the Windows x86-64
installer in `.github/workflows/release.yml` and publishes it on the GitHub
release for that tag:

- `eidos-<tag>-setup.exe` — the guided installer (Burn bundle with the setup
  UI, the core MSI and the optional collector MSI); this is what people
  download.
- `eidos-<tag>.msi`, `eidos-collector-<tag>.msi`,
  `eidos-collector-<tag>-setup.exe` — the bare packages and the
  collector-only setup for administrators and fleet installs.
- `.sha256` checksums for every asset.

The workflow runs two jobs in parallel - the full test gate (format, lint,
the generated API contract, every Rust test on Windows via cargo-nextest,
and the web lint/tests/build) and the signed build - and a third publishes
only when both are green, so a red suite wastes some signing work but
publishes nothing. The build job builds the web UI, the Rust executable
(with the UI embedded), the setup UI, both MSIs and both bundles, and signs
every executable piece with Azure Artifact Signing in the order the Windows
Installer and Burn require:

1. `eidos.exe` and `eidos-setup-ui.exe` (before they are bound into the MSI
   and bundle);
2. `eidos.msi` and `eidos-collector.msi`;
3. both Burn engines, detached from the bundles with `wix burn detach`, then
   reattached with `wix burn reattach` — this is what the UAC prompt and
   Programs and Features show for repair/uninstall;
4. the finished unified and collector-only setup executables.

Every signature and timestamp is verified with `Get-AuthenticodeSignature`
before any asset is uploaded; a failure anywhere uploads nothing. See
`installer/README.md` for the authoring and `installer/build.ps1` for the
stages the workflow calls (the core and `-SkipCollector*` stage flags).

Nothing is built or signed on a pull request. The same MSIs, UI and bundles can be
exercised unsigned on demand with `installer.yml`, which builds from a debug
executable, exercises the lifecycle paths below, and keeps the installer
artifacts and logs as a workflow artifact:

```powershell
gh workflow run installer.yml --ref <branch>
```

Run it before tagging whenever a change touches `installer/`, `build.ps1`, or
the way the web UI is embedded — ordinary CI does not cover any of that. It
covers core-only per-user, a per-user core with the per-machine collector,
core-plus-collector per-machine through the unified setup, same-version
adoption and later upgrade from the separate collector package, repair,
uninstall keeping data, and explicit purge of exactly one product's data.

## Release checklist

The tag is the release. Everything the workflow needs is in the tree it
tags; there is no separate rehearsal to pass first.

1. Bump the workspace `version` in `Cargo.toml` (and `Cargo.lock`, via
   `cargo update -w --offline` or any build) and `web/package.json`
   (`npm version <version> --no-git-tag-version`) to the version the tag
   will carry, numeric, no `-dev`.
2. Write `docs/releases/<tag>.md` - the announcement the GitHub release is
   created with. Without it the release gets generated notes.
3. `scripts\check.ps1` passes on the release commit.
4. Push the tag from that commit. The workflow refuses a tag that does not
   match `Cargo.toml`, runs the full gate and the signed build in parallel,
   and publishes only when both are green.

`installer.yml` (lifecycle rehearsal on an unsigned build) and
`sync-soak.yml` stay on demand for changes that touch what they cover.

## macOS

There is no macOS release job yet. `scripts/macos/build-agent.sh` produces
`dist/macos/Eidos.app` — the bundle the agent is installed from, because Full
Disk Access is only properly supported for bundled executables — signing with
a *Developer ID Application* identity when the keychain has one and ad-hoc
otherwise. `scripts/macos/sign-notarize.sh` already carries the notarisation
path used for the observatory collector (temporary keychain from
`APPLE_CERTIFICATE_P12`, `notarytool submit --wait`, `stapler staple`); a
macOS release job reuses it for the agent bundle and publishes the notarised
`Eidos.app`.

Until then, macOS is installed from source: see
[installing-macos.md](installing-macos.md).

## Azure and GitHub configuration

The signing principal needs the **Artifact Signing Certificate Profile
Signer** role on the certificate profile. Use a Public Trust certificate
profile for publicly distributed builds.

Configure these Actions secrets in the `release` GitHub environment (repository
secrets also work):

- `AZURE_CLIENT_ID`
- `AZURE_CLIENT_SECRET`
- `AZURE_TENANT_ID`

The existing `AZURE_SUBSCRIPTION_ID` secret is not needed by the signing action.
It can be retained for Azure administration or a future move to OpenID Connect.

Configure these non-secret Actions variables in the `release` environment or
at repository scope:

- `AZURE_ARTIFACT_SIGNING_ENDPOINT` — the account endpoint, such as
  `https://eus.codesigning.azure.net/`
- `AZURE_ARTIFACT_SIGNING_ACCOUNT_NAME`
- `AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE_NAME`

For example:

```powershell
gh variable set AZURE_ARTIFACT_SIGNING_ENDPOINT --body "https://eus.codesigning.azure.net/"
gh variable set AZURE_ARTIFACT_SIGNING_ACCOUNT_NAME --body "<account-name>"
gh variable set AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE_NAME --body "<profile-name>"
```

The endpoint must match the region in which the Artifact Signing account and
certificate profile were created.

## Publish

Bump the workspace `version` in `Cargo.toml` and `web/package.json`, commit,
then tag and push:

```powershell
git tag v0.5.0
git push origin v0.5.0
```

The tag push tests, builds and signs that revision and publishes the
release: it creates the release from `docs/releases/v0.5.0.md` (generated
notes when there is no such file) when the tag has none yet, and uploads to
an existing release (replacing same-named assets) when one is already there,
so drafting the release on GitHub first works equally well. A tag with a
pre-release suffix (`v0.5.1-rc.1`) is published as a pre-release. The
`release` environment only admits `v*` tags, so the first signing run of a
version is the release itself; a bad build fails before anything is
published, and a fix is a new patch tag.

The version inside the installer comes from the same workspace `version`
with any pre-release suffix removed (Windows Installer versions are
numeric); a same-version rebuild is not a major upgrade.

Client-secret authentication is supported by the Artifact Signing action and
matches the currently provisioned repository secrets. OpenID Connect is the
preferred follow-up because it removes the long-lived client secret; it
requires a federated credential for this repository before the workflow can be
switched.
