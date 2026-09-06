# ADR-0026: Retire the profiling collector

Date: 2026-09-05

Status: accepted; supersedes collector scope in ADR-0014, ADR-0020,
ADR-0021 and ADR-0022. Historical measurements remain historical evidence.

## Context

The v0.5.0 rollout failed operationally. Recovery must simplify the deployed
product and demonstrate fast initial indexing and quiet steady state.
A separate privileged observation pipeline adds background work, packaging,
configuration and failure modes that are not necessary for file search.

## Decision

Remove the observatory and both platform collectors from the workspace,
CLI, service API, web UI, Windows packaging and release assets. Retain core
operational metrics. Core content chunking keeps its historical hash-domain
constant: changing a label must not change stored content identity.

Do not silently orphan an installed collector. Windows setup and direct MSI
installation require retirement through the old package's uninstall entry
before proceeding. Keep its study data by default. Neither the retirement
gate nor the new installer deletes legacy data or indexed originals.

On macOS retain core app building, signing and notarization, without Endpoint
Security collector entitlements. Existing collector installations require
their original uninstall procedure with data preservation.

## Consequences

One deployed application remains. Recovery qualification measures its own
resource use directly. Old ADRs and release notes are history, not supported
commands. Legacy upgrade rehearsal is mandatory before rollout; synthetic
registry checks alone do not establish MSI lifecycle correctness.
