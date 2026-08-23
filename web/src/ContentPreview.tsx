import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { ApiError, api, type PreviewChunk, type PreviewWindow } from './api'
import { ContentBadge, ErrorBox, Spinner } from './components'
import { bytes, count } from './format'
import { stepWindow } from './preview-window'

/**
 * Panel over the text the indexer *stored* for one object generation.
 *
 * It is not the file: it is the catalog's extraction cache, so it can be
 * partial, it can lag the file on disk, and it stops at the server's window
 * limits. Every one of those facts is labelled rather than implied. Text is
 * rendered as text nodes only — never as markup.
 */
export function ContentPreviewPanel({
  objectId,
  name,
  generation,
  ordinal,
  onClose,
}: {
  objectId: number
  name: string
  /** Generation the caller believes it read (from a hit's content summary). */
  generation?: number
  ordinal: number
  onClose: () => void
}) {
  const [win, setWin] = useState<PreviewWindow>({ ordinal, before: 1, after: 1, generation })
  const preview = useQuery({
    queryKey: ['content-preview', objectId, win.ordinal, win.before, win.after, win.generation],
    queryFn: () => api.contentPreview(objectId, win),
    retry: false,
  })
  const p = preview.data
  const max = p?.limits.max_neighbors ?? 4
  const conflict = preview.error instanceof ApiError && preview.error.kind === 'stale_generation'

  // Widening only helps while the server still returns the whole window; past
  // that (its neighbour limit, or its byte budget) the window steps outwards
  // so the next chunk becomes the requested one. See `preview-window.ts`.
  const step = (direction: 'up' | 'down') => () =>
    setWin((w) => {
      const chunks = p?.chunks ?? []
      if (chunks.length === 0) return w
      const shown = { first: chunks[0].ordinal, last: chunks[chunks.length - 1].ordinal, max }
      return stepWindow(w, shown, direction)
    })
  const moreAbove = step('up')
  const moreBelow = step('down')

  return (
    <div className="preview-backdrop" onClick={onClose}>
      <div className="preview-panel" onClick={(e) => e.stopPropagation()}>
        <div className="preview-head">
          <div className="grow">
            <div className="name">{name}</div>
            <div className="path mono">{p?.path ?? ''}</div>
          </div>
          <button className="btn small" onClick={onClose}>
            Close
          </button>
        </div>

        <div className="banner">
          <span className="badge info">indexed text</span>
          <span>
            Stored extraction, not the file on disk.{' '}
            {p && (
              <>
                Generation {p.generation} · coverage {p.coverage} · {bytes(p.indexed_bytes)} of {bytes(p.total_bytes)}{' '}
                indexed in {count(p.chunk_count)} chunks
                {p.encoding ? ` · ${p.encoding}` : ''} <ContentBadge state={p.state} />
              </>
            )}
          </span>
        </div>

        {p && p.coverage !== 'full' && (
          <div className="banner warn">
            <span className="badge warn">partial</span>
            <span>
              Only the {p.coverage} of this file was extracted, so text outside the stored chunks is not searchable and
              cannot be shown. {p.reason ?? ''}
            </span>
          </div>
        )}
        {p?.stale && (
          <div className="banner warn">
            <span className="badge warn">out of date</span>
            <span>
              The file changed after this text was indexed (indexed generation {p.generation}, object now at{' '}
              {p.object_generation}). Re-extraction is still pending.
            </span>
          </div>
        )}
        {p?.truncated && (
          <div className="banner">
            <span className="badge accent">truncated</span>
            <span>
              The window was cut to the response limits ({bytes(p.limits.max_bytes)}, {count(p.limits.max_lines)} lines,{' '}
              {p.limits.max_neighbors} neighbouring chunks per side).
            </span>
          </div>
        )}
        {conflict && (
          <div className="banner warn">
            <span className="badge warn">generation changed</span>
            <span>
              This result refers to a version of the file the catalog no longer stores.{' '}
              <button className="btn small" onClick={() => setWin((w) => ({ ...w, generation: undefined }))}>
                Show current text
              </button>
            </span>
          </div>
        )}
        {preview.isError && !conflict && <ErrorBox error={preview.error} />}
        {preview.isPending && <Spinner label="Loading stored text…" />}

        {p && (
          <>
            <div className="toolbar">
              <button className="btn small" disabled={!p.has_more_before || preview.isFetching} onClick={moreAbove}>
                ▲ More above
              </button>
              <button className="btn small" disabled={!p.has_more_after || preview.isFetching} onClick={moreBelow}>
                ▼ More below
              </button>
              <div style={{ flex: 1 }} />
              <span className="muted small">
                chunks {p.chunks[0]?.ordinal ?? 0}–{p.chunks[p.chunks.length - 1]?.ordinal ?? 0} of{' '}
                {count(p.chunk_count)}
              </span>
            </div>
            <div className="preview-body">
              {p.chunks.map((c) => (
                <ChunkBlock key={c.ordinal} chunk={c} highlight={c.ordinal === p.requested_ordinal} />
              ))}
            </div>
          </>
        )}
      </div>
    </div>
  )
}

function ChunkBlock({ chunk, highlight }: { chunk: PreviewChunk; highlight: boolean }) {
  const lines = chunk.text.split('\n')
  return (
    <div className={`preview-chunk ${highlight ? 'current' : ''}`}>
      <div className="preview-chunk-head muted small">
        chunk {chunk.ordinal} · lines {chunk.line_start + 1}–{chunk.line_end + 1} · bytes {chunk.byte_start}–
        {chunk.byte_end}
        {chunk.sanitized && <span className="badge warn">control characters replaced</span>}
        {chunk.truncated && <span className="badge accent">cut</span>}
      </div>
      <div className="preview-lines">
        {lines.map((line, i) => (
          <div className="preview-line" key={i}>
            <span className="ln">{chunk.line_start + 1 + i}</span>
            <span className="lt">{line}</span>
          </div>
        ))}
      </div>
    </div>
  )
}
