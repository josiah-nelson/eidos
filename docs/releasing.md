# Releasing

Published GitHub releases receive a signed Windows x86-64 installer built by
`.github/workflows/release.yml`:

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

CI (`ci.yml`, job `installer`) builds the same MSI/UI/bundle from a debug
executable on every pull request, installs it silently per-user on the
runner, checks `/api/health`, uninstalls with data removal, and keeps the
unsigned `eidos-setup.exe` as a workflow artifact for manual testing.

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

Run the workflow manually before publishing the first release:

```powershell
gh workflow run release.yml --ref main
gh run watch
```

A manual run uploads the signed installer and MSI as a seven-day workflow
artifact and does not alter a GitHub release. Inspect the downloaded files
with:

```powershell
Get-AuthenticodeSignature .\eidos-manual-abc1234-setup.exe | Format-List Status,StatusMessage,SignerCertificate
```

To publish, create a release for the desired tag. The `release: published`
event builds that tagged revision and uploads the assets. The version inside
the installer comes from the workspace `version` in `Cargo.toml` with any
pre-release suffix removed (Windows Installer versions are numeric), so bump
it before tagging; a same-version rebuild is not a major upgrade.

Client-secret authentication is supported by the Artifact Signing action and
matches the currently provisioned repository secrets. OpenID Connect is the
preferred follow-up because it removes the long-lived client secret; it
requires a federated credential for this repository before the workflow can be
switched.
