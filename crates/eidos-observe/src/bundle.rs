use crate::schema::{ObservationBundle, SCHEMA_VERSION};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

pub fn write_bundle(file: &Path, bundle: &ObservationBundle) -> Result<(), BundleError> {
    if bundle.manifest.schema != SCHEMA_VERSION {
        return Err(BundleError::Schema(bundle.manifest.schema.clone()));
    }
    let output = BufWriter::new(File::create(file)?);
    let mut encoder = zstd::stream::write::Encoder::new(output, 9)?;
    serde_json::to_writer(&mut encoder, bundle)?;
    encoder.finish()?;
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
                },
                capture_gaps: Vec::new(),
                drops: DropCounters::default(),
                units: Units::default(),
            },
            records: vec![ObservationRecord::Mark(MarkRecord {
                at: TimeAnchor {
                    monotonic_ns: 10,
                    utc_ns: 20,
                },
                marker: key.token("mark", b"phase-a"),
            })],
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

    #[test]
    fn durable_schema_has_no_raw_identity_field() {
        let value = serde_json::to_value(bundle()).unwrap();
        let mut fields = BTreeSet::new();
        collect_fields("", &value, &mut fields);
        for field in fields {
            let leaf = field.rsplit('.').next().unwrap_or(&field);
            assert!(!matches!(
                leaf,
                "path" | "filename" | "hostname" | "username" | "arguments" | "content" | "ip"
            ));
        }
    }
}
