# Releasing

Pushing a tag that starts with `v` builds and signs the Windows x86-64
installer in `.github/workflows/release.yml` and publishes it on the GitHub
release for that tag:

- `eidos-<tag>-setup.exe` — guided core installer.
- `eidos-<tag>.msi` — bare core package for administrators.
- `.sha256` checksums for both assets.

The collector is retired and is not built, signed or published. Existing
installations follow [the retirement path](installing.md#retiring-an-existing-collector).

Publication requires three successful jobs: the full Windows/web test gate,
the signed build, and the reusable installer lifecycle workflow. Azure
Artifact Signing signs the executable and setup UI, then the core MSI, then
the detached Burn engine, then the reattached setup executable. Each
signature and timestamp is verified before asset upload. Signing
infrastructure already exists; it must also be used by the planned pushed
update path, which is not implemented by the advisory version badge.

Run the unsigned lifecycle gate once for an installer change:

```powershell
gh workflow run installer.yml --ref <branch>
```

It tests per-user and machine installations, upgrade, repair, retirement
detection, invalid unattended values, retained data, reinstall and explicit
purge. Its synthetic retired-package registration is not a substitute for
rehearsing removal of a real older collector MSI. See [recovery.md](recovery.md).

## Release checklist

Do not tag the recovery release until the real-machine acceptance evidence
in [recovery.md](recovery.md) is recorded. Everything CI needs must be in the
tagged tree, but ordinary CI does not prove quiet disks or a safe fleet rollout.

1. Bump the workspace `version` in `Cargo.toml` (and `Cargo.lock`, via
   `cargo update -w --offline` or any build) and `web/package.json`
   (`npm version <version> --no-git-tag-version`) to the version the tag
   will carry, numeric, no `-dev`.
2. Write `docs/releases/<tag>.md` - the announcement the GitHub release is
   created with. Without it the release gets generated notes.
3. `scripts\check.ps1` passes on the release commit.
4. Push the tag from that commit. The workflow refuses a tag that does not
   match `Cargo.toml`, runs the full gate and the signed build in parallel,
   and publishes only when tests, signed build and installer lifecycle pass.

`installer.yml` also remains callable on demand. `sync-soak.yml` is a
protocol simulation; real-machine I/O and interruption evidence is separate.

## macOS

There is no macOS release job yet. `scripts/macos/build-agent.sh` produces
`dist/macos/Eidos.app` — the bundle the agent is installed from, because Full
Disk Access is only properly supported for bundled executables — signing with
a *Developer ID Application* identity when the keychain has one and ad-hoc
otherwise. `scripts/macos/sign-notarize.sh` builds that core app with the
embedded web UI, signs it with a Developer ID identity (including the temporary
keychain path), submits it to Apple, staples the accepted ticket and creates
`Eidos.app.zip`. It no longer builds or packages the collector. This path
still requires validation on macOS; a Windows build cannot qualify it.

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
