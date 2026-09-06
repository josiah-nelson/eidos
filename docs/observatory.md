# Profiling collector — retired

The observatory collector is removed from the recovery build. It is not
part of the supported product, workspace, API, UI, installers or releases.
The `eidos observe` command no longer exists.

The failed v0.5.0 rollout showed that another privileged background pipeline
was not an acceptable recovery dependency. Measure core CPU, memory, disk I/O,
queue depth and idle behavior directly, without deploying a collector.

- [Windows retirement and data-preserving migration](installing.md#retiring-an-existing-collector)
- [Recovery requirements and qualification](recovery.md)
- [ADR-0026](adr/0026-retire-profiling-collector.md)

Historical collector ADRs remain as decision history, not installation
instructions. Existing study data is preserved by default when the old
package is uninstalled; the core does not consume or delete it.
