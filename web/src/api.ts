// Typed client for the eidos HTTP API. Shapes mirror the Rust serde output.

import type { PreviewWindow } from './preview-window'

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
  id: number
  host_id: number
  name: string
  kind: 'windows_local' | 'windows_generic' | 'smb'
  root_path: string
  aliases: string[]
  state: SourceState
  state_reason: string | null
  policy_version: number
  root_object_id: number | null
  published_generation: number | null
  volume_id: number | null
  preserve_offline: boolean
  checkpoint_kind: string | null
  checkpoint_at: number | null
  last_scan_started_at: number | null
  last_scan_completed_at: number | null
  created_at: number
  updated_at: number
}

export interface SourceCounts {
  objects: number
  entries: number
  directories: number
  files: number
  logical_bytes: number
  allocated_bytes: number
  content_pending: number
  content_indexed: number
  content_failed: number
  content_excluded: number
  content_unsupported: number
  open_errors: number
}

export interface SourceCompleteness {
  source_id: number
  name: string
  state: SourceState
  metadata_complete: boolean
  content_complete: boolean
  content_pending: number
  content_failed: number
  listing_errors: number
  last_scan_completed?: number
  checkpoint_age_ms?: number
  freshness: 'live' | 'periodic' | 'unknown'
  note?: string
}

export interface ScanStats {
  dirs_listed: number
  entries_seen: number
  errors: number
  objects_created: number
  objects_updated: number
  content_changed: number
  entries_created: number
  entries_replaced: number
  hard_links: number
  commits: number
}

export interface ScanSummary {
  source_id: number
  generation: number
  stats: ScanStats
  tombstoned_entries: number
  tombstoned_objects: number
  aggregates: { directories: number; extension_rows: number; unreachable_directories: number }
  elapsed_ms: number
  published: boolean
  final_state: SourceState
}

export interface ScanProgress {
  source_id: number
  running: boolean
  elapsed_ms: number
  dirs: number
  entries: number
  errors: number
  entries_per_sec: number
  summary?: ScanSummary
  error?: string
}

export interface WatcherView {
  state: 'starting' | 'live' | 'reconciling' | 'stopped'
  live: boolean
  detail: string | null
  batches: number
  events: number
  records: number
  reconciles: number
  last_usn: number
  last_batch_ms_ago: number | null
  last_apply_ms: number
  uptime_s: number
}

export interface SourceView {
  source: SourceRecord
  counts: SourceCounts
  completeness: SourceCompleteness
  scan?: ScanProgress
  watcher?: WatcherView
}

export interface ScanGeneration {
  source_id: number
  generation: number
  kind: string
  state: string
  started_at: number
  finished_at: number | null
  published_at: number | null
  dirs_listed: number
  entries_seen: number
  errors: number
  tombstoned: number
  note: string | null
}

export interface DirectoryAggregate {
  object_id: number
  file_count: number
  dir_count: number
  logical_bytes: number
  allocated_bytes: number
  newest_modified: number | null
  oldest_modified: number | null
  content_pending: number
  content_indexed: number
  content_failed: number
  content_excluded: number
  generation: number
  complete: boolean
}

export interface ExclusionRow {
  stage: string
  reason: string
  count: number
  bytes: number
}

export interface SourceDetail extends SourceView {
  generations: ScanGeneration[]
  root_aggregate: DirectoryAggregate | null
  exclusions: ExclusionRow[]
}

export interface ObjectRecord {
  id: number
  source_id: number
  kind: ObjectKind
  native: {
    volume_serial: number
    file_id_high: number
    file_id_low: number
    confidence: string
  } | null
  identity_confidence: string
  generation: number
  size: number
  allocated: number
  attributes: number
  created: number | null
  modified: number | null
  changed: number | null
  accessed: number | null
  reparse_tag: number
  link_count: number
  content_state: ContentState
  content_id: string | null
  first_seen_generation: number
  last_seen_generation: number
  deleted_at: number | null
}

export interface EntryRecord {
  id: number
  source_id: number
  parent_id: number | null
  object_id: number
  name: string
  extension: string
  is_virtual: boolean
  first_seen_generation: number
  last_seen_generation: number
  deleted_at: number | null
}

export interface ChildRow {
  entry: EntryRecord
  object: ObjectRecord
  aggregate: DirectoryAggregate | null
}

export interface ChildrenView {
  rows: ChildRow[]
  total: number
  offset: number
  path: string | null
  parent_id: number | null
  source: SourceCompleteness
}

export interface PolicyDecision {
  object_id: number
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
  count: number
  bytes: number
}

export interface ErrorRecord {
  id: number
  source_id: number
  object_id: number | null
  generation: number | null
  stage: string
  kind: string
  code: number
  path: string
  message: string
  occurred_at: number
  resolved_at: number | null
}

export interface Health {
  version: string
  schema_version: number
  host: string
  uptime_s: number
  catalog_path: string
  sources: number
  running_scans: number
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
  byte_start: number
  byte_end: number
  line_start: number
  line_end: number
  text: string
  highlights: [number, number][]
}

export interface DirectorySummary {
  file_count: number
  directory_count: number
  logical_bytes: number
  allocated_bytes: number
  newest_modified?: number
  oldest_modified?: number
  extension_counts?: Record<string, number>
  complete: boolean
}

export interface Hit {
  object_id: number
  entry_id?: number
  source_id: number
  host_id: number
  kind: ObjectKind
  name: string
  path?: string
  parent_id?: number
  extension: string
  size: number
  allocated_size: number
  modified?: number
  created?: number
  changed?: number
  attributes: number
  hard_link_count: number
  content: {
    state: ContentState
    coverage: string
    generation?: number
    indexed_bytes?: number
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
  from?: number
  to?: number
  clause: string
  exclude: string
}

export interface FacetValue {
  value: string
  count: number
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
  candidates?: number
  verified?: number
  elapsed_ms?: number
}

export interface SearchResponse {
  schema_version: number
  hits: Hit[]
  next_cursor?: string
  total: { value: number; exact: boolean }
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

export interface IndexStatus {
  follower: {
    iterations: number
    rebuilds: number
    rows_applied: number
    documents_added: number
    last_seq: number
    last_rebuild_ms: number
    last_error: string | null
    last_activity_ms_ago: number | null
    rebuilding_source: number | null
    index_documents: number
    outbox_pending: number
  }
  sources: {
    source_id: number
    name: string
    published_generation: number | null
    indexed_generation: number | null
    documents: number
    in_sync: boolean
  }[]
}

// ----- stored-text preview -------------------------------------------------

export interface PreviewChunk {
  ordinal: number
  byte_start: number
  byte_end: number
  line_start: number
  line_end: number
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
  object_id: number
  path: string | null
  generation: number
  object_generation: number
  stale: boolean
  state: ContentState
  coverage: string
  indexed_bytes: number
  total_bytes: number
  chunk_count: number
  line_count: number
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
  source: (id: number) => request<SourceDetail>(`/api/sources/${id}`),
  addSource: (body: { name: string; root_path: string; scan: boolean }) =>
    request<SourceView>('/api/sources', { method: 'POST', body: JSON.stringify(body) }),
  scanSource: (id: number) => request<ScanProgress>(`/api/sources/${id}/scan`, { method: 'POST' }),
  cancelScan: (id: number) => request<ScanProgress>(`/api/sources/${id}/scan/cancel`, { method: 'POST' }),
  sourceErrors: (id: number, includeResolved = false) =>
    request<ErrorRecord[]>(`/api/sources/${id}/errors?include_resolved=${includeResolved}&limit=500`),
  object: (id: number) => request<ObjectDetail>(`/api/objects/${id}`),
  children: (id: number, q: { sort: ChildSort; desc: boolean; offset: number; limit: number; hidden: boolean }) =>
    request<ChildrenView>(
      `/api/objects/${id}/children?sort=${q.sort}&desc=${q.desc}&offset=${q.offset}&limit=${q.limit}&hidden=${q.hidden}`,
    ),
  extensions: (id: number, limit = 30) => request<ExtensionCount[]>(`/api/objects/${id}/extensions?limit=${limit}`),
  contentPreview: (id: number, w: PreviewWindow) => {
    const p = new URLSearchParams({
      ordinal: String(w.ordinal),
      before: String(w.before),
      after: String(w.after),
    })
    if (w.generation != null) p.set('generation', String(w.generation))
    return request<ContentPreview>(`/api/objects/${id}/content?${p.toString()}`)
  },
  resolve: (source: number, path: string) =>
    request<{ object_id: number; path: string | null }>(
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
  setContentPolicy: (id: number, body: { enabled?: boolean; concurrency?: number }) =>
    request<SourceView>(`/api/sources/${id}/content`, { method: 'POST', body: JSON.stringify(body) }),
}

export interface WorkerCurrent {
  worker: string
  source_id: number
  object_id: number
  path: string
  size: number
  started_ms_ago: number
}

export interface ContentWorkersView {
  workers: number
  current: WorkerCurrent[]
  files_indexed: number
  files_unsupported: number
  files_failed: number
  files_skipped: number
  files_retried: number
  bytes_read: number
  chunks_written: number
  commits: number
  published: number
  enqueued: number
  pending_publish: number
  uncommitted_documents: number
  last_commit_ms: number
  last_error?: string | null
  throughput_bytes_per_s: number
  uptime_s: number
}

export interface JobCounts {
  by_stage: Record<string, Record<string, number>>
  queued: number
  running: number
  failed: number
  oldest_queued_age_ms?: number | null
}

export interface ContentStats {
  by_state: Record<string, [number, number, number]>
  total_records: number
  indexed_bytes: number
  chunks: number
}

export interface ActivitySourceView {
  source_id: number
  name: string
  state: SourceState
  content_enabled: boolean
  content_concurrency: number
  content_reserved: number
  content_peak_reserved: number
  jobs_queued: number
  jobs_running: number
  content_states: Record<string, number>
  content_bytes_indexed: number
}

export interface JobRecord {
  id: number
  source_id: number
  object_id?: number | null
  object_generation: number
  stage: string
  priority: number
  state: string
  attempts: number
  last_error?: string | null
  failure_class?: string | null
  finished_at?: number | null
}

export interface ActivityView {
  content_enabled: boolean
  jobs: JobCounts
  content: ContentStats
  workers: ContentWorkersView
  follower: IndexStatus['follower']
  content_index_documents: number
  sources: ActivitySourceView[]
  recent_failures: JobRecord[]
}
