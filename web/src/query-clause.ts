/**
 * Editing the search box when a facet value is clicked.
 *
 * Clauses combine with AND, so appending one only ever narrows the query and
 * never changes what the rest of it means. Two rules keep repeated clicks
 * sane, both of them purely textual — nothing here re-derives a boundary,
 * the server hands every bucket its exact clause:
 *
 *   1. Clicking a value that is already in the query is a no-op, and
 *      clicking include on an excluded value (or the reverse) swaps the two
 *      forms instead of contradicting them.
 *   2. The buckets of a range facet are disjoint, so intersecting two of
 *      them can only give nothing. Selecting one therefore drops whatever
 *      other bucket of the same facet is in the query. Bounds typed by hand
 *      are left alone: `size:>=100` plus the "< 4 KiB" bucket stays the
 *      intersection of both.
 */

/**
 * Split a query into top-level tokens, keeping quoted strings and
 * parenthesised groups whole (`-(size:>=1M size:<16M)` is one token).
 */
export function tokenize(query: string): string[] {
  const out: string[] = []
  let cur = ''
  let quoted = false
  let depth = 0
  for (const ch of query) {
    if (ch === '"') {
      quoted = !quoted
    } else if (!quoted && ch === '(') {
      depth++
    } else if (!quoted && ch === ')') {
      depth = Math.max(0, depth - 1)
    } else if (!quoted && depth === 0 && /\s/.test(ch)) {
      if (cur) out.push(cur)
      cur = ''
      continue
    }
    cur += ch
  }
  if (cur) out.push(cur)
  return out
}

function indexOfRun(tokens: string[], run: string[]): number {
  if (run.length === 0) return -1
  for (let i = 0; i + run.length <= tokens.length; i++) {
    if (run.every((t, j) => tokens[i + j] === t)) return i
  }
  return -1
}

/** Remove every occurrence of a contiguous token run. */
function removeRun(tokens: string[], run: string[]): string[] {
  let out = tokens
  for (;;) {
    const at = indexOfRun(out, run)
    if (at < 0) return out
    out = [...out.slice(0, at), ...out.slice(at + run.length)]
  }
}

/**
 * Append `clause` to `query`, first removing any of `replaces` (the opposite
 * include/exclude form of the same value, and the clauses of sibling buckets
 * of the same range facet).
 */
export function applyClause(query: string, clause: string, replaces: string[] = []): string {
  const want = tokenize(clause)
  let tokens = tokenize(query)
  for (const other of replaces) {
    if (!other || other === clause) continue
    tokens = removeRun(tokens, tokenize(other))
  }
  if (indexOfRun(tokens, want) >= 0) return tokens.join(' ')
  return [...tokens, ...want].join(' ')
}

/** The negated form of a clause; groups it when it is more than one token. */
export function negate(clause: string): string {
  return tokenize(clause).length > 1 ? `-(${clause})` : `-${clause}`
}
