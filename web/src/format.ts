const UNITS = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB']
export type IntegerLike = string | number | bigint

/** Deliberately lossy conversion for visual scales and elapsed-time display. */
export function integerNumber(n: IntegerLike): number {
  return Number(n)
}

export function bytes(n: IntegerLike | null | undefined, digits = 1): string {
  if (n == null) return '—'
  const exact = typeof n === 'number' ? null : BigInt(n)
  if (exact !== null && exact < 1024n) return `${exact} B`
  let v = integerNumber(n)
  if (v < 1024) return `${v} B`
  let i = 0
  while (v >= 1024 && i < UNITS.length - 1) {
    v /= 1024
    i++
  }
  return `${v.toFixed(digits)} ${UNITS[i]}`
}

export function count(n: IntegerLike | null | undefined): string {
  if (n == null) return '—'
  return (typeof n === 'number' ? n : BigInt(n)).toLocaleString()
}

/** Unix nanoseconds -> local date-time string. */
export function when(nanos: IntegerLike | null | undefined): string {
  if (nanos == null) return '—'
  const d = new Date(typeof nanos === 'number' ? nanos / 1e6 : Number(BigInt(nanos) / 1_000_000n))
  if (Number.isNaN(d.getTime())) return '—'
  return d.toLocaleString(undefined, {
    year: 'numeric',
    month: 'short',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
  })
}

export function ago(nanos: IntegerLike | null | undefined): string {
  if (nanos == null) return 'never'
  const timestamp = typeof nanos === 'number' ? nanos / 1e6 : Number(BigInt(nanos) / 1_000_000n)
  const ms = Date.now() - timestamp
  if (ms < 0) return 'just now'
  const s = Math.floor(ms / 1000)
  if (s < 60) return `${s}s ago`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m ago`
  const h = Math.floor(m / 60)
  if (h < 48) return `${h}h ago`
  return `${Math.floor(h / 24)}d ago`
}

export function duration(value: IntegerLike): string {
  const ms = integerNumber(value)
  if (ms < 1000) return `${Math.round(ms)} ms`
  const s = ms / 1000
  if (s < 60) return `${s.toFixed(1)} s`
  const m = Math.floor(s / 60)
  return `${m}m ${Math.round(s - m * 60)}s`
}

export function rate(n: number): string {
  if (n >= 1e6) return `${(n / 1e6).toFixed(2)} M/s`
  if (n >= 1e3) return `${(n / 1e3).toFixed(1)} k/s`
  return `${Math.round(n)}/s`
}

export function attrFlags(a: number): string {
  const table: [number, string][] = [
    [0x1, 'R'],
    [0x2, 'H'],
    [0x4, 'S'],
    [0x20, 'A'],
    [0x200, 'P'],
    [0x400, 'L'],
    [0x800, 'C'],
    [0x1000, 'O'],
    [0x4000, 'E'],
  ]
  return table
    .filter(([bit]) => (a & bit) !== 0)
    .map(([, c]) => c)
    .join('')
}

export function humanState(s: string): string {
  return s.replace(/_/g, ' ')
}
