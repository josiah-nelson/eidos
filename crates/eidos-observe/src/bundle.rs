use crate::schema::{ObservationBundle, SCHEMA_VERSION};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

pub fn write_bundle(file: &Path, bundle: &ObservationBundle) -> Result<(), BundleError> {
    if bundle.manifest.schema != SCHEMA_VERSION {
        return Err(BundleError::Schema(bundle.manifest.schema.clone()));
    }
    let parent = file.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    {
        let output = BufWriter::new(temporary.as_file_mut());
        let mut encoder = zstd::stream::write::Encoder::new(output, 9)?;
        serde_json::to_writer(&mut encoder, bundle)?;
        encoder.finish()?.flush()?;
    }
    temporary.as_file().sync_all()?;
    temporary.persist(file).map_err(|error| error.error)?;
    Ok(())
}

pub fn read_bundle(file: &Path) -> Result<ObservationBundle, BundleError> {
    let input = BufReader::new(File::open(file)?);
    let decoder = zstd::stream::read::Decoder::new(input)?;
    let bundle: ObservationBundle = serde_json::from_reader(decoder)?;
    if bundle.manifest.schema != SCHEMA_VERSION {
        return Err(BundleError::Schema(bundle.manifest.schema));
    }
    Ok(bundle)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleInspection {
    pub schema: String,
    /// Sorted paths for every field actually present in the export.
    pub fields: Vec<String>,
    pub records: usize,
}

pub fn inspect_bundle(file: &Path) -> Result<BundleInspection, BundleError> {
    let bundle = read_bundle(file)?;
    let value = serde_json::to_value(&bundle)?;
    let mut fields = BTreeSet::new();
    collect_fields("", &value, &mut fields);
    Ok(BundleInspection {
        schema: bundle.manifest.schema,
        fields: fields.into_iter().collect(),
        records: bundle.records.len(),
    })
}

fn collect_fields(prefix: &str, value: &Value, fields: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let next = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                fields.insert(next.clone());
                collect_fields(&next, child, fields);
            }
        }
        Value::Array(values) => {
            let next = format!("{prefix}[]");
            for child in values {
                collect_fields(&next, child, fields);
            }
        }
        _ => {}
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("unsupported observation schema: {0}")]
    Schema(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::families::*;
    use crate::privacy::StudyKey;
    use crate::schema::*;

    fn bundle() -> ObservationBundle {
        let key = StudyKey::from_bytes([3; 32]);
        ObservationBundle {
            manifest: BundleManifest {
                schema: SCHEMA_VERSION.into(),
                build_hash: "build-token".into(),
                config_hash: "config-token".into(),
                created: TimeAnchor {
                    monotonic_ns: 10,
                    utc_ns: 20,
                },
                capabilities: Capabilities {
                    fsevents: true,
                    endpoint_security: EndpointSecurityCapability {
                        state: EndpointSecurityState::Off,
                        entitlement_claimed: false,
                        tcc_full_disk_access: None,
                        running_as_root: false,
                    },
                    apfs: true,
                    windows: Some(WindowsCapabilities {
                        usn: UsnState::Available,
                        etw: EtwState::Off,
                        running_as_system: true,
                        elevated: true,
                        study_key_available: true,
                    }),
                },
                capture_gaps: Vec::new(),
                drops: DropCounters::default(),
                units: Units::default(),
            },
            records: every_record_kind(&key),
        }
    }

    /// One record of every variant, so the inspector and the raw-field
    /// check cover the whole durable schema. Adding a variant without
    /// adding it here fails `every_variant_is_covered`.
    fn every_record_kind(key: &StudyKey) -> Vec<ObservationRecord> {
        let at = TimeAnchor {
            monotonic_ns: 10,
            utc_ns: 20,
        };
        let volume = key.token("volume", b"volume-a");
        let object = key.token("object", b"object-a");
        let mut histogram = Histogram::new();
        histogram.observe(3);
        vec![
            ObservationRecord::Health(HealthRecord {
                at: at.clone(),
                os_build: "0.0".into(),
                machine: MachineKind::Unknown,
                lifecycle: LifecycleEvent::Started,
                clean_prior_shutdown: None,
                feed_cursor: None,
                drops: DropCounters::default(),
                cpu_millis: 0,
                resident_bytes_bucket: SizeBucket::B1M,
            }),
            ObservationRecord::LogicalChange(LogicalChange {
                at: at.clone(),
                object: object.clone(),
                subtree: key.token("subtree", b"dir-a"),
                operation: ChangeOperation::Update,
                rename_pair: None,
                size: SizeBucket::B1K,
                extension: ExtensionBucket::Source,
                depth: DepthBucket::Shallow,
                edit_count: CountBucket::One,
                delete_recreate_age: None,
                fan_out: CountBucket::One,
                backlog_age: AgeBucket::Immediate,
            }),
            ObservationRecord::Workload(WorkloadSummary {
                at: at.clone(),
                process: ProcessClass::Other,
                opens: CountBucket::Zero,
                closes: CountBucket::Zero,
                mappings: CountBucket::Zero,
                executions: CountBucket::Zero,
                changed_objects: CountBucket::Zero,
            }),
            ObservationRecord::Apfs(ApfsObservation {
                at: at.clone(),
                volume: volume.clone(),
                object: object.clone(),
                kind: ApfsKind::Sparse,
                prevalence: CountBucket::One,
                size: SizeBucket::B4K,
            }),
            ObservationRecord::Mark(MarkRecord {
                at: at.clone(),
                marker: key.token("mark", b"phase-a"),
            }),
            ObservationRecord::Volume(VolumeObservation {
                at: at.clone(),
                volume: volume.clone(),
                event: VolumeEvent::Inventory,
                filesystem: FilesystemKind::Ntfs,
                drive: DriveKind::Fixed,
                bus: BusKind::Nvme,
                media: MediaKind::Solid,
                capacity: CapacityBucket::T1,
                free: PercentBucket::Under50,
                bytes_per_cluster: 4096,
                case_sensitive: Some(false),
                supports_usn: true,
                supports_file_ids: true,
                supports_sparse: true,
                supports_reparse_points: true,
                supports_hard_links: true,
                compressed: false,
                journal: Some(JournalShape {
                    maximum_size: SizeBucket::B64M,
                    allocation_delta: SizeBucket::B16M,
                    span: SizeBucket::B16M,
                    max_major_version: 3,
                }),
            }),
            ObservationRecord::FeedHealth(FeedHealthRecord {
                at: at.clone(),
                volume: volume.clone(),
                feed: FeedKind::Usn,
                state: FeedState::Live,
                cursor: Some(FeedCursor {
                    feed: FeedKind::Usn,
                    version: 1,
                    opaque: "opaque".into(),
                }),
                lag: SizeBucket::Zero,
                fill: PercentBucket::Under10,
                batches: 1,
                records: 2,
                logical_changes: 1,
                coalesced: 1,
                overflows: 0,
                recreations: 0,
                read_errors: 0,
                backlog_ms: histogram.clone(),
            }),
            ObservationRecord::Rate(RateSummary {
                at: at.clone(),
                volume: volume.clone(),
                interval_s: 60,
                records: 2,
                logical_changes: 1,
                per_second: histogram.clone(),
                operations: OperationCounts::default(),
                coalesced: CoalescingWindows::default(),
                tombstones: 0,
                hot_objects: 0,
                directories_touched: 0,
                recreates: 0,
                extensions: vec![(ExtensionBucket::Source, 1)],
                sizes: vec![(SizeBucket::B1K, 1)],
                depths: vec![(DepthBucket::Shallow, 1)],
                max_backlog: AgeBucket::Immediate,
            }),
            ObservationRecord::Reasons(ReasonSummary {
                at: at.clone(),
                volume: volume.clone(),
                interval_s: 60,
                combinations: vec![(0x8000_0002, 1)],
                close_records: 1,
                intermediate_records: 1,
                directory_records: 0,
            }),
            ObservationRecord::Access(AccessSummary {
                at: at.clone(),
                interval_s: 60,
                process: ProcessClass::Build,
                process_starts: 1,
                opens: 1,
                reads: 1,
                writes: 1,
                closes: 1,
                deletes: 0,
                renames: 0,
                read_bytes: 10,
                write_bytes: 10,
                distinct_objects: 1,
                read_write_objects: 1,
                read_size: histogram.clone(),
                write_size: histogram.clone(),
                extensions: vec![(ExtensionBucket::Build, 1)],
            }),
            ObservationRecord::Content(ContentObservation {
                at: at.clone(),
                volume: volume.clone(),
                object: object.clone(),
                size: SizeBucket::B64K,
                extension: ExtensionBucket::Source,
                outcome: ContentOutcome::Measured,
                fingerprint: Some(key.token("content", b"bytes")),
                chunker: ChunkerKind::FastCdc {
                    min: 4096,
                    average: 16384,
                    max: 65536,
                },
                chunks: 3,
                chunk_size: histogram.clone(),
                reused_chunks: 1,
                reuse_runs: histogram.clone(),
                compressed: PercentBucket::Under50,
                read_ms: 1,
                text_like: Some(true),
            }),
            ObservationRecord::Enumeration(EnumerationProbe {
                at: at.clone(),
                volume: volume.clone(),
                duration_ms: 1,
                cpu_ms: 1,
                files: 1,
                directories: 1,
                errors: 0,
                max_depth: DepthBucket::Medium,
                fan_out: histogram.clone(),
                sizes: vec![(SizeBucket::B1K, 1)],
                extensions: vec![(ExtensionBucket::Other, 1)],
                reparse_points: 0,
                placeholders: 0,
                sparse: 0,
                compressed: 0,
                encrypted: 0,
                offline: 0,
                hard_linked: 0,
                under_allocated: 0,
            }),
            ObservationRecord::Resource(ResourceSample {
                at,
                interval_s: 60,
                collector: ProcessResources::default(),
                system: HostResources {
                    cpu_busy_percent: 5,
                    memory_used_percent: 50,
                    memory_total: CapacityBucket::G64,
                    logical_processors: 8,
                    uptime: AgeBucket::Days,
                    on_battery: None,
                    slept_ms: 0,
                },
                lanes: LaneStates::default(),
            }),
        ]
    }

    #[test]
    fn every_variant_is_covered() {
        let key = StudyKey::from_bytes([3; 32]);
        let kinds: BTreeSet<String> = every_record_kind(&key)
            .iter()
            .map(|record| {
                serde_json::to_value(record).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        let expected = [
            "health",
            "logical_change",
            "workload",
            "apfs",
            "mark",
            "volume",
            "feed_health",
            "rate",
            "reasons",
            "access",
            "content",
            "enumeration",
            "resource",
        ];
        assert_eq!(kinds.len(), expected.len());
        for name in expected {
            assert!(kinds.contains(name), "{name} missing from the fixture");
        }
    }

    #[test]
    fn schema_round_trip_and_inspection_is_exact() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("bundle.eidos-observation.zst");
        let expected = bundle();
        write_bundle(&file, &expected).unwrap();
        assert_eq!(read_bundle(&file).unwrap(), expected);

        let inspection = inspect_bundle(&file).unwrap();
        let value = serde_json::to_value(expected).unwrap();
        let mut exact = BTreeSet::new();
        collect_fields("", &value, &mut exact);
        assert_eq!(inspection.fields, exact.into_iter().collect::<Vec<_>>());
    }

    /// The streaming export must produce exactly the bundle the materialising
    /// path would have written, for every record variant in the fixture.
    #[test]
    fn streamed_export_matches_a_materialised_bundle() {
        let expected = bundle();
        let temp = tempfile::tempdir().unwrap();
        let spool_file = temp.path().join("spool.db");
        {
            let mut spool = crate::spool::Spool::open(
                &spool_file,
                crate::spool::SpoolLimits {
                    detailed_max_bytes: 1 << 30,
                    detailed_max_age_ns: i64::MAX,
                    summary_max_age_ns: i64::MAX,
                },
            )
            .unwrap();
            for record in &expected.records {
                spool.append(record).unwrap();
            }
        }

        let out = temp.path().join("streamed.eidos-observation.zst");
        let written = crate::spool::export_bundle(&spool_file, &expected.manifest, &out).unwrap();
        assert_eq!(written as usize, expected.records.len());

        let streamed = read_bundle(&out).unwrap();
        assert_eq!(streamed, expected);

        // And an empty ring still yields a well-formed, readable bundle.
        let empty_file = temp.path().join("empty.db");
        drop(
            crate::spool::Spool::open(
                &empty_file,
                crate::spool::SpoolLimits {
                    detailed_max_bytes: 1 << 20,
                    detailed_max_age_ns: i64::MAX,
                    summary_max_age_ns: i64::MAX,
                },
            )
            .unwrap(),
        );
        let empty_out = temp.path().join("empty.eidos-observation.zst");
        assert_eq!(
            crate::spool::export_bundle(&empty_file, &expected.manifest, &empty_out).unwrap(),
            0
        );
        assert!(read_bundle(&empty_out).unwrap().records.is_empty());
    }

    #[test]
    fn complete_bundle_atomically_replaces_an_existing_file() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("bundle.eidos-observation.zst");
        std::fs::write(&file, b"old-incomplete-data").unwrap();
        let expected = bundle();
        write_bundle(&file, &expected).unwrap();
        assert_eq!(read_bundle(&file).unwrap(), expected);
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[test]
    fn durable_schema_has_no_raw_identity_field() {
        let value = serde_json::to_value(bundle()).unwrap();
        let mut fields = BTreeSet::new();
        collect_fields("", &value, &mut fields);
        for field in fields {
            let leaf = field.rsplit('.').next().unwrap_or(&field);
            assert!(
                !matches!(
                    leaf,
                    "path"
                        | "filename"
                        | "name"
                        | "hostname"
                        | "username"
                        | "host"
                        | "arguments"
                        | "command_line"
                        | "image"
                        | "content"
                        | "ip"
                        | "serial"
                        | "volume_serial"
                        | "journal_id"
                        | "frn"
                        | "pid"
                ),
                "raw identity field {field}"
            );
        }
    }
}
