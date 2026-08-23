// Typed client for the eidos HTTP API. Rust-owned wire shapes are generated
// from the serde contract; only client conveniences live in this module.

import type {
  ActivityView,
  AddSourceBody,
  ApiErrorBody,
  ApiInt,
  ChildrenQuery,
  ChildrenView,
  ContentPolicyBody,
  ContentPreview,
  ErrorRecord,
  ExportFormat,
  ExtensionCount,
  FacetField,
  Health,
  IndexStatus,
  ObjectDetail,
  ParseView,
  ResolveView,
  ResultMode,
  RetryBody,
  RetryReport,
  ScanProgress,
  SearchView,
  SortField,
  SourceDetail,
  SourceView,
} from './generated/api'
import type { PreviewWindow } from './preview-window'

export type * from './generated/api'
export type { PreviewWindow }

/** IDs accepted in URL paths before callers have normalized user input. */
export type ApiRouteId = ApiInt | number

/** Stable client name for the flattened POST/GET search response. */
export type SearchResponse = SearchView

/** Durable UI search state; the request mapper below supplies wire defaults. */
export interface SearchParams {
  q: string
  mode: ResultMode
  sort: SortField
  desc: boolean
  limit: number
  cursor?: string
  facets: FacetField[]
  explain: boolean
}

type AddSourceParams = Pick<AddSourceBody, 'name' | 'root_path' | 'scan'> &
  Partial<Pick<AddSourceBody, 'kind' | 'aliases'>>
type ChildrenParams = Omit<ChildrenQuery, 'offset'> & { offset: ApiInt | number }

/** URL of the streaming export of a query's whole result set. */
export function exportUrl(
  p: { q: string; mode: ResultMode; sort: SortField; desc: boolean },
  format: ExportFormat,
  bom = false,
): string {
  const qs = new URLSearchParams({ format, q: p.q, mode: p.mode, sort: p.sort, desc: String(p.desc) })
  if (bom) qs.set('bom', '1')
  return `/api/search/export?${qs.toString()}`
}

export class ApiError extends Error {
  status: number
  kind: string
  constructor(status: number, kind: string, message: string) {
    super(message)
    this.status = status
    this.kind = kind
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    ...init,
    headers: { 'content-type': 'application/json', ...(init?.headers ?? {}) },
  })
  if (!res.ok) {
    let kind = 'http'
    let message = `${res.status} ${res.statusText}`
    try {
      const body = (await res.json()) as Partial<ApiErrorBody>
      if (typeof body.error === 'string') {
        message = body.error
        kind = body.kind ?? kind
      }
    } catch {
      // Keep the HTTP fallback when the response is not a JSON API error.
    }
    throw new ApiError(res.status, kind, message)
  }
  return (await res.json()) as T
}

export const api = {
  health: () => request<Health>('/api/health'),
  sources: () => request<SourceView[]>('/api/sources'),
  source: (id: ApiRouteId) => request<SourceDetail>(`/api/sources/${id}`),
  addSource: (body: AddSourceParams) =>
    request<SourceView>('/api/sources', { method: 'POST', body: JSON.stringify(body) }),
  scanSource: (id: ApiRouteId) => request<ScanProgress>(`/api/sources/${id}/scan`, { method: 'POST' }),
  cancelScan: (id: ApiRouteId) =>
    request<ScanProgress>(`/api/sources/${id}/scan/cancel`, { method: 'POST' }),
  sourceErrors: (id: ApiRouteId, includeResolved = false) =>
    request<ErrorRecord[]>(`/api/sources/${id}/errors?include_resolved=${includeResolved}&limit=500`),
  object: (id: ApiRouteId) => request<ObjectDetail>(`/api/objects/${id}`),
  children: (id: ApiRouteId, q: ChildrenParams) =>
    request<ChildrenView>(
      `/api/objects/${id}/children?sort=${q.sort}&desc=${q.desc}&offset=${q.offset}&limit=${q.limit}&hidden=${q.hidden}`,
    ),
  extensions: (id: ApiRouteId, limit = 30) =>
    request<ExtensionCount[]>(`/api/objects/${id}/extensions?limit=${limit}`),
  contentPreview: (id: ApiRouteId, w: PreviewWindow) => {
    const p = new URLSearchParams({
      ordinal: String(w.ordinal),
      before: String(w.before),
      after: String(w.after),
    })
    if (w.generation != null) p.set('generation', String(w.generation))
    return request<ContentPreview>(`/api/objects/${id}/content?${p.toString()}`)
  },
  resolve: (source: ApiRouteId, path: string) =>
    request<ResolveView>(`/api/resolve?source=${source}&path=${encodeURIComponent(path)}`),
  search: (p: SearchParams) =>
    request<SearchResponse>('/api/search', {
      method: 'POST',
      body: JSON.stringify({
        q: p.q,
        mode: p.mode,
        sort: { field: p.sort, descending: p.desc },
        limit: p.limit,
        cursor: p.cursor,
        explain: p.explain,
        facets: p.facets.map((field) => ({ field, limit: 20 })),
      }),
    }),
  parse: (q: string) => request<ParseView>(`/api/search/parse?q=${encodeURIComponent(q)}`),
  indexStatus: () => request<IndexStatus>('/api/index'),
  activity: () => request<ActivityView>('/api/activity'),
  setContentPolicy: (id: ApiRouteId, body: ContentPolicyBody) =>
    request<SourceView>(`/api/sources/${id}/content`, {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  retryJob: (id: ApiRouteId) =>
    request<RetryReport>(`/api/jobs/${id}/retry`, {
      method: 'POST',
      body: JSON.stringify({ preview: false }),
    }),
  retrySourceContent: (id: ApiRouteId, body: RetryBody) =>
    request<RetryReport>(`/api/sources/${id}/content/retry`, {
      method: 'POST',
      body: JSON.stringify(body),
    }),
}
