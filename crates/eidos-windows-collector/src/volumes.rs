//! Local volume inventory: filesystem, drive and bus kind, media class,
//! capacity, feature flags, and USN journal shape. The inventory is diffed
//! every scan so mounts, unmounts, and journal recreation become records.

use eidos_observe::{
    bucket_capacity, bucket_size, BusKind, DriveKind, FilesystemKind, JournalShape, MediaKind,
    ObjectToken, PercentBucket, StudyKey, TimeAnchor, VolumeEvent, VolumeObservation,
};
use eidos_scanner::usn::{query_journal, JournalInfo, UsnError, VolumeHandle};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    BusTypeAta, BusTypeAtapi, BusTypeFileBackedVirtual, BusTypeNvme, BusTypeSas, BusTypeSata,
    BusTypeScsi, BusTypeSd, BusTypeUsb, BusTypeVirtual, CreateFileW, FindFirstVolumeW,
    FindNextVolumeW, FindVolumeClose, GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetDriveTypeW,
    GetVolumeInformationW, GetVolumePathNamesForVolumeNameW, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::{
    PropertyStandardQuery, StorageDeviceProperty, StorageDeviceSeekPenaltyProperty,
    DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, STORAGE_DEVICE_DESCRIPTOR,
    STORAGE_PROPERTY_QUERY,
};
use windows_sys::Win32::System::SystemServices::{
    FILE_SUPPORTS_HARD_LINKS, FILE_SUPPORTS_OPEN_BY_FILE_ID, FILE_SUPPORTS_REPARSE_POINTS,
    FILE_SUPPORTS_SPARSE_FILES, FILE_SUPPORTS_USN_JOURNAL, FILE_VOLUME_IS_COMPRESSED,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeFacts {
    /// `\\?\Volume{guid}\` — the stable local name.
    pub guid_path: String,
    /// `\\?\Volume{guid}` — the device path for volume handles.
    pub device: String,
    /// Drive roots and mount folders, e.g. `D:\`.
    pub mounts: Vec<String>,
    pub drive: DriveKind,
    pub filesystem: FilesystemKind,
    pub filesystem_name: String,
    pub flags: u32,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub bytes_per_cluster: u32,
    pub bus: BusKind,
    pub media: MediaKind,
    pub journal: Option<JournalInfo>,
    pub journal_error: Option<String>,
}

impl VolumeFacts {
    pub fn root(&self) -> &str {
        self.mounts
            .first()
            .map(String::as_str)
            .unwrap_or(&self.guid_path)
    }

    pub fn supports_usn(&self) -> bool {
        self.flags & FILE_SUPPORTS_USN_JOURNAL != 0
    }

    /// Volumes worth a change feed: local fixed or removable media with a
    /// journaling filesystem. Optical and RAM disks never qualify.
    pub fn is_feed_candidate(&self) -> bool {
        matches!(self.drive, DriveKind::Fixed | DriveKind::Removable)
            && matches!(self.filesystem, FilesystemKind::Ntfs | FilesystemKind::Refs)
            && self.supports_usn()
            // A volume whose journal is inactive is re-evaluated when the
            // inventory sees one; an unqueryable journal gets a reader so
            // the reason is recorded.
            && (self.journal.is_some() || self.journal_error.is_some())
    }

    pub fn matches_exclusion(&self, exclusion: &str) -> bool {
        let wanted = exclusion.trim_end_matches('\\').to_ascii_uppercase();
        if wanted.is_empty() {
            return false;
        }
        self.guid_path.trim_end_matches('\\').to_ascii_uppercase() == wanted
            || self
                .mounts
                .iter()
                .any(|mount| mount.trim_end_matches('\\').to_ascii_uppercase() == wanted)
    }

    pub fn token(&self, key: &StudyKey) -> ObjectToken {
        key.token("volume", self.guid_path.as_bytes())
    }

    pub fn observation(
        &self,
        key: &StudyKey,
        event: VolumeEvent,
        at: TimeAnchor,
    ) -> VolumeObservation {
        VolumeObservation {
            at,
            volume: self.token(key),
            event,
            filesystem: self.filesystem,
            drive: self.drive,
            bus: self.bus,
            media: self.media,
            capacity: bucket_capacity(self.total_bytes),
            free: PercentBucket::from_ratio(self.free_bytes, self.total_bytes),
            bytes_per_cluster: self.bytes_per_cluster,
            // NTFS/ReFS advertise FILE_CASE_SENSITIVE_SEARCH as a capability
            // even though names are case-insensitive by default; per-directory
            // case sensitivity is not a volume fact. Match the scanner.
            case_sensitive: match self.filesystem {
                FilesystemKind::Ntfs
                | FilesystemKind::Refs
                | FilesystemKind::Fat
                | FilesystemKind::Exfat => Some(false),
                _ => None,
            },
            supports_usn: self.supports_usn(),
            supports_file_ids: self.flags & FILE_SUPPORTS_OPEN_BY_FILE_ID != 0,
            supports_sparse: self.flags & FILE_SUPPORTS_SPARSE_FILES != 0,
            supports_reparse_points: self.flags & FILE_SUPPORTS_REPARSE_POINTS != 0,
            supports_hard_links: self.flags & FILE_SUPPORTS_HARD_LINKS != 0,
            compressed: self.flags & FILE_VOLUME_IS_COMPRESSED != 0,
            journal: self.journal.map(|journal| JournalShape {
                maximum_size: bucket_size(journal.maximum_size),
                allocation_delta: bucket_size(journal.allocation_delta),
                span: bucket_size(journal.next_usn.saturating_sub(journal.first_usn).max(0) as u64),
                max_major_version: journal.max_major_version,
            }),
        }
    }
}

/// Events implied by two consecutive inventories, keyed by volume GUID.
pub fn diff(previous: &[VolumeFacts], current: &[VolumeFacts]) -> Vec<(VolumeEvent, VolumeFacts)> {
    let mut events = Vec::new();
    for volume in current {
        match previous.iter().find(|p| p.guid_path == volume.guid_path) {
            None => events.push((VolumeEvent::Mounted, volume.clone())),
            Some(before) => {
                let event = match (before.journal, volume.journal) {
                    (None, Some(_)) => Some(VolumeEvent::JournalCreated),
                    // A journal that queried cleanly but is no longer active
                    // really is gone. A query that failed says nothing about
                    // the journal, so it must not be reported as a deletion.
                    (Some(_), None) if volume.journal_error.is_none() => {
                        Some(VolumeEvent::JournalDeleted)
                    }
                    (Some(a), Some(b)) if a.journal_id != b.journal_id => {
                        Some(VolumeEvent::JournalRecreated)
                    }
                    _ => None,
                };
                if let Some(event) = event {
                    events.push((event, volume.clone()));
                }
            }
        }
    }
    for volume in previous {
        if !current.iter().any(|c| c.guid_path == volume.guid_path) {
            events.push((VolumeEvent::Unmounted, volume.clone()));
        }
    }
    events
}

pub fn enumerate() -> Vec<VolumeFacts> {
    let mut volumes = Vec::new();
    let mut name = [0u16; 128];
    // SAFETY: buffer sized as declared; handle closed below.
    let find = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
    if find == INVALID_HANDLE_VALUE {
        return volumes;
    }
    loop {
        let guid_path =
            String::from_utf16_lossy(&name[..name.iter().position(|c| *c == 0).unwrap_or(0)]);
        if let Some(facts) = inspect(&guid_path) {
            volumes.push(facts);
        }
        // SAFETY: as above.
        if unsafe { FindNextVolumeW(find, name.as_mut_ptr(), name.len() as u32) } == 0 {
            break;
        }
    }
    // SAFETY: handle from FindFirstVolumeW.
    unsafe { FindVolumeClose(find) };
    volumes.sort_by(|a, b| a.root().cmp(b.root()));
    volumes
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn inspect(guid_path: &str) -> Option<VolumeFacts> {
    let root = wide(guid_path);
    // SAFETY: NUL-terminated path.
    let drive = match unsafe { GetDriveTypeW(root.as_ptr()) } {
        2 => DriveKind::Removable,
        3 => DriveKind::Fixed,
        4 => DriveKind::Remote,
        5 => DriveKind::Optical,
        6 => DriveKind::RamDisk,
        _ => DriveKind::Unknown,
    };
    let mounts = mount_points(&root);
    let mut serial = 0u32;
    let mut max_component = 0u32;
    let mut flags = 0u32;
    let mut fs_name = [0u16; 64];
    // SAFETY: buffers sized as declared. Fails for unformatted or offline
    // volumes (and optical drives without media), which are skipped.
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            &mut max_component,
            &mut flags,
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    };
    if ok == 0 {
        return None;
    }
    let filesystem_name =
        String::from_utf16_lossy(&fs_name[..fs_name.iter().position(|c| *c == 0).unwrap_or(0)]);
    let filesystem = match filesystem_name.to_ascii_uppercase().as_str() {
        "NTFS" => FilesystemKind::Ntfs,
        "REFS" => FilesystemKind::Refs,
        "FAT" | "FAT32" | "FAT16" => FilesystemKind::Fat,
        "EXFAT" => FilesystemKind::Exfat,
        "UDF" | "CDFS" => FilesystemKind::Udf,
        _ => FilesystemKind::Other,
    };
    let (mut free_available, mut total_bytes, mut free_bytes) = (0u64, 0u64, 0u64);
    // SAFETY: out-parameters only.
    unsafe {
        GetDiskFreeSpaceExW(
            root.as_ptr(),
            &mut free_available,
            &mut total_bytes,
            &mut free_bytes,
        )
    };
    let (mut sectors_per_cluster, mut bytes_per_sector, mut free_clusters, mut total_clusters) =
        (0u32, 0u32, 0u32, 0u32);
    // SAFETY: out-parameters only.
    unsafe {
        GetDiskFreeSpaceW(
            root.as_ptr(),
            &mut sectors_per_cluster,
            &mut bytes_per_sector,
            &mut free_clusters,
            &mut total_clusters,
        )
    };
    let device = guid_path.trim_end_matches('\\').to_string();
    let (bus, media) = storage_properties(&device);
    let (journal, journal_error) =
        if matches!(filesystem, FilesystemKind::Ntfs | FilesystemKind::Refs)
            && flags & FILE_SUPPORTS_USN_JOURNAL != 0
        {
            match VolumeHandle::open_device(&device, false)
                .and_then(|handle| query_journal(&handle))
            {
                Ok(journal) => (Some(journal), None),
                Err(UsnError::NotActive) => (None, None),
                Err(error) => (None, Some(error.to_string())),
            }
        } else {
            (None, None)
        };
    Some(VolumeFacts {
        guid_path: guid_path.to_string(),
        device,
        mounts,
        drive,
        filesystem,
        filesystem_name,
        flags,
        total_bytes,
        free_bytes,
        bytes_per_cluster: sectors_per_cluster.saturating_mul(bytes_per_sector),
        bus,
        media,
        journal,
        journal_error,
    })
}

fn mount_points(root: &[u16]) -> Vec<String> {
    let mut buffer = vec![0u16; 1024];
    let mut returned = 0u32;
    // SAFETY: buffer sized as declared; a too-small buffer only truncates.
    let ok = unsafe {
        GetVolumePathNamesForVolumeNameW(
            root.as_ptr(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            &mut returned,
        )
    };
    if ok == 0 {
        return Vec::new();
    }
    buffer[..(returned as usize).min(buffer.len())]
        .split(|c| *c == 0)
        .filter(|part| !part.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

fn storage_properties(device: &str) -> (BusKind, MediaKind) {
    let path = wide(device);
    // SAFETY: NUL-terminated path; zero access is enough for property queries.
    let handle: HANDLE = unsafe {
        CreateFileW(
            path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return (BusKind::Unknown, MediaKind::Unknown);
    }
    let bus = {
        let mut query = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceProperty,
            QueryType: PropertyStandardQuery,
            AdditionalParameters: [0],
        };
        let mut buffer = vec![0u8; 1024];
        let mut returned = 0u32;
        // SAFETY: the descriptor header fits the buffer; extra fields are ignored.
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_STORAGE_QUERY_PROPERTY,
                &mut query as *mut _ as *const _,
                std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
                buffer.as_mut_ptr() as *mut _,
                buffer.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok != 0 && returned as usize >= std::mem::size_of::<STORAGE_DEVICE_DESCRIPTOR>() {
            // SAFETY: the driver filled at least the fixed descriptor prefix.
            let descriptor = unsafe { &*(buffer.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR) };
            match descriptor.BusType {
                t if t == BusTypeNvme => BusKind::Nvme,
                t if t == BusTypeSata || t == BusTypeAta || t == BusTypeAtapi => BusKind::Sata,
                t if t == BusTypeSas => BusKind::Sas,
                t if t == BusTypeScsi => BusKind::Scsi,
                t if t == BusTypeUsb => BusKind::Usb,
                t if t == BusTypeSd => BusKind::Sd,
                t if t == BusTypeVirtual => BusKind::Virtual,
                t if t == BusTypeFileBackedVirtual => BusKind::FileBacked,
                _ => BusKind::Other,
            }
        } else {
            let _ = unsafe { GetLastError() };
            BusKind::Unknown
        }
    };
    let media = {
        let mut query = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceSeekPenaltyProperty,
            QueryType: PropertyStandardQuery,
            AdditionalParameters: [0],
        };
        let mut descriptor = DEVICE_SEEK_PENALTY_DESCRIPTOR {
            Version: 0,
            Size: 0,
            IncursSeekPenalty: false,
        };
        let mut returned = 0u32;
        // SAFETY: output buffer is exactly the documented descriptor.
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_STORAGE_QUERY_PROPERTY,
                &mut query as *mut _ as *const _,
                std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
                &mut descriptor as *mut _ as *mut _,
                std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok != 0 {
            if descriptor.IncursSeekPenalty {
                MediaKind::Rotational
            } else {
                MediaKind::Solid
            }
        } else {
            MediaKind::Unknown
        }
    };
    // SAFETY: handle from CreateFileW above.
    unsafe { CloseHandle(handle) };
    (bus, media)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(guid: &str, journal_id: Option<u64>) -> VolumeFacts {
        VolumeFacts {
            guid_path: format!(r"\\?\Volume{{{guid}}}\"),
            device: format!(r"\\?\Volume{{{guid}}}"),
            mounts: vec![r"Q:\".into()],
            drive: DriveKind::Fixed,
            filesystem: FilesystemKind::Ntfs,
            filesystem_name: "NTFS".into(),
            flags: FILE_SUPPORTS_USN_JOURNAL,
            total_bytes: 1 << 40,
            free_bytes: 1 << 39,
            bytes_per_cluster: 4096,
            bus: BusKind::Nvme,
            media: MediaKind::Solid,
            journal: journal_id.map(|journal_id| JournalInfo {
                journal_id,
                first_usn: 0,
                next_usn: 1 << 20,
                lowest_valid_usn: 0,
                max_usn: i64::MAX,
                maximum_size: 32 << 20,
                allocation_delta: 8 << 20,
                min_major_version: 2,
                max_major_version: 3,
            }),
            journal_error: None,
        }
    }

    #[test]
    fn inventory_diff_reports_mounts_and_journal_transitions() {
        let a = facts("a", Some(1));
        let b = facts("b", None);
        let events = diff(
            &[a.clone(), b.clone()],
            &[facts("a", Some(2)), facts("c", Some(1))],
        );
        let kinds: Vec<VolumeEvent> = events.iter().map(|(event, _)| *event).collect();
        assert_eq!(
            kinds,
            vec![
                VolumeEvent::JournalRecreated,
                VolumeEvent::Mounted,
                VolumeEvent::Unmounted
            ]
        );
        assert!(diff(std::slice::from_ref(&a), std::slice::from_ref(&a)).is_empty());
        assert_eq!(
            diff(&[b], &[facts("b", Some(5))])[0].0,
            VolumeEvent::JournalCreated
        );
        // A journal queried as inactive is a deletion; a journal whose query
        // failed is unknown and must stay silent.
        let deleted = facts("a", None);
        assert_eq!(
            diff(std::slice::from_ref(&a), &[deleted])[0].0,
            VolumeEvent::JournalDeleted
        );
        let unreadable = VolumeFacts {
            journal_error: Some("access denied".into()),
            ..facts("a", None)
        };
        assert!(diff(std::slice::from_ref(&a), &[unreadable]).is_empty());
        assert!(a.matches_exclusion("q:"));
        assert!(a.matches_exclusion(r"Q:\"));
        assert!(!a.matches_exclusion("r:"));
        assert!(a.is_feed_candidate());
    }

    #[test]
    fn observation_buckets_the_shape() {
        let key = StudyKey::from_bytes([2; 32]);
        let at = TimeAnchor {
            monotonic_ns: 1,
            utc_ns: 2,
        };
        let observation = facts("a", Some(1)).observation(&key, VolumeEvent::Inventory, at);
        assert_eq!(observation.capacity, eidos_observe::CapacityBucket::T4);
        assert_eq!(observation.free, PercentBucket::Under75);
        assert_eq!(
            observation.journal.unwrap().span,
            eidos_observe::SizeBucket::B4M
        );
        assert!(observation.supports_usn);
    }

    #[test]
    fn enumerates_this_host() {
        let volumes = enumerate();
        assert!(!volumes.is_empty());
        assert!(volumes.iter().any(|v| v.drive == DriveKind::Fixed));
    }
}
