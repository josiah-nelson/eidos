//! NTFS inventory: a streaming pass over the Master File Table of one
//! partition, emitting a [`Member`] per live file and directory.
//!
//! Streaming the MFT rather than walking directory indexes keeps the work
//! linear in the number of records and reads each one exactly once, which is
//! what a cold image on spinning or networked storage wants. The cost is
//! that paths must be reconstructed afterwards from each record's
//! `$FILE_NAME` parent reference; that resolution is iterative, depth-capped,
//! and cycle-tolerant, so a corrupt or hostile MFT cannot recurse the
//! process to death or spin forever.
//!
//! `$`-prefixed NTFS metafiles (`$MFT`, `$LogFile`, the `$Extend` subtree, …)
//! are counted but not emitted: they describe the filesystem rather than its
//! contents.

use crate::{flag, DiskImageError, DiskImageLimits, Member, Outcome, Partition};
use eidos_domain::UnixNanos;
use ntfs::attribute_value::NtfsAttributeValue;
use ntfs::structured_values::{NtfsFileName, NtfsFileNamespace, NtfsVolumeFlags};
use ntfs::{
    KnownNtfsFileRecordNumber, Ntfs, NtfsAttributeType, NtfsError, NtfsFile, NtfsFileFlags,
    NtfsTime,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek};

/// Record numbers below this are NTFS's own reserved metafiles.
const FIRST_USER_RECORD: u64 = 16;
const ROOT_RECORD: u64 = KnownNtfsFileRecordNumber::RootDirectory as u64;

/// What one NTFS partition yielded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeReport {
    /// Index of this volume in [`crate::ImageReport::volumes`].
    pub volume: u32,
    /// Index of the partition it was read from.
    pub partition: u32,
    pub start: u64,
    pub length: u64,
    pub label: String,
    pub serial_number: u64,
    pub cluster_size: u32,
    /// The volume requests a consistency check. A read-only inventory can
    /// still return useful members, but cannot claim they are complete.
    pub dirty: bool,
    /// Records the `$MFT` itself claims to hold.
    pub mft_records: u64,
    /// Records actually read, after budgets.
    pub scanned_records: u64,
    pub member_count: u64,
    pub dir_count: u64,
    /// Live `$`-prefixed metafiles, which are counted but never emitted.
    pub metafile_count: u64,
    /// Records that have never been used (no `FILE` signature).
    pub unused_records: u64,
    /// Records that once held a file and are now free.
    pub deleted_records: u64,
    /// In-use records the parser could not decode.
    pub unreadable_records: u64,
    /// Members whose `$STANDARD_INFORMATION` was unreadable and whose
    /// timestamps therefore use the less-current `$FILE_NAME` copies.
    pub fallback_metadata_count: u64,
    /// Live members dropped because their path exceeded a path budget.
    pub skipped_deep: u64,
    /// Members whose name or path was flagged (see [`crate::flag`]).
    pub suspicious_count: u64,
    /// Sum of the emitted members' logical sizes.
    pub declared_size: u64,
    pub allocated_size: u64,
    /// Emitted members whose allocated size is an upper bound because their
    /// data spans an NTFS attribute list the dependency cannot expose as a
    /// single sequence of data runs.
    pub inexact_allocation_count: u64,
    pub outcome: Outcome,
    pub truncated_reason: Option<String>,
}

/// One MFT record's raw facts, before paths are reconstructed.
struct Record {
    links: Vec<Link>,
    sequence: u16,
    is_dir: bool,
    size: u64,
    allocated: u64,
    allocation_exact: bool,
    created: Option<UnixNanos>,
    modified: Option<UnixNanos>,
    accessed: Option<UnixNanos>,
    changed: Option<UnixNanos>,
    hard_links: u16,
    fallback_metadata: bool,
}

/// One user-visible `$FILE_NAME`. Files may have several of these; DOS 8.3
/// aliases are excluded when a long name exists for the same record.
#[derive(Clone)]
struct Link {
    name: String,
    flags: u32,
    parent: u64,
    parent_sequence: u16,
    /// `$FILE_NAME` carries per-link timestamp copies. They are only used
    /// when this record's authoritative `$STANDARD_INFORMATION` cannot be
    /// read, but keeping them on the link avoids borrowing one hard link's
    /// stale metadata for every sibling link.
    created: Option<UnixNanos>,
    modified: Option<UnixNanos>,
    accessed: Option<UnixNanos>,
    changed: Option<UnixNanos>,
}

struct ParsedNames {
    links: Vec<Link>,
    fallback: NtfsFileName,
}

fn time(t: NtfsTime) -> Option<UnixNanos> {
    let ticks = t.nt_timestamp();
    (ticks != 0 && ticks <= i64::MAX as u64).then(|| UnixNanos::from_filetime_ticks(ticks as i64))
}

/// Normalise a stored name into one path segment; returns (segment, flags).
///
/// NTFS forbids separators and NUL in names, so anything found here is a
/// sign of corruption or of a name written to be misread once it reaches a
/// path namespace.
fn normalize(raw: &str) -> (String, u32) {
    let mut flags = 0u32;
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '/' | '\\' => {
                flags |= flag::SEPARATOR;
                out.push('_');
            }
            // `to_string_lossy` maps unpaired surrogates to the replacement
            // character; a genuine one in a name is worth the same suspicion.
            '\u{FFFD}' => {
                flags |= flag::ENCODING;
                out.push(c);
            }
            c if c.is_control() => flags |= flag::CONTROL,
            c => out.push(c),
        }
    }
    if out == "." || out == ".." {
        flags |= flag::TRAVERSAL;
        out.clear();
    }
    if out.is_empty() {
        flags |= flag::EMPTY;
    }
    (out, flags)
}

/// Collect every long/POSIX link name. A DOS-only record remains visible as
/// a flagged fallback, while ordinary 8.3 aliases do not become duplicate
/// catalog entries.
fn file_names<R: Read + Seek>(file: &NtfsFile<'_>, fs: &mut R) -> Result<Option<ParsedNames>, ()> {
    let mut long = Vec::new();
    let mut dos = Vec::new();
    let mut attributes = file.attributes();
    while let Some(item) = attributes.next(fs) {
        let item = item.map_err(|_| ())?;
        let attribute = item.to_attribute().map_err(|_| ())?;
        if attribute.ty().map_err(|_| ())? != NtfsAttributeType::FileName {
            continue;
        }
        let file_name = attribute
            .structured_value::<_, NtfsFileName>(fs)
            .map_err(|_| ())?;
        if file_name.namespace() == NtfsFileNamespace::Dos {
            dos.push(file_name);
        } else {
            long.push(file_name);
        }
    }
    let (names, short) = if long.is_empty() {
        (dos, true)
    } else {
        (long, false)
    };
    let Some(fallback) = names.first().cloned() else {
        // Attribute-extension records legitimately have no name.
        return Ok(None);
    };
    let mut links: Vec<Link> = names
        .into_iter()
        .map(|file_name| {
            let (name, mut flags) = normalize(&file_name.name().to_string_lossy());
            if short {
                flags |= flag::SHORT_NAME;
            }
            let parent = file_name.parent_directory_reference();
            Link {
                name,
                flags,
                parent: parent.file_record_number(),
                parent_sequence: parent.sequence_number(),
                created: time(file_name.creation_time()),
                modified: time(file_name.modification_time()),
                accessed: time(file_name.access_time()),
                changed: time(file_name.mft_record_modification_time()),
            }
        })
        .collect();
    links.sort_by(|a, b| {
        (a.parent, a.parent_sequence, &a.name).cmp(&(b.parent, b.parent_sequence, &b.name))
    });
    links.dedup_by(|a, b| {
        a.parent == b.parent && a.parent_sequence == b.parent_sequence && a.name == b.name
    });
    Ok(Some(ParsedNames { links, fallback }))
}

/// Logical and on-disk size of the unnamed `$DATA` stream.
///
/// The `$FILE_NAME` copies of these sizes are only refreshed when a name
/// changes, so they are routinely stale; the attribute itself is
/// authoritative for the logical size. On-disk size is derived rather than
/// read, because the crate does not expose the non-resident header: resident
/// data occupies no clusters, and non-resident data is rounded up to a whole
/// number of them. That over-states sparse and compressed streams, which
/// this slice does not distinguish.
struct DataExtent {
    size: u64,
    allocated: u64,
    allocation_exact: bool,
}

fn data_extent<R: Read + Seek>(
    file: &NtfsFile<'_>,
    fs: &mut R,
    cluster: u64,
) -> Result<Option<DataExtent>, NtfsError> {
    let Some(item) = file.data(fs, "") else {
        return Ok(None);
    };
    let item = item?;
    let attribute = item.to_attribute()?;
    let size = attribute.value_length();
    let value = attribute.value(fs)?;
    let (allocated, allocation_exact) = match value {
        NtfsAttributeValue::Resident(_) => (0, true),
        NtfsAttributeValue::NonResident(value) => {
            let mut allocated = 0u64;
            for run in value.data_runs() {
                let run = run?;
                if run.data_position().value().is_some() {
                    allocated = allocated.saturating_add(run.allocated_size());
                }
            }
            (allocated, true)
        }
        NtfsAttributeValue::AttributeListNonResident(_) => {
            (size.div_ceil(cluster).saturating_mul(cluster), false)
        }
    };
    Ok(Some(DataExtent {
        size,
        allocated,
        allocation_exact,
    }))
}

enum Resolved {
    Path(String, u32),
    TooDeep,
    TooLong,
}

/// Rebuild a member's virtual path by walking `$FILE_NAME` parent references
/// up to the volume root. Bounded by both path budgets, and tolerant of a
/// missing or self-referential ancestor.
///
/// `seen` is scratch space the caller reuses across members: a set rather
/// than a list so that an image whose every record sits at the depth limit
/// costs linear, not quadratic, work per member.
fn resolve(
    record: u64,
    link: &Link,
    records: &HashMap<u64, Record>,
    limits: &DiskImageLimits,
    seen: &mut HashSet<u64>,
) -> Option<Resolved> {
    if limits.max_path_depth == 0 {
        return Some(Resolved::TooDeep);
    }
    let mut segments = vec![link.name.clone()];
    let mut bytes = segments[0].len();
    if bytes > limits.max_path_bytes {
        return Some(Resolved::TooLong);
    }
    let mut flags = link.flags;
    let mut current = link.parent;
    let mut expected_sequence = link.parent_sequence;
    seen.clear();
    seen.insert(record);
    loop {
        if current == ROOT_RECORD {
            if records
                .get(&current)
                .is_some_and(|root| root.sequence == expected_sequence)
            {
                break;
            }
            flags |= flag::ORPHAN;
            let segment = format!("(orphan {current})");
            bytes = bytes.saturating_add(segment.len() + 1);
            if bytes > limits.max_path_bytes {
                return Some(Resolved::TooLong);
            }
            segments.push(segment);
            break;
        }
        if segments.len() >= limits.max_path_depth {
            return Some(Resolved::TooDeep);
        }
        if !seen.insert(current) {
            // A parent cycle: root the path at a synthetic segment rather
            // than walking it forever.
            flags |= flag::ORPHAN;
            let segment = format!("(cycle {current})");
            bytes = bytes.saturating_add(segment.len() + 1);
            if bytes > limits.max_path_bytes {
                return Some(Resolved::TooLong);
            }
            segments.push(segment);
            break;
        }
        match records
            .get(&current)
            .filter(|parent| parent.sequence == expected_sequence)
        {
            Some(parent) => {
                let Some(parent_link) = parent.links.first() else {
                    flags |= flag::ORPHAN;
                    let segment = format!("(orphan {current})");
                    bytes = bytes.saturating_add(segment.len() + 1);
                    if bytes > limits.max_path_bytes {
                        return Some(Resolved::TooLong);
                    }
                    segments.push(segment);
                    break;
                };
                segments.push(parent_link.name.clone());
                flags |= parent_link.flags;
                current = parent_link.parent;
                expected_sequence = parent_link.parent_sequence;
            }
            None => {
                flags |= flag::ORPHAN;
                let segment = format!("(orphan {current})");
                bytes = bytes.saturating_add(segment.len() + 1);
                if bytes > limits.max_path_bytes {
                    return Some(Resolved::TooLong);
                }
                segments.push(segment);
                break;
            }
        }
        bytes += segments.last().map_or(0, |s| s.len()) + 1;
        if bytes > limits.max_path_bytes {
            return Some(Resolved::TooLong);
        }
    }
    segments.reverse();
    Some(Resolved::Path(segments.join("/"), flags))
}

/// Whether one link belongs to NTFS's own housekeeping: a reserved low
/// record, or a descendant of one such as the `$Extend` subtree. A user file
/// merely named with `$` remains ordinary. Ancestor verdicts are memoised.
fn is_metafile_link(
    record: u64,
    link: &Link,
    records: &HashMap<u64, Record>,
    memo: &mut HashMap<(u64, u16), bool>,
    depth_limit: usize,
) -> bool {
    if record == ROOT_RECORD {
        return false;
    }
    if record < FIRST_USER_RECORD {
        return true;
    }
    let mut chain = Vec::new();
    let mut current = link.parent;
    let mut expected_sequence = link.parent_sequence;
    let verdict = loop {
        // The root directory is reserved but holds user files, so reaching
        // it means the chain was ordinary all the way up.
        if current == ROOT_RECORD {
            break false;
        }
        let identity = (current, expected_sequence);
        if let Some(&known) = memo.get(&identity) {
            break known;
        }
        let Some(raw) = records
            .get(&current)
            .filter(|raw| raw.sequence == expected_sequence)
        else {
            break false;
        };
        if current < FIRST_USER_RECORD {
            break true;
        }
        if chain.len() >= depth_limit {
            break false;
        }
        let Some(parent_link) = raw.links.first() else {
            break false;
        };
        chain.push(identity);
        current = parent_link.parent;
        expected_sequence = parent_link.parent_sequence;
    };
    for identity in chain {
        memo.insert(identity, verdict);
    }
    verdict
}

/// Inventory one NTFS partition. `fs` must cover exactly that partition.
pub fn inventory_volume<R: Read + Seek, F: FnMut(Member)>(
    fs: &mut R,
    volume: u32,
    partition: &Partition,
    limits: &DiskImageLimits,
    remaining_members: &mut u64,
    sink: &mut F,
) -> Result<VolumeReport, DiskImageError> {
    let ntfs = Ntfs::new(fs).map_err(|e| DiskImageError::Ntfs(e.to_string()))?;
    let dirty = ntfs
        .volume_info(fs)
        .map_err(|e| DiskImageError::Ntfs(format!("$Volume information is unreadable: {e}")))?
        .flags()
        .contains(NtfsVolumeFlags::IS_DIRTY);
    let (label, label_unreadable) = match ntfs.volume_name(fs) {
        Some(Ok(name)) => (name.name().to_string_lossy(), false),
        Some(Err(_)) => (String::new(), true),
        None => (String::new(), false),
    };
    let mut report = VolumeReport {
        volume,
        partition: partition.index,
        start: partition.start,
        length: partition.length,
        label,
        serial_number: ntfs.serial_number(),
        cluster_size: ntfs.cluster_size(),
        dirty,
        mft_records: 0,
        scanned_records: 0,
        member_count: 0,
        dir_count: 0,
        metafile_count: 0,
        unused_records: 0,
        deleted_records: 0,
        unreadable_records: 0,
        fallback_metadata_count: 0,
        skipped_deep: 0,
        suspicious_count: 0,
        declared_size: 0,
        allocated_size: 0,
        inexact_allocation_count: 0,
        outcome: Outcome::Complete,
        truncated_reason: None,
    };
    if dirty {
        report.outcome = Outcome::Partial;
        report.truncated_reason = Some(
            "NTFS volume is dirty; uncommitted filesystem changes may not be represented".into(),
        );
    }
    if label_unreadable {
        report.outcome = Outcome::Partial;
        report.truncated_reason = Some("NTFS volume name is unreadable".into());
    }

    let mft = ntfs
        .file(fs, KnownNtfsFileRecordNumber::MFT as u64)
        .map_err(|e| DiskImageError::Ntfs(format!("$MFT is unreadable: {e}")))?;
    let record_size = ntfs.file_record_size().max(1) as u64;
    let cluster = ntfs.cluster_size().max(1) as u64;
    report.mft_records = data_extent(&mft, fs, cluster)
        .map_err(|e| DiskImageError::Ntfs(format!("$MFT data is unreadable: {e}")))?
        .ok_or_else(|| DiskImageError::Ntfs("$MFT has no unnamed data stream".into()))?
        .size
        / record_size;
    drop(mft);

    let scan = report.mft_records.min(limits.max_mft_records);
    if scan < report.mft_records {
        report.outcome = Outcome::Partial;
        report.truncated_reason = Some(format!(
            "MFT record limit ({}) reached; the volume holds {}",
            limits.max_mft_records, report.mft_records
        ));
    }

    let mut records: HashMap<u64, Record> = HashMap::new();
    for number in 0..scan {
        if records.len() as u64 >= *remaining_members {
            report.outcome = Outcome::Partial;
            report.truncated_reason.get_or_insert_with(|| {
                format!(
                    "aggregate member limit ({}) reached after {number} MFT records",
                    limits.max_members
                )
            });
            break;
        }
        report.scanned_records += 1;
        let file = match ntfs.file(fs, number) {
            Ok(file) => file,
            // An untouched or wiped record has no `FILE` signature; that is
            // the normal state of most of a freshly formatted MFT, not a
            // fault worth reporting as one.
            Err(NtfsError::InvalidFileSignature { .. }) => {
                report.unused_records += 1;
                continue;
            }
            Err(_) => {
                report.unreadable_records += 1;
                continue;
            }
        };
        if !file.flags().contains(NtfsFileFlags::IN_USE) {
            report.deleted_records += 1;
            continue;
        }
        // Extension records hold overflow attributes and carry no name of
        // their own; their contents already belong to their base record.
        let ParsedNames {
            mut links,
            fallback,
        } = match file_names(&file, fs) {
            Ok(Some(names)) => names,
            Ok(None) => continue,
            Err(()) => {
                report.unreadable_records += 1;
                continue;
            }
        };
        for link in &mut links {
            if link.name.is_empty() {
                link.name = format!("(unnamed record {number})");
            }
        }
        let is_dir = file.is_directory();
        let extent = if is_dir {
            DataExtent {
                size: 0,
                allocated: 0,
                allocation_exact: true,
            }
        } else {
            match data_extent(&file, fs, cluster) {
                Ok(Some(extent)) => extent,
                Ok(None) => DataExtent {
                    size: fallback.data_size(),
                    allocated: fallback.allocated_size(),
                    allocation_exact: false,
                },
                Err(_) => {
                    report.unreadable_records += 1;
                    continue;
                }
            }
        };
        let info = file.info();
        let fallback_metadata = info.is_err();
        if fallback_metadata {
            report.fallback_metadata_count += links.len() as u64;
        }
        let info = info.ok();
        records.insert(
            number,
            Record {
                links,
                sequence: file.sequence_number(),
                is_dir,
                size: extent.size,
                allocated: extent.allocated,
                allocation_exact: extent.allocation_exact,
                created: info.as_ref().and_then(|i| time(i.creation_time())),
                modified: info.as_ref().and_then(|i| time(i.modification_time())),
                accessed: info.as_ref().and_then(|i| time(i.access_time())),
                changed: info
                    .as_ref()
                    .and_then(|i| time(i.mft_record_modification_time())),
                hard_links: file.hard_link_count(),
                fallback_metadata,
            },
        );
    }

    if report.unreadable_records != 0 {
        report.outcome = Outcome::Partial;
        report.truncated_reason.get_or_insert_with(|| {
            format!(
                "{} MFT records could not be decoded",
                report.unreadable_records
            )
        });
    }
    if report.fallback_metadata_count != 0 {
        report.outcome = Outcome::Partial;
        report.truncated_reason.get_or_insert_with(|| {
            format!(
                "{} members use stale fallback timestamps",
                report.fallback_metadata_count
            )
        });
    }

    let mut memo: HashMap<(u64, u16), bool> = HashMap::new();
    let mut seen: HashSet<u64> = HashSet::new();
    let mut numbers: Vec<u64> = records.keys().copied().collect();
    numbers.sort_unstable();
    'records: for number in numbers {
        if number == ROOT_RECORD {
            // The root directory *is* the volume; it has no path of its own.
            continue;
        }
        let raw = &records[&number];
        let mut metafile = false;
        for link in &raw.links {
            if is_metafile_link(number, link, &records, &mut memo, limits.max_path_depth) {
                metafile = true;
                continue;
            }
            if *remaining_members == 0 {
                report.outcome = Outcome::Partial;
                report.truncated_reason.get_or_insert_with(|| {
                    format!("aggregate member limit ({}) reached", limits.max_members)
                });
                break 'records;
            }
            let (path, flags) = match resolve(number, link, &records, limits, &mut seen) {
                Some(Resolved::Path(path, flags)) => (path, flags),
                Some(Resolved::TooDeep) | Some(Resolved::TooLong) => {
                    report.skipped_deep += 1;
                    report.outcome = Outcome::Partial;
                    report.truncated_reason.get_or_insert_with(|| {
                        format!(
                            "path budget (depth {}, {} bytes) reached at MFT record {number}",
                            limits.max_path_depth, limits.max_path_bytes
                        )
                    });
                    continue;
                }
                None => continue,
            };
            if flags != 0 {
                report.suspicious_count += 1;
            }
            let (parent, name) = match path.rsplit_once('/') {
                Some((p, n)) => (p.to_string(), n.to_string()),
                None => (String::new(), path.clone()),
            };
            if raw.is_dir {
                report.dir_count += 1;
            } else {
                report.declared_size = report.declared_size.saturating_add(raw.size);
                report.allocated_size = report.allocated_size.saturating_add(raw.allocated);
                if !raw.allocation_exact {
                    report.inexact_allocation_count += 1;
                }
            }
            report.member_count += 1;
            *remaining_members -= 1;
            sink(Member {
                volume,
                record: number,
                parent_record: link.parent,
                path,
                name,
                parent,
                is_dir: raw.is_dir,
                size: raw.size,
                allocated: raw.allocated,
                allocation_exact: raw.allocation_exact,
                created: if raw.fallback_metadata {
                    link.created
                } else {
                    raw.created
                },
                modified: if raw.fallback_metadata {
                    link.modified
                } else {
                    raw.modified
                },
                accessed: if raw.fallback_metadata {
                    link.accessed
                } else {
                    raw.accessed
                },
                changed: if raw.fallback_metadata {
                    link.changed
                } else {
                    raw.changed
                },
                hard_links: raw.hard_links,
                flags,
            });
        }
        if metafile {
            report.metafile_count += 1;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, parent: u64) -> Record {
        Record {
            links: vec![Link {
                name: name.to_string(),
                flags: 0,
                parent,
                parent_sequence: 1,
                created: None,
                modified: None,
                accessed: None,
                changed: None,
            }],
            sequence: 1,
            is_dir: true,
            size: 0,
            allocated: 0,
            allocation_exact: true,
            created: None,
            modified: None,
            accessed: None,
            changed: None,
            hard_links: 1,
            fallback_metadata: false,
        }
    }

    fn tree(entries: &[(u64, &str, u64)]) -> HashMap<u64, Record> {
        let mut records: HashMap<u64, Record> = entries
            .iter()
            .map(|(n, name, parent)| (*n, record(name, *parent)))
            .collect();
        records
            .entry(ROOT_RECORD)
            .or_insert_with(|| record("$Root", ROOT_RECORD));
        records
    }

    fn resolve_one(
        record: u64,
        records: &HashMap<u64, Record>,
        limits: &DiskImageLimits,
    ) -> Option<Resolved> {
        resolve(
            record,
            records.get(&record)?.links.first()?,
            records,
            limits,
            &mut HashSet::new(),
        )
    }

    fn path_of(record: u64, records: &HashMap<u64, Record>, limits: &DiskImageLimits) -> String {
        match resolve_one(record, records, limits) {
            Some(Resolved::Path(path, _)) => path,
            _ => panic!("record {record} did not resolve"),
        }
    }

    fn metafile(
        record: u64,
        records: &HashMap<u64, Record>,
        memo: &mut HashMap<(u64, u16), bool>,
    ) -> bool {
        let fallback = Link {
            name: String::new(),
            flags: 0,
            parent: ROOT_RECORD,
            parent_sequence: 1,
            created: None,
            modified: None,
            accessed: None,
            changed: None,
        };
        let link = records
            .get(&record)
            .and_then(|raw| raw.links.first())
            .unwrap_or(&fallback);
        is_metafile_link(record, link, records, memo, 256)
    }

    #[test]
    fn names_are_normalised_and_flagged() {
        assert_eq!(normalize("ledger.txt"), ("ledger.txt".into(), 0));
        assert_eq!(
            normalize("grünwald-πλούτος.txt"),
            ("grünwald-πλούτος.txt".into(), 0)
        );
        let (name, flags) = normalize("up\\over/out");
        assert_eq!(name, "up_over_out");
        assert_eq!(flags, flag::SEPARATOR);
        let (name, flags) = normalize("bell\u{7}.txt");
        assert_eq!(name, "bell.txt");
        assert_eq!(flags, flag::CONTROL);
        assert_eq!(normalize("..").1, flag::TRAVERSAL | flag::EMPTY);
        assert_eq!(normalize("").1, flag::EMPTY);
        assert_eq!(normalize("lo\u{FFFD}st").1, flag::ENCODING);
    }

    #[test]
    fn paths_are_rebuilt_from_parent_references() {
        let limits = DiskImageLimits::default();
        let records = tree(&[
            (32, "corpus", ROOT_RECORD),
            (33, "alcove", 32),
            (34, "ledger.txt", 33),
            (35, "brindle.bin", ROOT_RECORD),
        ]);
        assert_eq!(path_of(34, &records, &limits), "corpus/alcove/ledger.txt");
        assert_eq!(path_of(35, &records, &limits), "brindle.bin");
    }

    #[test]
    fn each_hard_link_resolves_to_its_own_path() {
        let mut records = tree(&[
            (32, "first", ROOT_RECORD),
            (33, "second", ROOT_RECORD),
            (34, "ledger.txt", 32),
        ]);
        records.get_mut(&34).unwrap().links.push(Link {
            name: "ledger-link.txt".into(),
            flags: 0,
            parent: 33,
            parent_sequence: 1,
            created: None,
            modified: None,
            accessed: None,
            changed: None,
        });
        let paths: Vec<String> = records[&34]
            .links
            .iter()
            .map(|link| {
                match resolve(
                    34,
                    link,
                    &records,
                    &DiskImageLimits::default(),
                    &mut HashSet::new(),
                ) {
                    Some(Resolved::Path(path, _)) => path,
                    _ => panic!("link did not resolve"),
                }
            })
            .collect();
        assert_eq!(paths, ["first/ledger.txt", "second/ledger-link.txt"]);
    }

    #[test]
    fn a_missing_or_cyclic_ancestor_is_flagged_not_followed() {
        let limits = DiskImageLimits::default();
        let orphaned = tree(&[(40, "stray.txt", 999)]);
        match resolve_one(40, &orphaned, &limits) {
            Some(Resolved::Path(path, flags)) => {
                assert_eq!(path, "(orphan 999)/stray.txt");
                assert_eq!(flags, flag::ORPHAN);
            }
            _ => panic!("orphan did not resolve"),
        }
        // Two directories that claim each other as parent must terminate.
        let looped = tree(&[(41, "a", 42), (42, "b", 41)]);
        match resolve_one(41, &looped, &limits) {
            Some(Resolved::Path(path, flags)) => {
                assert_eq!(path, "(cycle 41)/b/a");
                assert_eq!(flags, flag::ORPHAN);
            }
            _ => panic!("cycle did not resolve"),
        }

        let mut stale = tree(&[(43, "child", 44), (44, "reused", ROOT_RECORD)]);
        stale.get_mut(&43).unwrap().links[0].parent_sequence = 2;
        match resolve_one(43, &stale, &limits) {
            Some(Resolved::Path(path, flags)) => {
                assert_eq!(path, "(orphan 44)/child");
                assert_eq!(flags, flag::ORPHAN);
            }
            _ => panic!("stale parent reference did not resolve as an orphan"),
        }
        let mut stale_root = tree(&[(44, "child", ROOT_RECORD)]);
        stale_root.get_mut(&44).unwrap().links[0].parent_sequence = 2;
        match resolve_one(44, &stale_root, &limits) {
            Some(Resolved::Path(path, flags)) => {
                assert_eq!(path, "(orphan 5)/child");
                assert_eq!(flags, flag::ORPHAN);
            }
            _ => panic!("stale root reference did not resolve as an orphan"),
        }
    }

    #[test]
    fn path_budgets_drop_a_member_instead_of_the_scan() {
        let mut entries: Vec<(u64, String, u64)> = Vec::new();
        let mut parent = ROOT_RECORD;
        for depth in 0..40u64 {
            entries.push((100 + depth, format!("tier{depth:02}"), parent));
            parent = 100 + depth;
        }
        let records: HashMap<u64, Record> = entries
            .iter()
            .map(|(n, name, parent)| (*n, record(name, *parent)))
            .collect();
        let deepest = 139;
        assert!(path_of(deepest, &records, &DiskImageLimits::default()).ends_with("tier39"));
        let shallow = DiskImageLimits {
            max_path_depth: 8,
            ..DiskImageLimits::default()
        };
        assert!(matches!(
            resolve_one(deepest, &records, &shallow),
            Some(Resolved::TooDeep)
        ));
        let narrow = DiskImageLimits {
            max_path_bytes: 24,
            ..DiskImageLimits::default()
        };
        assert!(matches!(
            resolve_one(deepest, &records, &narrow),
            Some(Resolved::TooLong)
        ));

        let root_child = tree(&[(200, "wide-name", ROOT_RECORD)]);
        let no_segments = DiskImageLimits {
            max_path_depth: 0,
            ..DiskImageLimits::default()
        };
        assert!(matches!(
            resolve_one(200, &root_child, &no_segments),
            Some(Resolved::TooDeep)
        ));
        let too_narrow = DiskImageLimits {
            max_path_bytes: 8,
            ..DiskImageLimits::default()
        };
        assert!(matches!(
            resolve_one(200, &root_child, &too_narrow),
            Some(Resolved::TooLong)
        ));

        let orphan = tree(&[(201, "ok", 999)]);
        let synthetic_too_long = DiskImageLimits {
            max_path_bytes: 10,
            ..DiskImageLimits::default()
        };
        assert!(matches!(
            resolve_one(201, &orphan, &synthetic_too_long),
            Some(Resolved::TooLong)
        ));
    }

    #[test]
    fn suspicious_ancestor_flags_propagate_to_descendants() {
        let mut records = tree(&[(32, "normalised", ROOT_RECORD), (33, "ledger.txt", 32)]);
        records.get_mut(&32).unwrap().links[0].flags = flag::SEPARATOR;
        match resolve_one(33, &records, &DiskImageLimits::default()) {
            Some(Resolved::Path(path, flags)) => {
                assert_eq!(path, "normalised/ledger.txt");
                assert_eq!(flags, flag::SEPARATOR);
            }
            _ => panic!("descendant did not resolve"),
        }
    }

    #[test]
    fn metafiles_are_recognised_through_the_extend_subtree() {
        let mut records = tree(&[
            // $Extend is record 11; its children inherit metafile status.
            (11, "$Extend", ROOT_RECORD),
            (24, "quota-without-dollar", 11),
            (27, "$RmMetadata", 11),
            (28, "$TxfLog", 27),
            // A user file that merely starts with $ is not a metafile.
            (32, "$savings.txt", ROOT_RECORD),
            (33, "ledger.txt", 32),
        ]);
        let mut memo = HashMap::new();
        for meta in [0, 11, 24, 27, 28] {
            assert!(metafile(meta, &records, &mut memo), "{meta}");
        }
        // The root directory is reserved but holds user files, and a user
        // file whose name merely starts with `$` is not housekeeping.
        for user in [ROOT_RECORD, 32, 33] {
            assert!(!metafile(user, &records, &mut memo), "{user}");
        }
        // A stale reference to a reused reserved slot must not suppress an
        // otherwise visible member as NTFS metadata.
        records.get_mut(&24).unwrap().links[0].parent_sequence = 2;
        memo.clear();
        assert!(!metafile(24, &records, &mut memo));
    }
}
