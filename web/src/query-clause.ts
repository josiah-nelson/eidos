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
 *      other bucket of the same facet is in the query. Only those exact
 *      clauses are touched: `size:>=100` plus the "< 4 KiB" bucket stays
 *      the intersection of both. Matching is textual, so a hand-written
 *      bound that reads exactly like a bucket is treated as that bucket —
 *      there is nothing to tell them apart, and keeping both would leave no
 *      results.
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

const isOperator = (token: string | undefined) =>
  token === 'OR' || token === '|' || token === 'AND' || token === '&&'

/**
 * Drop operators a removal left dangling. Taking `size:<4k` out of
 * `size:<4k OR ext:md` leaves a leading `OR`, which does not parse.
 */
function dropDanglingOperators(tokens: string[]): string[] {
  const out: string[] = []
  for (const t of tokens) {
    if (isOperator(t) && (out.length === 0 || isOperator(out[out.length - 1]))) continue
    out.push(t)
  }
  while (out.length > 0 && isOperator(out[out.length - 1])) out.pop()
  return out
}

/** Remove every occurrence of a contiguous token run. */
function removeRun(tokens: string[], run: string[]): string[] {
  let out = tokens
  for (;;) {
    const at = indexOfRun(out, run)
    if (at < 0) return out
    out = dropDanglingOperators([...out.slice(0, at), ...out.slice(at + run.length)])
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
  // `AND` binds tighter than `OR`, so appending to `a OR b` would filter the
  // last branch only. Group the query first: `(a OR b) ext:cs`.
  const base = tokens.some((t) => t === 'OR' || t === '|') ? [`(${tokens.join(' ')})`] : tokens
  return [...base, ...want].join(' ')
}

/** The clauses one facet value contributes: what selects it, what removes it. */
export interface FacetForms {
  clause: string
  exclude: string
}

/**
 * Apply a click on one facet value.
 *
 * `siblings` are the other values of the same facet that are mutually
 * exclusive with this one — the buckets of a range facet. Two term values
 * (`ext:cs` and `ext:md`) are not: intersecting them is empty but it is what
 * the user asked for, so nothing is passed for them.
 *
 * Selecting a bucket drops any other bucket of the same facet, since
 * intersecting disjoint buckets can only give nothing, and drops exclusions
 * of those buckets, which the selection already implies. Excluding one
 * removes only its own inclusion: exclusions of different buckets are
 * independent filters and accumulate.
 */
export function applyFacetClick(
  query: string,
  value: FacetForms,
  form: 'include' | 'exclude',
  siblings: FacetForms[] = [],
): string {
  if (form === 'exclude') return applyClause(query, value.exclude, [value.clause])
  return applyClause(query, value.clause, [
    value.exclude,
    ...siblings.flatMap((s) => [s.clause, s.exclude]),
  ])
}

/** The negated form of a clause; groups it when it is more than one token. */
export function negate(clause: string): string {
  return tokenize(clause).length > 1 ? `-(${clause})` : `-${clause}`
}
