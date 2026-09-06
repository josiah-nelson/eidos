# ADR-0034: verified release staging precedes node installation

Status: accepted for recovery implementation.

## Decision

Eidos separates release discovery and artifact staging from installation and
fleet rollout. The service may discover only the latest release from the
project's fixed GitHub repository. A candidate is accepted only when it is a
compatible newer patch/minor version, has the exact
`eidos-v<version>-setup.exe` name and canonical release URL, is uploaded, fits
the configured byte ceiling, and carries GitHub's SHA-256 asset digest.

Staging writes beneath `<data>/updates/staging`, refuses bytes beyond both the
declared and configured sizes, hashes while downloading, flushes the temporary
file, and verifies it before the final rename. On Windows, verification uses
the operating system's Authenticode policy result and compares the exact
configured certificate subject, product name, and product version. A missing
publisher configuration, unsupported platform, invalid trust result, or any
mismatch fails closed. An unverified temporary file is removed and is never
executed.

Check and staging progress, errors, and the verified result are written under
`<data>/updates`. Startup converts an interrupted download or verification
phase into an explicit retryable failure. The current recovery slice exposes
manual and automatic checks plus verified staging through API, CLI, and Nodes.
It does not execute the installer or claim that any node was updated.

## Consequences

The future fleet update protocol can refer to a release version and digest,
instead of accepting an operator URL or executable command. Installation must
remain a separate reviewed lifecycle with drain, restart, health, retry,
serialization, and canary behavior. A real signed installer qualification is
still required before enabling that lifecycle.
