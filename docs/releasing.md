# Releasing

Published GitHub releases receive a Windows x86-64 package built by
`.github/workflows/release.yml`. The workflow builds the Rust executable and
web UI, signs `eidos.exe` with Azure Artifact Signing, verifies the Authenticode
signature and timestamp, and uploads the ZIP and its SHA-256 checksum to the
release. A signing or verification failure prevents any assets from being
uploaded.

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

A manual run uploads the signed package as a seven-day workflow artifact and
does not alter a GitHub release. Inspect the downloaded executable with:

```powershell
Get-AuthenticodeSignature .\eidos.exe | Format-List Status,StatusMessage,SignerCertificate
```

To publish, create a release for the desired tag. The `release: published`
event builds that tagged revision and uploads assets named
`eidos-<tag>-windows-x86_64.zip` and
`eidos-<tag>-windows-x86_64.zip.sha256`.

Client-secret authentication is supported by the Artifact Signing action and
matches the currently provisioned repository secrets. OpenID Connect is the
preferred follow-up because it removes the long-lived client secret; it
requires a federated credential for this repository before the workflow can be
switched.
