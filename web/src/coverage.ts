import type {
  CoverageEnvelope,
  CoverageReason,
  SourceCoverage,
} from './generated/api.js'

function distinctReasons(reasons: CoverageReason[]): CoverageReason[] {
  const seen = new Set<string>()
  return reasons.filter((reason) => {
    const key = `${reason.kind}\0${reason.severity}\0${reason.detail}\0${reason.remediation ?? ''}`
    if (seen.has(key)) return false
    seen.add(key)
    return true
  })
}

/** Merge the coverage contract for every page whose hits remain visible. */
export function mergeCoverage(
  pages: ReadonlyArray<{ coverage: CoverageEnvelope }>,
): CoverageEnvelope | undefined {
  if (pages.length === 0) return undefined

  const sources = new Map<SourceCoverage['source_id'], SourceCoverage>()
  for (const page of pages) {
    for (const source of page.coverage.sources) {
      const previous = sources.get(source.source_id)
      sources.set(source.source_id, {
        ...source,
        degraded: distinctReasons([...(previous?.degraded ?? []), ...(source.degraded ?? [])]),
      })
    }
  }

  return {
    full: pages.every((page) => page.coverage.full),
    degraded: distinctReasons(pages.flatMap((page) => page.coverage.degraded ?? [])),
    sources: Array.from(sources.values()),
  }
}
