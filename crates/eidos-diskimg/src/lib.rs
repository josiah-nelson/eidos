//! Read-only inventory of NTFS directory entries inside VHDX images, without
//! booting or mounting them.
//!
//! The layers remain independently usable: [`vhdx`] exposes a fixed or
//! dynamic virtual disk, [`partition`] validates GPT (including its backup)
//! or MBR, and [`volume`] streams the NTFS MFT under explicit budgets. The
//! top-level [`inventory`] APIs compose them and preserve every reason a
//! result is partial.
//!
//! Deliberate boundaries are reported rather than hidden. Differencing VHDX
//! images return [`Outcome::NeedsParent`]; a pending VHDX log, dirty NTFS
//! volume, extended MBR chain, damaged record, or exhausted budget returns
//! [`Outcome::Partial`]. A pending log yields a metadata-only report because
//! its payload cannot be read truthfully until an implementation replays the
//! log in memory. Named NTFS data streams are not separate members;
//! `size` and `allocated` describe the unnamed `$DATA` stream. Long/POSIX
//! hard-link names are separate members, while DOS 8.3 aliases are suppressed
//! unless they are the only readable name.

#[cfg(test)]
mod fixture;
pub mod partition;
pub mod vhdx;
pub mod volume;

use eidos_domain::UnixNanos;
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Instant;

pub use partition::{Partition, PartitionScheme};
pub use vhdx::{ParentLocator, PayloadKind, VhdxDisk, VhdxInfo};
pub use volume::VolumeReport;

/// How far an inventory got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Complete,
    /// A budget, damaged record, or unsupported linked structure made the
    /// result incomplete; every emitted member is still valid.
    Partial,
    /// A differencing disk cannot be interpreted without its parent image.
    NeedsParent,
}

/// Container formats this crate can open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    Vhdx,
}

/// Resource budgets applied while opening one image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskImageLimits {
    /// Directory-entry members emitted across every volume in one image.
    /// The same allowance, plus fixed headroom for NTFS's reserved records,
    /// bounds named MFT records and hard-link names retained while paths are
    /// reconstructed, so metadata memory stays bounded without consuming the
    /// entire output budget before the first user record.
    pub max_members: u64,
    /// MFT records read per volume before the scan is cut short.
    pub max_mft_records: u64,
    /// Longest virtual path accepted for one member, in UTF-8 bytes.
    pub max_path_bytes: usize,
    /// Most path segments accepted for one member.
    pub max_path_depth: usize,
    /// Partition-table entries examined.
    pub max_partitions: usize,
    /// GPT partition-array bytes checksummed before entries are trusted.
    pub max_partition_table_bytes: u64,
    /// Region-table entries accepted (the VHDX specification's own cap).
    pub max_region_entries: u32,
    /// Metadata-table entries accepted (likewise).
    pub max_metadata_entries: u16,
    /// Largest virtual disk size accepted, in bytes.
    pub max_virtual_size: u64,
    /// Block-allocation-table entries loaded, capping BAT memory at 8 bytes
    /// each.
    pub max_bat_entries: u64,
    /// Bytes retained for one parent-locator key or value.
    pub max_locator_bytes: usize,
}

impl Default for DiskImageLimits {
    fn default() -> Self {
        Self {
            max_members: 1_000_000,
            max_mft_records: 4_000_000,
            max_path_bytes: 4096,
            max_path_depth: 256,
            max_partitions: 128,
            max_partition_table_bytes: 16 * 1024 * 1024,
            max_region_entries: 2047,
            max_metadata_entries: 2047,
            max_virtual_size: 64 << 40,
            max_bat_entries: 4 << 20,
            max_locator_bytes: 4096,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DiskImageError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// No recognisable container signature: not an image this crate can read.
    #[error("not a VHDX image")]
    NotDiskImage,
    /// Structure inconsistent with the VHDX specification.
    #[error("corrupt image: {0}")]
    Corrupt(String),
    /// Well-formed but beyond what this reader supports.
    #[error("unsupported image: {0}")]
    Unsupported(String),
    /// The `ntfs` crate rejected the filesystem.
    #[error("unreadable NTFS volume: {0}")]
    Ntfs(String),
}

/// Why a member's stored name was not taken at face value (bit flags).
pub mod flag {
    /// Stored name was `.` or `..`.
    pub const TRAVERSAL: u32 = 1 << 0;
    /// Stored name contained a path separator, normalised to `_`.
    pub const SEPARATOR: u32 = 1 << 1;
    /// Control characters were stripped.
    pub const CONTROL: u32 = 1 << 2;
    /// UTF-16 decoding required replacement characters.
    pub const ENCODING: u32 = 1 << 3;
    /// Name was empty after normalisation.
    pub const EMPTY: u32 = 1 << 4;
    /// Parent reference was missing, stale, or cyclic.
    pub const ORPHAN: u32 = 1 << 5;
    /// Only a DOS 8.3 name was readable.
    pub const SHORT_NAME: u32 = 1 << 6;
}

/// One live directory entry found inside an image. Hard-linked files produce
/// one member per long/POSIX `$FILE_NAME`, sharing the same MFT `record`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub volume: u32,
    pub record: u64,
    pub parent_record: u64,
    /// Normalised virtual path relative to the volume root.
    pub path: String,
    pub name: String,
    pub parent: String,
    pub is_dir: bool,
    pub size: u64,
    pub allocated: u64,
    /// Whether `allocated` was computed from concrete NTFS data runs. When
    /// false it is a conservative cluster-rounded upper bound.
    pub allocation_exact: bool,
    pub created: Option<UnixNanos>,
    pub modified: Option<UnixNanos>,
    pub accessed: Option<UnixNanos>,
    pub changed: Option<UnixNanos>,
    pub hard_links: u16,
    /// `flag::*` bits, including flags inherited from path ancestors.
    pub flags: u32,
}

/// Image-level result of one inventory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageReport {
    pub format: ImageFormat,
    /// Virtual disk geometry and chain identity as the container declares it.
    pub image: VhdxInfo,
    pub scheme: Option<PartitionScheme>,
    pub partitions: Vec<Partition>,
    /// One entry per NTFS partition that was opened.
    pub volumes: Vec<VolumeReport>,
    /// NTFS-signature partitions that could not be parsed. One damaged
    /// volume does not discard useful results from the others.
    pub failed_volumes: Vec<(u32, String)>,
    pub member_count: u64,
    pub outcome: Outcome,
    /// Every independently observed reason the result is partial. Reasons
    /// are kept in discovery order and deduplicated.
    pub partial_reasons: Vec<String>,
    /// Bytes actually fetched from the image, including container metadata.
    pub bytes_read: u64,
    pub elapsed_ms: f64,
}

impl ImageReport {
    fn mark_partial(&mut self, reason: impl Into<String>) {
        if self.outcome == Outcome::Complete {
            self.outcome = Outcome::Partial;
        }
        let reason = reason.into();
        if !self.partial_reasons.contains(&reason) {
            self.partial_reasons.push(reason);
        }
    }
}

/// Inventory the NTFS directory entries in the image at `path`.
pub fn inventory<F: FnMut(Member)>(
    path: &Path,
    limits: &DiskImageLimits,
    sink: F,
) -> Result<ImageReport, DiskImageError> {
    let file = open_shared(path)?;
    let len = file.metadata()?.len();
    inventory_reader(file, len, limits, sink)
}

fn open_shared(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Observe a live image without blocking its writer or lifecycle.
        options.share_mode(0x1 | 0x2 | 0x4);
        options.custom_flags(0x1000_0000); // FILE_FLAG_RANDOM_ACCESS
    }
    options.open(path)
}

/// Inventory from any seekable reader of `len` bytes. Members are delivered
/// in volume, MFT-record, then link-name order.
pub fn inventory_reader<R: Read + Seek, F: FnMut(Member)>(
    reader: R,
    len: u64,
    limits: &DiskImageLimits,
    mut sink: F,
) -> Result<ImageReport, DiskImageError> {
    let started = Instant::now();
    let mut disk = VhdxDisk::open(reader, len, limits)?;
    let info = disk.info().clone();
    let payload = info.payload;
    let virtual_size = info.virtual_size;
    let sector_size = info.logical_sector_size;
    let mut report = ImageReport {
        format: ImageFormat::Vhdx,
        image: info,
        scheme: None,
        partitions: Vec::new(),
        volumes: Vec::new(),
        failed_volumes: Vec::new(),
        member_count: 0,
        outcome: Outcome::Complete,
        partial_reasons: Vec::new(),
        bytes_read: 0,
        elapsed_ms: 0.0,
    };
    if report.image.log_replay_pending {
        report.mark_partial(
            "VHDX log replay is pending; payload inventory requires the log to be replayed first",
        );
    }
    if payload == PayloadKind::Differencing {
        report.outcome = Outcome::NeedsParent;
        finish_report(&mut report, &disk, started);
        return Ok(report);
    }
    if report.image.log_replay_pending {
        finish_report(&mut report, &disk, started);
        return Ok(report);
    }

    let table = partition::read_table(&mut disk, virtual_size, sector_size, limits)?;
    report.scheme = table.scheme;
    report.partitions = table.partitions;
    if let Some(reason) = table.incomplete_reason {
        report.mark_partial(reason);
    }

    let ntfs_partitions: Vec<Partition> = report
        .partitions
        .iter()
        .filter(|partition| partition.ntfs)
        .cloned()
        .collect();
    let mut remaining_members = limits.max_members;
    for partition in &ntfs_partitions {
        let volume_index = report.volumes.len() as u32;
        let mut window = Window::new(&mut disk, partition.start, partition.length);
        match volume::inventory_volume(
            &mut window,
            volume_index,
            partition,
            limits,
            &mut remaining_members,
            &mut sink,
        ) {
            Ok(volume) => {
                report.member_count += volume.member_count;
                if volume.outcome == Outcome::Partial {
                    if volume.partial_reasons.is_empty() {
                        report.mark_partial(format!(
                            "partition {} inventory is incomplete",
                            partition.index
                        ));
                    } else {
                        for reason in &volume.partial_reasons {
                            report.mark_partial(format!("partition {}: {reason}", partition.index));
                        }
                    }
                }
                report.volumes.push(volume);
            }
            Err(error) => {
                let reason = format!(
                    "partition {} carries an NTFS signature but did not open: {error}",
                    partition.index
                );
                report
                    .failed_volumes
                    .push((partition.index, error.to_string()));
                report.mark_partial(reason);
            }
        }
    }
    finish_report(&mut report, &disk, started);
    Ok(report)
}

fn finish_report<R: Read + Seek>(report: &mut ImageReport, disk: &VhdxDisk<R>, started: Instant) {
    report.bytes_read = disk.bytes_read();
    report.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
}

/// Convenience wrapper for callers that want every member in memory.
/// Allocation is bounded by [`DiskImageLimits::max_members`].
pub fn inventory_all<R: Read + Seek>(
    reader: R,
    len: u64,
    limits: &DiskImageLimits,
) -> Result<(ImageReport, Vec<Member>), DiskImageError> {
    let mut members = Vec::new();
    let report = inventory_reader(reader, len, limits, |member| members.push(member))?;
    Ok((report, members))
}

/// A partition byte range presented as a standalone stream to the NTFS
/// parser. Reads cannot escape the declared partition.
struct Window<'a, R> {
    inner: &'a mut R,
    start: u64,
    len: u64,
    pos: u64,
}

impl<'a, R: Read + Seek> Window<'a, R> {
    fn new(inner: &'a mut R, start: u64, len: u64) -> Self {
        Self {
            inner,
            start,
            len,
            pos: 0,
        }
    }
}

impl<R: Read + Seek> Read for Window<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.len.saturating_sub(self.pos);
        if remaining == 0 || buf.is_empty() {
            return Ok(0);
        }
        let wanted = buf.len().min(remaining.try_into().unwrap_or(usize::MAX));
        let absolute = self.start.checked_add(self.pos).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "partition offset overflow",
            )
        })?;
        self.inner.seek(SeekFrom::Start(absolute))?;
        let read = self.inner.read(&mut buf[..wanted])?;
        self.pos += read as u64;
        Ok(read)
    }
}

impl<R: Read + Seek> Seek for Window<'_, R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(offset) => self.len.checked_add_signed(offset),
            SeekFrom::Current(offset) => self.pos.checked_add_signed(offset),
        };
        self.pos = target.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek outside the partition window",
            )
        })?;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{build_vhdx, gpt_disk, PartSpec, VhdxSpec};
    use std::io::Cursor;

    fn inventory_bytes(
        image: Vec<u8>,
        limits: &DiskImageLimits,
    ) -> Result<(ImageReport, Vec<Member>), DiskImageError> {
        let len = image.len() as u64;
        inventory_all(Cursor::new(image), len, limits)
    }

    fn image_of(disk: Vec<u8>) -> Vec<u8> {
        build_vhdx(&VhdxSpec {
            virtual_size: disk.len() as u64,
            payload: disk,
            ..VhdxSpec::default()
        })
    }

    #[test]
    fn a_differencing_image_stops_at_needs_parent() {
        let image = build_vhdx(&VhdxSpec {
            differencing: true,
            parent_path: Some(".\\basin.vhdx".into()),
            log_guid: [7; 16],
            ..VhdxSpec::default()
        });
        let (report, members) = inventory_bytes(image, &DiskImageLimits::default()).unwrap();
        assert_eq!(report.outcome, Outcome::NeedsParent);
        assert_eq!(report.image.payload, PayloadKind::Differencing);
        assert_eq!(
            report.image.parent.as_ref().unwrap().get("relative_path"),
            Some(".\\basin.vhdx")
        );
        assert!(report.partitions.is_empty());
        assert!(report.volumes.is_empty());
        assert!(members.is_empty());
        assert!(report
            .partial_reasons
            .iter()
            .any(|reason| reason.contains("log replay")));
    }

    #[test]
    fn an_image_with_no_ntfs_partition_yields_no_members() {
        let disk = gpt_disk(4096, 512, &[PartSpec::basic(128, 512, "spillway", false)]);
        let (report, members) =
            inventory_bytes(image_of(disk), &DiskImageLimits::default()).unwrap();
        assert_eq!(report.outcome, Outcome::Complete);
        assert_eq!(report.partitions.len(), 1);
        assert!(report.volumes.is_empty() && members.is_empty());
    }

    #[test]
    fn incomplete_container_and_partition_states_are_explained() {
        let logged = build_vhdx(&VhdxSpec {
            log_guid: [7; 16],
            ..VhdxSpec::default()
        });
        let (report, _) = inventory_bytes(logged, &DiskImageLimits::default()).unwrap();
        assert_eq!(report.outcome, Outcome::Partial);
        assert!(report.partitions.is_empty());
        assert!(report.volumes.is_empty());
        assert!(report
            .partial_reasons
            .iter()
            .any(|r| r.contains("log replay")));

        let disk = gpt_disk(4096, 512, &[PartSpec::basic(128, 512, "spillway", false)]);
        let limits = DiskImageLimits {
            max_partitions: 0,
            ..DiskImageLimits::default()
        };
        let (report, _) = inventory_bytes(image_of(disk), &limits).unwrap();
        assert_eq!(report.outcome, Outcome::Partial);
        assert!(report
            .partial_reasons
            .iter()
            .any(|r| r.contains("entry budget")));
    }

    #[test]
    fn one_unreadable_ntfs_volume_does_not_sink_the_image() {
        let disk = gpt_disk(
            4096,
            512,
            &[
                PartSpec::basic(128, 512, "corpus", true),
                PartSpec::basic(1024, 512, "spillway", false),
            ],
        );
        let (report, members) =
            inventory_bytes(image_of(disk), &DiskImageLimits::default()).unwrap();
        assert_eq!(report.outcome, Outcome::Partial);
        assert_eq!(report.failed_volumes.len(), 1);
        assert_eq!(report.failed_volumes[0].0, 0);
        assert!(report.volumes.is_empty() && members.is_empty());
        assert!(report
            .partial_reasons
            .iter()
            .any(|r| r.contains("did not open")));
    }

    #[test]
    fn a_window_cannot_read_outside_its_partition() {
        let mut disk = Cursor::new((0..=255u8).collect::<Vec<u8>>());
        let mut window = Window::new(&mut disk, 16, 8);
        let mut got = [0u8; 32];
        assert_eq!(window.read(&mut got).unwrap(), 8);
        assert_eq!(&got[..8], &[16, 17, 18, 19, 20, 21, 22, 23]);
        assert_eq!(window.read(&mut got).unwrap(), 0);
        assert_eq!(window.seek(SeekFrom::End(0)).unwrap(), 8);
        assert_eq!(window.seek(SeekFrom::Start(4)).unwrap(), 4);
        assert_eq!(window.read(&mut got).unwrap(), 4);
        assert_eq!(&got[..4], &[20, 21, 22, 23]);
        assert!(window.seek(SeekFrom::Current(-16)).is_err());
    }
}
