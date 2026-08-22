import { useEffect, useRef, useState } from 'react'
import { useNavigate } from 'react-router'
import { bytes } from './format'

export interface TreemapItem {
  id: number
  label: string
  value: number
  isDir: boolean
  detail?: string
}

interface Rect {
  x: number
  y: number
  w: number
  h: number
  item: TreemapItem
}

/** Squarified treemap layout (Bruls, Huizing, van Wijk). */
function squarify(items: TreemapItem[], x: number, y: number, w: number, h: number): Rect[] {
  const total = items.reduce((s, i) => s + i.value, 0)
  if (total <= 0 || w <= 0 || h <= 0) return []
  const sorted = [...items].filter((i) => i.value > 0).sort((a, b) => b.value - a.value)
  const out: Rect[] = []
  let rx = x
  let ry = y
  let rw = w
  let rh = h
  let row: TreemapItem[] = []
  let i = 0
  const area = (v: number) => (v / total) * w * h
  const worst = (r: TreemapItem[], side: number) => {
    const s = r.reduce((a, b) => a + area(b.value), 0)
    if (s === 0) return Infinity
    const mx = Math.max(...r.map((t) => area(t.value)))
    const mn = Math.min(...r.map((t) => area(t.value)))
    return Math.max((side * side * mx) / (s * s), (s * s) / (side * side * mn))
  }
  const layoutRow = (r: TreemapItem[]) => {
    const s = r.reduce((a, b) => a + area(b.value), 0)
    const vertical = rw >= rh
    if (vertical) {
      const cw = s / rh
      let cy = ry
      for (const t of r) {
        const ch = area(t.value) / cw
        out.push({ x: rx, y: cy, w: cw, h: ch, item: t })
        cy += ch
      }
      rx += cw
      rw -= cw
    } else {
      const ch = s / rw
      let cx = rx
      for (const t of r) {
        const cw = area(t.value) / ch
        out.push({ x: cx, y: ry, w: cw, h: ch, item: t })
        cx += cw
      }
      ry += ch
      rh -= ch
    }
  }
  while (i < sorted.length) {
    const side = Math.min(rw, rh)
    const next = sorted[i]
    if (row.length === 0 || worst([...row, next], side) <= worst(row, side)) {
      row.push(next)
      i++
    } else {
      layoutRow(row)
      row = []
    }
  }
  if (row.length) layoutRow(row)
  return out
}

const PALETTE = ['#3b82f6', '#10b981', '#f59e0b', '#ef4444', '#8b5cf6', '#06b6d4', '#84cc16', '#f97316', '#ec4899', '#14b8a6']

export function Treemap({ items, total }: { items: TreemapItem[]; total: number }) {
  const ref = useRef<HTMLCanvasElement>(null)
  const wrap = useRef<HTMLDivElement>(null)
  const [rects, setRects] = useState<Rect[]>([])
  const [tip, setTip] = useState<{ x: number; y: number; text: string } | null>(null)
  const navigate = useNavigate()

  useEffect(() => {
    const canvas = ref.current
    const box = wrap.current
    if (!canvas || !box) return
    const draw = () => {
      const dpr = window.devicePixelRatio || 1
      const w = box.clientWidth
      const h = box.clientHeight
      canvas.width = w * dpr
      canvas.height = h * dpr
      const ctx = canvas.getContext('2d')
      if (!ctx) return
      ctx.scale(dpr, dpr)
      ctx.clearRect(0, 0, w, h)
      const shown = items.slice(0, 60)
      const rest = items.slice(60).reduce((s, i) => s + i.value, 0)
      const data = rest > 0 ? [...shown, { id: -1, label: `${items.length - 60} more`, value: rest, isDir: false }] : shown
      const layout = squarify(data, 0, 0, w, h)
      setRects(layout)
      const dark = matchMedia('(prefers-color-scheme: dark)').matches
      layout.forEach((r, i) => {
        const color = r.item.id === -1 ? (dark ? '#374151' : '#cbd5e1') : PALETTE[i % PALETTE.length]
        ctx.fillStyle = color
        ctx.globalAlpha = r.item.isDir ? 0.85 : 0.55
        ctx.fillRect(r.x + 0.5, r.y + 0.5, Math.max(0, r.w - 1), Math.max(0, r.h - 1))
        ctx.globalAlpha = 1
        if (r.w > 48 && r.h > 16) {
          ctx.fillStyle = dark ? '#f3f4f6' : '#111827'
          ctx.font = '11px Segoe UI, system-ui, sans-serif'
          const label = r.item.label
          let text = label
          while (text.length > 2 && ctx.measureText(text).width > r.w - 8) text = text.slice(0, -2)
          if (text !== label) text = text.slice(0, -1) + '…'
          ctx.fillText(text, r.x + 4, r.y + 12)
          if (r.h > 30) {
            ctx.fillStyle = dark ? '#d1d5db' : '#374151'
            ctx.fillText(bytes(r.item.value), r.x + 4, r.y + 25)
          }
        }
      })
    }
    draw()
    const ro = new ResizeObserver(draw)
    ro.observe(box)
    return () => ro.disconnect()
  }, [items])

  const hit = (e: React.MouseEvent) => {
    const box = wrap.current
    if (!box) return null
    const b = box.getBoundingClientRect()
    const x = e.clientX - b.left
    const y = e.clientY - b.top
    return rects.find((r) => x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h) ?? null
  }

  return (
    <div
      className="treemap"
      ref={wrap}
      onMouseMove={(e) => {
        const r = hit(e)
        if (!r) return setTip(null)
        const b = wrap.current!.getBoundingClientRect()
        const pct = total > 0 ? ((100 * r.item.value) / total).toFixed(1) : '0'
        setTip({
          x: Math.min(e.clientX - b.left + 12, b.width - 200),
          y: Math.min(e.clientY - b.top + 12, b.height - 30),
          text: `${r.item.label} · ${bytes(r.item.value)} (${pct}%)${r.item.detail ? ' · ' + r.item.detail : ''}`,
        })
      }}
      onMouseLeave={() => setTip(null)}
      onClick={(e) => {
        const r = hit(e)
        if (r && r.item.isDir && r.item.id > 0) navigate(`/browse/${r.item.id}`)
      }}
      title="apparent size of direct children; click a folder to open it"
    >
      <canvas ref={ref} />
      {tip && (
        <div className="tip" style={{ left: tip.x, top: tip.y }}>
          {tip.text}
        </div>
      )}
    </div>
  )
}
