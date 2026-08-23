// Typed client for the eidos HTTP API. Shapes mirror the Rust serde output.

import type { PreviewWindow } from './preview-window'

/** Decimal JSON string emitted for every Rust `i64`/`u64`. */
export type ApiInt = string
export type ApiRouteId = ApiInt | number

export type SourceState =
  | 'new'
  | 'enumerating'
  | 'metadata_complete'
  | 'content_pending'
  | 'complete'
  | 'degraded'
  | 'offline'
  | 'stale'
  | 'reconciling'
  | 'retired'

export type ContentState =
  | 'pending'
  | 'indexed'
  | 'partial'
  | 'failed'
  | 'excluded'
  | 'unsupported'
  | 'stale'
  | 'not_applicable'

export type ObjectKind = 'file' | 'directory' | 'reparse' | 'virtual_file' | 'virtual_directory'

export interface SourceRecord {
  content_enabled: boolean
  content_concurrency: number
  id: ApiInt
  host_id: ApiInt
  name: string
  kind: 'windows_local' | 'windows_generic' | 'smb'
  root_path: string
  aliases: string[]
  state: SourceState
  state_reason: string | null
  policy_version: number
  root_object_id: ApiInt | null
  published_generation: ApiInt | null
  volume_id: ApiInt | null
  preserve_offline: boolean
  reconcile_interval_s: ApiInt | null
  checkpoint_kind: string | null
  checkpoint_at: ApiInt | null
  last_scan_started_at: ApiInt | null
  last_scan_completed_at: ApiInt | null
  created_at: ApiInt
  updated_at: ApiInt
}

export interface SourceCounts {
  objects: ApiInt
  entries: ApiInt
  directories: ApiInt
  files: ApiInt
  logical_bytes: ApiInt
  allocated_bytes: ApiInt
  content_pending: ApiInt
  content_indexed: ApiInt
  content_failed: ApiInt
  content_excluded: ApiInt
  content_unsupported: ApiInt
  open_errors: ApiInt
}

export interface SourceCompleteness {
  source_id: ApiInt
  name: string
  state: SourceState
  metadata_complete: boolean
  content_complete: boolean
  content_pending: ApiInt
  content_failed: ApiInt
  listing_errors: ApiInt
  last_scan_completed?: ApiInt
  checkpoint_age_ms?: ApiInt
  freshness: 'live' | 'periodic' | 'unknown'
  note?: string
}

export interface ScanStats {
  dirs_listed: ApiInt
  entries_seen: ApiInt
  errors: ApiInt
  objects_created: ApiInt
  objects_updated: ApiInt
  content_changed: ApiInt
  entries_created: ApiInt
  entries_replaced: ApiInt
  hard_links: ApiInt
  commits: ApiInt
}

export interface ScanSummary {
  source_id: ApiInt
  generation: ApiInt
  stats: ScanStats
  tombstoned_entries: ApiInt
  tombstoned_objects: ApiInt
  aggregates: { directories: ApiInt; extension_rows: ApiInt; unreachable_directories: ApiInt }
  elapsed_ms: number
  published: boolean
  final_state: SourceState
}

export interface ScanProgress {
  source_id: ApiInt
  running: boolean
  phase: string
  elapsed_ms: ApiInt
  dirs: ApiInt
  entries: ApiInt
  errors: ApiInt
  entries_per_sec: number
  summary?: ScanSummary
  error?: string
}

export interface WatcherView {
  state: 'starting' | 'live' | 'reconciling' | 'stopped'
  live: boolean
  detail: string | null
  batches: ApiInt
  events: ApiInt
  records: ApiInt
  reconciles: ApiInt
  last_usn: ApiInt
  last_batch_ms_ago: ApiInt | null
  last_apply_ms: ApiInt
  uptime_s: ApiInt
}

export interface SourceView {
  source: SourceRecord
  counts: SourceCounts
  completeness: SourceCompleteness
  scan?: ScanProgress
  watcher?: WatcherView
}

export interface ScanGeneration {
  source_id: ApiInt
  generation: ApiInt
  kind: string
  state: string
  started_at: ApiInt
  finished_at: ApiInt | null
  published_at: ApiInt | null
  dirs_listed: ApiInt
  entries_seen: ApiInt
  errors: ApiInt
  tombstoned: ApiInt
  note: string | null
}

export interface DirectoryAggregate {
  object_id: ApiInt
  file_count: ApiInt
  dir_count: ApiInt
  logical_bytes: ApiInt
  allocated_bytes: ApiInt
  newest_modified: ApiInt | null
  oldest_modified: ApiInt | null
  content_pending: ApiInt
  content_indexed: ApiInt
  content_failed: ApiInt
  content_excluded: ApiInt
  generation: ApiInt
  complete: boolean
}

export interface ExclusionRow {
  stage: string
  reason: string
  count: ApiInt
  bytes: ApiInt
}

export interface SourceDetail extends SourceView {
  generations: ScanGeneration[]
  root_aggregate: DirectoryAggregate | null
  exclusions: ExclusionRow[]
}

export interface ObjectRecord {
  id: ApiInt
  source_id: ApiInt
  kind: ObjectKind
  native: {
    volume_serial: ApiInt
    file_id_high: ApiInt
    file_id_low: ApiInt
    confidence: string
  } | null
  identity_confidence: string
  generation: number
  size: ApiInt
  allocated: ApiInt
  attributes: number
  created: ApiInt | null
  modified: ApiInt | null
  changed: ApiInt | null
  accessed: ApiInt | null
  reparse_tag: number
  link_count: number
  content_state: ContentState
  content_id: string | null
  first_seen_generation: ApiInt
  last_seen_generation: ApiInt
  deleted_at: ApiInt | null
}

export interface EntryRecord {
  id: ApiInt
  source_id: ApiInt
  parent_id: ApiInt | null
  object_id: ApiInt
  name: string
  extension: string
  is_virtual: boolean
  first_seen_generation: ApiInt
  last_seen_generation: ApiInt
  deleted_at: ApiInt | null
}

export interface ChildRow {
  entry: EntryRecord
  object: ObjectRecord
  aggregate: DirectoryAggregate | null
}

export interface ChildrenView {
  rows: ChildRow[]
  total: ApiInt
  offset: ApiInt
  path: string | null
  parent_id: ApiInt | null
  source: SourceCompleteness
}

export interface PolicyDecision {
  object_id: ApiInt
  stage: string
  included: boolean
  reason: string
  rule: string
  policy_version: number
  user_override: boolean
}

export interface ObjectDetail {
  object: ObjectRecord
  path: string | null
  entries: EntryRecord[]
  aggregate: DirectoryAggregate | null
  policy: PolicyDecision[]
  source: SourceCompleteness
}

export interface ExtensionCount {
  extension: string
  count: ApiInt
  bytes: ApiInt
}

export interface ErrorRecord {
  id: ApiInt
  source_id: ApiInt
  object_id: ApiInt | null
  generation: ApiInt | null
  stage: string
  kind: string
  code: ApiInt
  path: string
  message: string
  occurred_at: ApiInt
  resolved_at: ApiInt | null
}

export interface Health {
  version: string
  schema_version: number
  host: string
  uptime_s: ApiInt
  catalog_path: string
  sources: number
  running_scans: number
  export_max_rows: ApiInt
}

// ----- search --------------------------------------------------------------

export type ResultMode = 'files' | 'directories' | 'both'
export type SortField =
  | 'relevance'
  | 'name'
  | 'path'
  | 'size'
  | 'allocated_size'
  | 'subtree_size'
  | 'modified'
  | 'created'
export type FacetField =
  | 'source'
  | 'extension'
  | 'kind'
  | 'content_state'
  | 'top_directory'
  | 'size_bucket'
  | 'modified_bucket'

export interface Snippet {
  chunk_ordinal: number
  byte_start: ApiInt
  byte_end: ApiInt
  line_start: ApiInt
  line_end: ApiInt
  text: string
  highlights: [number, number][]
}

export interface DirectorySummary {
  file_count: ApiInt
  directory_count: ApiInt
  logical_bytes: ApiInt
  allocated_bytes: ApiInt
  newest_modified?: ApiInt
  oldest_modified?: ApiInt
  extension_counts?: Record<string, ApiInt>
  complete: boolean
}

export interface Hit {
  object_id: ApiInt
  entry_id?: ApiInt
  source_id: ApiInt
  host_id: ApiInt
  kind: ObjectKind
  name: string
  path?: string
  parent_id?: ApiInt
  extension: string
  size: ApiInt
  allocated_size: ApiInt
  modified?: ApiInt
  created?: ApiInt
  changed?: ApiInt
  attributes: number
  hard_link_count: number
  content: {
    state: ContentState
    coverage: string
    generation?: number
    indexed_bytes?: ApiInt
    content_id?: string
    reason?: string
  }
  score?: number
  snippets?: Snippet[]
  directory?: DirectorySummary
  source_state: SourceState
}

/**
 * Exact boundaries of a size or modification-time bucket plus the query text
 * that selects or excludes it. The interval is half-open (`from` inclusive,
 * `to` exclusive) and either end may be open. Always append `clause` /
 * `exclude` verbatim: the bounds are informative only — times are Unix
 * nanoseconds, which exceed the precision of a JavaScript number.
 */
export interface FacetRange {
  from?: ApiInt
  to?: ApiInt
  clause: string
  exclude: string
}

export interface FacetValue {
  value: string
  count: ApiInt
  label?: string
  /** Present for range buckets the current result mode can express. */
  range?: FacetRange
}

export interface Facet {
  field: FacetField
  values: FacetValue[]
  truncated: boolean
}

export interface PlanStep {
  stage: string
  description: string
  candidates?: ApiInt
  verified?: ApiInt
  elapsed_ms?: number
}

export interface SearchResponse {
  schema_version: number
  hits: Hit[]
  next_cursor?: string
  total: { value: ApiInt; exact: boolean; origin?: 'counted' | 'cursor' | 'bound' }
  timing: { total_ms: number; plan_ms: number; retrieve_ms: number; verify_ms: number; join_ms: number }
  completeness: SourceCompleteness[]
  explanation?: { readable: string; steps: PlanStep[] }
  facets?: Facet[]
  warnings?: string[]
  query: unknown
  rendered: string
  notes?: string[]
}

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

export type ExportFormat = 'csv' | 'json' | 'ndjson'

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

export interface IndexStatus {
  follower: {
    iterations: ApiInt
    rebuilds: ApiInt
    rows_applied: ApiInt
    documents_added: ApiInt
    last_seq: ApiInt
    last_rebuild_ms: ApiInt
    last_error: string | null
    last_activity_ms_ago: ApiInt | null
    rebuilding_source: ApiInt | null
    index_documents: ApiInt
    outbox_pending: ApiInt
  }
  sources: {
    source_id: ApiInt
    name: string
    published_generation: ApiInt | null
    indexed_generation: ApiInt | null
    documents: ApiInt
    in_sync: boolean
  }[]
}

// ----- stored-text preview -------------------------------------------------

export interface PreviewChunk {
  ordinal: number
  byte_start: ApiInt
  byte_end: ApiInt
  line_start: ApiInt
  line_end: ApiInt
  chars: number
  text: string
  truncated: boolean
  sanitized: boolean
}

/**
 * A window of the text the indexer stored for one object generation. Never
 * the file on disk: the service reads it out of the catalog.
 */
export interface ContentPreview {
  object_id: ApiInt
  path: string | null
  generation: number
  object_generation: number
  stale: boolean
  state: ContentState
  coverage: string
  indexed_bytes: ApiInt
  total_bytes: ApiInt
  chunk_count: number
  line_count: ApiInt
  encoding: string | null
  reason: string | null
  requested_ordinal: number
  chunks: PreviewChunk[]
  has_more_before: boolean
  has_more_after: boolean
  truncated: boolean
  limits: { max_neighbors: number; max_bytes: number; max_lines: number }
}

export type { PreviewWindow }

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
      const body = await res.json()
      if (body && typeof body.error === 'string') {
        message = body.error
        kind = body.kind ?? kind
      }
    } catch {
      // ignore
    }
    throw new ApiError(res.status, kind, message)
  }
  return (await res.json()) as T
}

export type ChildSort = 'name' | 'size' | 'allocated_size' | 'modified' | 'kind'

export const api = {
  health: () => request<Health>('/api/health'),
  sources: () => request<SourceView[]>('/api/sources'),
  source: (id: ApiRouteId) => request<SourceDetail>(`/api/sources/${id}`),
  addSource: (body: { name: string; root_path: string; scan: boolean }) =>
    request<SourceView>('/api/sources', { method: 'POST', body: JSON.stringify(body) }),
  scanSource: (id: ApiRouteId) => request<ScanProgress>(`/api/sources/${id}/scan`, { method: 'POST' }),
  cancelScan: (id: ApiRouteId) => request<ScanProgress>(`/api/sources/${id}/scan/cancel`, { method: 'POST' }),
  sourceErrors: (id: ApiRouteId, includeResolved = false) =>
    request<ErrorRecord[]>(`/api/sources/${id}/errors?include_resolved=${includeResolved}&limit=500`),
  object: (id: ApiRouteId) => request<ObjectDetail>(`/api/objects/${id}`),
  children: (id: ApiRouteId, q: { sort: ChildSort; desc: boolean; offset: ApiInt | number; limit: number; hidden: boolean }) =>
    request<ChildrenView>(
      `/api/objects/${id}/children?sort=${q.sort}&desc=${q.desc}&offset=${q.offset}&limit=${q.limit}&hidden=${q.hidden}`,
    ),
  extensions: (id: ApiRouteId, limit = 30) => request<ExtensionCount[]>(`/api/objects/${id}/extensions?limit=${limit}`),
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
    request<{ object_id: ApiInt; path: string | null }>(
      `/api/resolve?source=${source}&path=${encodeURIComponent(path)}`,
    ),
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
  parse: (q: string) =>
    request<{ query: unknown; rendered: string; notes: string[]; needs_content: boolean }>(
      `/api/search/parse?q=${encodeURIComponent(q)}`,
    ),
  indexStatus: () => request<IndexStatus>('/api/index'),
  activity: () => request<ActivityView>('/api/activity'),
  setContentPolicy: (id: ApiRouteId, body: { enabled?: boolean; concurrency?: number }) =>
    request<SourceView>(`/api/sources/${id}/content`, { method: 'POST', body: JSON.stringify(body) }),
  retryJob: (id: ApiRouteId) =>
    request<RetryReport>(`/api/jobs/${id}/retry`, { method: 'POST', body: JSON.stringify({ preview: false }) }),
  retrySourceContent: (
    id: ApiRouteId,
    body: {
      class?: string
      reason_prefix?: string
      preview: boolean
      limit?: number
      as_of?: ApiInt
      confirmation?: string
    },
  ) =>
    request<RetryReport>(`/api/sources/${id}/content/retry`, { method: 'POST', body: JSON.stringify(body) }),
}

// Result of a retry action (`preview: true` counts without acting).
export interface RetryReport {
  preview: boolean
  as_of: ApiInt
  confirmation: string | null
  accepted: ApiInt
  skipped: ApiInt
  rejected: ApiInt
  bytes: ApiInt
  skipped_reasons: Record<string, ApiInt>
  rejected_reasons: Record<string, ApiInt>
  job_ids: ApiInt[]
}

export interface WorkerCurrent {
  worker: string
  source_id: ApiInt
  object_id: ApiInt
  path: string
  size: ApiInt
  started_ms_ago: ApiInt
}

export interface ContentWorkersView {
  workers: number
  current: WorkerCurrent[]
  files_indexed: ApiInt
  files_unsupported: ApiInt
  files_failed: ApiInt
  files_skipped: ApiInt
  files_retried: ApiInt
  bytes_read: ApiInt
  chunks_written: ApiInt
  commits: ApiInt
  published: ApiInt
  enqueued: ApiInt
  pending_publish: ApiInt
  uncommitted_documents: ApiInt
  last_commit_ms: ApiInt
  last_error?: string | null
  throughput_bytes_per_s: number
  uptime_s: ApiInt
}

export interface JobCounts {
  by_stage: Record<string, Record<string, ApiInt>>
  queued: ApiInt
  running: ApiInt
  failed: ApiInt
  oldest_queued_age_ms?: ApiInt | null
}

export interface ContentStats {
  by_state: Record<string, [ApiInt, ApiInt, ApiInt]>
  total_records: ApiInt
  indexed_bytes: ApiInt
  chunks: ApiInt
}

export interface ActivitySourceView {
  source_id: ApiInt
  name: string
  state: SourceState
  content_enabled: boolean
  content_concurrency: number
  content_reserved: number
  content_peak_reserved: number
  jobs_queued: ApiInt
  jobs_running: ApiInt
  jobs_failed: ApiInt
  jobs_failed_bytes: ApiInt
  content_states: Record<string, ApiInt>
  content_bytes_indexed: ApiInt
}

export interface JobRecord {
  id: ApiInt
  source_id: ApiInt
  object_id?: ApiInt | null
  object_generation: number
  stage: string
  priority: number
  state: string
  attempts: number
  last_error?: string | null
  failure_class?: string | null
  finished_at?: ApiInt | null
  requeue_count: number
  requeued_at?: ApiInt | null
}

export interface ActivityView {
  content_enabled: boolean
  jobs: JobCounts
  content: ContentStats
  workers: ContentWorkersView
  follower: IndexStatus['follower']
  content_index_documents: ApiInt
  sources: ActivitySourceView[]
  recent_failures: JobRecord[]
}
