# Releasing

Pushing a tag that starts with `v` builds and signs the Windows x86-64
installer in `.github/workflows/release.yml` and publishes it on the GitHub
release for that tag:

- `eidos-<tag>-setup.exe` — the guided installer (Burn bundle with the setup
  UI and the MSI); this is what people download.
- `eidos-<tag>.msi` — the bare package for administrators and unattended
  installs (`msiexec /i ... ALLUSERS=1 EIDOS_PORT=7700`).
- `.sha256` checksums for both.

The workflow builds the web UI, the Rust executable (with the UI embedded),
the setup UI, the MSI and the bundle, and signs every executable piece with
Azure Artifact Signing in the order the Windows Installer and Burn require:

1. `eidos.exe` and `eidos-setup-ui.exe` (before they are bound into the MSI
   and bundle);
2. `eidos.msi`;
3. the Burn engine, detached from the bundle with `wix burn detach`, then
   reattached with `wix burn reattach` — this is what the UAC prompt and
   Programs and Features show for repair/uninstall;
4. the finished `eidos-<tag>-setup.exe`.

Every signature and timestamp is verified with `Get-AuthenticodeSignature`
before any asset is uploaded; a failure anywhere uploads nothing. See
`installer/README.md` for the authoring and `installer/build.ps1` for the
stages the workflow calls (`-SkipMsi`, `-SkipUi`, `-SkipBundle`).

Nothing is built or signed on a pull request. The same MSI/UI/bundle can be
exercised unsigned on demand with `installer.yml`, which builds from a debug
executable, installs silently per-user on the runner, checks `/api/health`,
uninstalls with data removal, and keeps `eidos-setup.exe` as a workflow
artifact:

```powershell
gh workflow run installer.yml --ref <branch>
```

Run it before tagging whenever a change touches `installer/`, `build.ps1`, or
the way the web UI is embedded — ordinary CI does not cover any of that.

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

## Test and publish

Run the workflow manually against a branch to rehearse signing without
touching a release:

```powershell
gh workflow run release.yml --ref main
gh run watch
```

A manual run names the assets `eidos-manual-<sha>-*`, uploads them as a
seven-day workflow artifact, and publishes nothing. Inspect the downloaded
files with:

```powershell
Get-AuthenticodeSignature .\eidos-manual-abc1234-setup.exe | Format-List Status,StatusMessage,SignerCertificate
```

To publish, bump the workspace `version` in `Cargo.toml`, then tag and push:

```powershell
git tag v0.4.0
git push origin v0.4.0
```

The tag push builds that revision, signs it, and publishes the release: it
creates the release with generated notes when the tag has none yet, and
uploads to an existing release (replacing same-named assets) when one is
already there — so drafting release notes first and then pushing the tag
works equally well. A tag with a pre-release suffix (`v0.4.0-rc.1`) is
published as a pre-release.

The version inside the installer comes from the workspace `version` in
`Cargo.toml` with any pre-release suffix removed (Windows Installer versions
are numeric), so bump it before tagging; a same-version rebuild is not a
major upgrade.

Client-secret authentication is supported by the Artifact Signing action and
matches the currently provisioned repository secrets. OpenID Connect is the
preferred follow-up because it removes the long-lived client secret; it
requires a federated credential for this repository before the workflow can be
switched.
