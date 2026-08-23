// This file is generated from Rust by `cargo test -p eidos-service export_api_contract`.
// Do not edit it by hand. See docs/development.md for compatibility policy.

/** Exact decimal JSON string used for every Rust i64/u64. */
export type ApiInt = string;

export type ActivitySourceView = { source_id: SourceId, name: string, state: SourceState, content_enabled: boolean, content_concurrency: number, content_reserved: number, content_peak_reserved: number, jobs_queued: ApiInt, jobs_running: ApiInt, jobs_failed: ApiInt, jobs_failed_bytes: ApiInt, content_states: { [key in string]: ApiInt }, content_bytes_indexed: ApiInt, };

export type ActivityView = { content_enabled: boolean, jobs: JobCounts, content: ContentStats, archives: ArchiveStats, workers: ContentWorkersView, follower: FollowerView, content_index_documents: ApiInt, content_rebuild: RebuildStatus, admission: AdmissionView, sources: Array<ActivitySourceView>, recent_failures: Array<JobRecord>, };

export type AddSourceBody = { name: string, root_path: string, kind?: SourceKind | null, aliases: Array<string>, scan: boolean, };

export type AdmissionView = { limit: number, queue_depth: number, in_flight: ApiInt, queued: ApiInt, detached: ApiInt, admitted: ApiInt, completed: ApiInt, rejected_busy: ApiInt, timed_out: ApiInt, queue_wait_ms: ApiInt, search_timeout_ms: ApiInt, operation_timeout_ms: ApiInt, max_body_bytes: number, };

export type AggStats = { directories: ApiInt, extension_rows: ApiInt, unreachable_directories: ApiInt, };

export type ApiErrorBody = { error: string, kind: string, };

export type ArchiveStats = { archives: ApiInt, members: ApiInt, declared_size: ApiInt, truncated: ApiInt, failed: ApiInt, };

export type ArchiveSummary = { container_id: ObjectId, depth: number, member_path: string, compressed_size?: ApiInt, };

export type ChildRow = { entry: EntryRecord, object: ObjectRecord, aggregate: DirectoryAggregate | null, };

export type ChildSort = "name" | "size" | "allocated_size" | "modified" | "kind";

export type ChildrenQuery = { sort: ChildSort, desc: boolean, offset: ApiInt, limit: number, hidden: boolean, };

export type ChildrenView = { path: string | null, parent_id: ObjectId | null, source: SourceCompleteness, rows: Array<ChildRow>, total: ApiInt, offset: ApiInt, };

export type ContentId = string;

export type ContentPolicyBody = { enabled?: boolean | null, concurrency?: number | null, };

export type ContentPreview = { object_id: ObjectId, path: string | null, generation: number, object_generation: number, stale: boolean, state: ContentState, coverage: Coverage, indexed_bytes: ApiInt, total_bytes: ApiInt, chunk_count: number, line_count: ApiInt, encoding: string | null, reason: string | null, requested_ordinal: number, chunks: Array<PreviewChunk>, has_more_before: boolean, has_more_after: boolean, truncated: boolean, limits: PreviewLimits, };

export type ContentState = "pending" | "indexed" | "partial" | "failed" | "excluded" | "unsupported" | "stale" | "not_applicable";

export type ContentStats = { by_state: { [key in string]: [ApiInt, ApiInt, ApiInt] }, total_records: ApiInt, indexed_bytes: ApiInt, chunks: ApiInt, };

export type ContentSummary = { state: ContentState, coverage: Coverage, generation?: number, indexed_bytes?: ApiInt, content_id?: string, reason?: string, };

export type ContentWorkersView = { workers: number, current: Array<WorkerCurrent>, concurrency: Array<SourceConcurrencyView>, files_indexed: ApiInt, files_unsupported: ApiInt, files_failed: ApiInt, files_skipped: ApiInt, files_retried: ApiInt, bytes_read: ApiInt, chunks_written: ApiInt, commits: ApiInt, published: ApiInt, enqueued: ApiInt, pending_publish: ApiInt, uncommitted_documents: ApiInt, last_commit_ms: ApiInt, last_error: string | null, throughput_bytes_per_s: number, uptime_s: ApiInt, };

export type CountPolicy = "auto" | "exact" | "none";

export type Coverage = "full" | "prefix" | "tail" | "sample" | "none";

export type DirectoryAggregate = { object_id: ObjectId, file_count: ApiInt, dir_count: ApiInt, logical_bytes: ApiInt, allocated_bytes: ApiInt, newest_modified: UnixNanos | null, oldest_modified: UnixNanos | null, content_pending: ApiInt, content_indexed: ApiInt, content_failed: ApiInt, content_excluded: ApiInt, generation: ApiInt, complete: boolean, };

export type DirectorySummary = { file_count: ApiInt, directory_count: ApiInt, logical_bytes: ApiInt, allocated_bytes: ApiInt, newest_modified?: UnixNanos, oldest_modified?: UnixNanos, extension_counts?: { [key in string]: ApiInt }, complete: boolean, };

export type EntryId = ApiInt;

export type EntryRecord = { id: EntryId, source_id: SourceId, parent_id: ObjectId | null, object_id: ObjectId, name: string, extension: string, is_virtual: boolean, first_seen_generation: ApiInt, last_seen_generation: ApiInt, deleted_at: UnixNanos | null, };

export type ErrorsQuery = { include_resolved: boolean, limit: number, };

export type ExclusionRow = { stage: string, reason: string, count: ApiInt, bytes: ApiInt, };

export type Explanation = { readable: string, steps: Array<PlanStep>, };

export type Facet = { field: FacetField, values: Array<FacetValue>, truncated: boolean, };

export type FacetField = "source" | "extension" | "kind" | "content_state" | "top_directory" | "size_bucket" | "modified_bucket";

export type FacetRange = { from?: ApiInt, to?: ApiInt, clause: string, exclude: string, };

export type FacetRequest = { field: FacetField, limit: number, };

export type FacetValue = { value: string, count: ApiInt, label?: string, range?: FacetRange, };

export type FailureClass = "transient" | "unsupported" | "deterministic" | "resource_limit" | "corrupt";

export type FileAttributes = number;

export type FollowerView = { iterations: ApiInt, rebuilds: ApiInt, rows_applied: ApiInt, documents_added: ApiInt, last_seq: ApiInt, last_rebuild_ms: ApiInt, last_error: string | null, last_activity_ms_ago: ApiInt | null, rebuilding_source: ApiInt | null, index_documents: ApiInt, outbox_pending: ApiInt, };

export type Freshness = "live" | "periodic" | "unknown";

export type Health = { version: string, schema_version: number, host: string, uptime_s: ApiInt, catalog_path: string, sources: number, running_scans: number, export_max_rows: ApiInt, };

export type Hit = { object_id: ObjectId, entry_id?: EntryId, source_id: SourceId, host_id: HostId, kind: ObjectKind, name: string, path?: string, parent_id?: ObjectId, extension: string, size: ApiInt, allocated_size: ApiInt, modified?: UnixNanos, created?: UnixNanos, changed?: UnixNanos, attributes: FileAttributes, hard_link_count: number, content: ContentSummary, score?: number, snippets?: Array<Snippet>, directory?: DirectorySummary, archive?: ArchiveSummary, source_state: SourceState, };

export type HostId = ApiInt;

export type IdentityConfidence = "native" | "weak" | "path_derived";

export type IndexSourceState = { source_id: SourceId, name: string, published_generation: ApiInt | null, indexed_generation: ApiInt | null, documents: ApiInt, in_sync: boolean, };

export type IndexStatus = { follower: FollowerView, admission: AdmissionView, sources: Array<IndexSourceState>, };

export type JobCounts = { by_stage: { [key in string]: { [key in string]: ApiInt } }, queued: ApiInt, running: ApiInt, failed: ApiInt, oldest_queued_age_ms: ApiInt | null, };

export type JobId = ApiInt;

export type JobRecord = { id: JobId, source_id: SourceId, object_id: ObjectId | null, object_generation: number, stage: JobStage, priority: Priority, state: JobState, attempts: number, idempotency_key: string, payload: JsonValue | null, estimated_cost: ApiInt, created_at: UnixNanos, scheduled_at: UnixNanos, started_at: UnixNanos | null, finished_at: UnixNanos | null, worker: string | null, last_error: string | null, failure_class: FailureClass | null, requeue_count: number, requeued_at: UnixNanos | null, };

export type JobStage = "catalog_change" | "metadata_projection" | "content_text" | "archive_manifest" | "aggregate_rebuild" | "reconcile";

export type JobState = "queued" | "running" | "done" | "failed" | "superseded";

export type JsonValue = number | string | boolean | Array<JsonValue> | { [key in string]: JsonValue } | null;

export type LimitQuery = { limit: number, };

export type NativeIdentity = { volume_serial: ApiInt, file_id_high: ApiInt, file_id_low: ApiInt, confidence: IdentityConfidence, };

export type ObjectDetail = { object: ObjectRecord, path: string | null, entries: Array<EntryRecord>, aggregate: DirectoryAggregate | null, policy: Array<PolicyDecision>, source: SourceCompleteness, };

export type ObjectId = ApiInt;

export type ObjectKind = "file" | "directory" | "reparse" | "virtual_file" | "virtual_directory";

export type ObjectRecord = { id: ObjectId, source_id: SourceId, kind: ObjectKind, native: NativeIdentity | null, identity_confidence: IdentityConfidence, generation: number, size: ApiInt, allocated: ApiInt, attributes: FileAttributes, created: UnixNanos | null, modified: UnixNanos | null, changed: UnixNanos | null, accessed: UnixNanos | null, reparse_tag: number, link_count: number, content_state: ContentState, content_id: ContentId | null, first_seen_generation: ApiInt, last_seen_generation: ApiInt, deleted_at: UnixNanos | null, };

export type ParseQuery = { q: string, };

export type ParseView = { query: Query, rendered: string, notes: Array<string>, needs_content: boolean, };

export type PathMode = "exact" | "prefix" | "glob" | "regex";

export type PlanStep = { stage: string, description: string, candidates?: ApiInt, verified?: ApiInt, elapsed_ms?: number, };

export type PolicyDecision = { object_id: ObjectId, stage: string, included: boolean, reason: string, rule: string, policy_version: number, user_override: boolean, };

export type PreviewChunk = { ordinal: number, byte_start: ApiInt, byte_end: ApiInt, line_start: ApiInt, line_end: ApiInt, chars: number, text: string, truncated: boolean, sanitized: boolean, };

export type PreviewLimits = { max_neighbors: number, max_bytes: number, max_lines: number, };

export type PreviewQuery = { generation?: number | null, ordinal: number, before: number, after: number, };

export type Priority = "catalog_critical" | "metadata_projection" | "small_text" | "normal_text" | "large_text" | "archive_manifest" | "enrichment";

export type Query = { "op": "all" } | { "op": "and", clauses: Array<Query>, } | { "op": "or", clauses: Array<Query>, } | { "op": "not", clause: Query, } | { "op": "text", field: TextField, mode: TextMode, value: string, case_sensitive: boolean, slop: number, } | { "op": "host", ids: Array<HostId>, } | { "op": "source", ids: Array<SourceId>, names?: Array<string>, } | { "op": "object", ids: Array<ObjectId>, } | { "op": "path", mode: PathMode, value: string, case_sensitive: boolean, } | { "op": "descendant_of", directory: ObjectId, max_depth?: number | null, } | { "op": "extension", values: Array<string>, } | { "op": "kind", values: Array<ObjectKind>, } | { "op": "size", field: SizeField, min?: ApiInt | null, max?: ApiInt | null, } | { "op": "time", field: TimeField, after?: UnixNanos | null, before?: UnixNanos | null, } | { "op": "attributes", all_of: number, none_of: number, } | { "op": "content_state", states: Array<ContentState>, } | { "op": "descendant_extension", extension: string, min_count: ApiInt, max_count?: ApiInt | null, } | { "op": "subtree_size", field: SizeField, min?: ApiInt | null, max?: ApiInt | null, } | { "op": "descendant_count", min?: ApiInt | null, max?: ApiInt | null, } | { "op": "archive", in_archive?: boolean | null, container?: ObjectId | null, max_depth?: number | null, };

export type RebuildPhase = "idle" | "pending" | "running" | "failed";

export type RebuildStatus = { phase: RebuildPhase, chunks: ApiInt, docs: ApiInt, elapsed_ms: ApiInt, error?: string, };

export type ResolveQuery = { source: ApiInt, path: string, };

export type ResolveView = { object_id: ObjectId, path: string | null, };

export type ResultMode = "files" | "directories" | "both";

export type RetryBody = { class?: string | null, reason_prefix?: string | null, preview: boolean, limit?: number | null, as_of?: UnixNanos | null, confirmation?: string | null, };

export type RetryReport = { preview: boolean, as_of: UnixNanos, confirmation: string | null, accepted: ApiInt, skipped: ApiInt, rejected: ApiInt, bytes: ApiInt, skipped_reasons: { [key in string]: ApiInt }, rejected_reasons: { [key in string]: ApiInt }, job_ids: Array<JobId>, };

export type ScanGeneration = { source_id: SourceId, generation: ApiInt, kind: string, state: string, started_at: UnixNanos, finished_at: UnixNanos | null, published_at: UnixNanos | null, dirs_listed: ApiInt, entries_seen: ApiInt, errors: ApiInt, tombstoned: ApiInt, note: string | null, };

export type ScanProgress = { source_id: SourceId, running: boolean, phase: string, elapsed_ms: ApiInt, dirs: ApiInt, entries: ApiInt, errors: ApiInt, entries_per_sec: number, summary?: ScanSummary, error?: string, };

export type ScanStats = { dirs_listed: ApiInt, entries_seen: ApiInt, errors: ApiInt, objects_created: ApiInt, objects_updated: ApiInt, content_changed: ApiInt, entries_created: ApiInt, entries_replaced: ApiInt, hard_links: ApiInt, commits: ApiInt, };

export type ScanSummary = { source_id: SourceId, generation: ApiInt, stats: ScanStats, tombstoned_entries: ApiInt, tombstoned_objects: ApiInt, aggregates: AggStats, elapsed_ms: number, published: boolean, final_state: SourceState, };

export type SearchBody = { q?: string | null, query?: Query | null, mode: ResultMode, sort: Sort, limit: number, cursor?: string | null, explain: boolean, facets: Array<FacetRequest>, include_retired: boolean, count: CountPolicy, };

export type SearchView = { query: Query, rendered: string, notes?: Array<string>, schema_version: number, hits: Array<Hit>, next_cursor?: string, total: TotalCount, timing: Timing, completeness: Array<SourceCompleteness>, explanation?: Explanation, facets?: Array<Facet>, warnings?: Array<string>, };

export type SizeField = "logical" | "allocated";

export type Snippet = { chunk_ordinal: number, byte_start: ApiInt, byte_end: ApiInt, line_start: ApiInt, line_end: ApiInt, text: string, highlights: Array<[number, number]>, };

export type Sort = { field: SortField, descending: boolean, };

export type SortField = "relevance" | "name" | "path" | "size" | "allocated_size" | "subtree_size" | "modified" | "created";

export type SourceCompleteness = { source_id: SourceId, name: string, state: SourceState, metadata_complete: boolean, content_complete: boolean, content_pending: ApiInt, content_failed: ApiInt, listing_errors: ApiInt, last_scan_completed?: UnixNanos, checkpoint_age_ms?: ApiInt, freshness: Freshness, note?: string, };

export type SourceConcurrencyView = { source_id: SourceId, budget: number, reserved: number, peak_reserved: number, };

export type SourceCounts = { objects: ApiInt, entries: ApiInt, directories: ApiInt, files: ApiInt, logical_bytes: ApiInt, allocated_bytes: ApiInt, content_pending: ApiInt, content_indexed: ApiInt, content_failed: ApiInt, content_excluded: ApiInt, content_unsupported: ApiInt, open_errors: ApiInt, };

export type SourceDetail = { generations: Array<ScanGeneration>, root_aggregate: DirectoryAggregate | null, exclusions: Array<ExclusionRow>, source: SourceRecord, counts: SourceCounts, completeness: SourceCompleteness, scan?: ScanProgress, watcher?: WatcherView, };

export type SourceId = ApiInt;

export type SourceKind = "windows_local" | "windows_generic" | "smb";

export type SourceRecord = { id: SourceId, host_id: HostId, name: string, kind: SourceKind, root_path: string, aliases: Array<string>, state: SourceState, state_reason: string | null, policy_version: number, root_object_id: ObjectId | null, published_generation: ApiInt | null, volume_id: VolumeId | null, preserve_offline: boolean, reconcile_interval_s: ApiInt | null, content_enabled: boolean, content_concurrency: number, checkpoint_kind: string | null, checkpoint_at: UnixNanos | null, last_scan_started_at: UnixNanos | null, last_scan_completed_at: UnixNanos | null, created_at: UnixNanos, updated_at: UnixNanos, };

export type SourceState = "new" | "enumerating" | "metadata_complete" | "content_pending" | "complete" | "degraded" | "offline" | "stale" | "reconciling" | "retired";

export type SourceView = { source: SourceRecord, counts: SourceCounts, completeness: SourceCompleteness, scan?: ScanProgress, watcher?: WatcherView, };

export type TextField = "name" | "path" | "content";

export type TextMode = "ranked" | "exact" | "phrase" | "proximity" | "substring" | "regex";

export type TimeField = "modified" | "created" | "changed" | "accessed" | "subtree_modified";

export type Timing = { total_ms: number, plan_ms: number, retrieve_ms: number, verify_ms: number, join_ms: number, };

export type TotalCount = { value: ApiInt, exact: boolean, origin: TotalOrigin, };

export type TotalOrigin = "counted" | "cursor" | "bound";

export type UnixNanos = ApiInt;

export type VolumeId = ApiInt;

export type WatcherState = "starting" | "live" | "reconciling" | "stopped";

export type WatcherView = { state: WatcherState, live: boolean, detail: string | null, batches: ApiInt, events: ApiInt, records: ApiInt, reconciles: ApiInt, last_usn: ApiInt, last_batch_ms_ago: ApiInt | null, last_apply_ms: ApiInt, uptime_s: ApiInt, };

export type WorkerCurrent = { worker: string, source_id: SourceId, object_id: ObjectId, path: string, size: ApiInt, started_ms_ago: ApiInt, };
