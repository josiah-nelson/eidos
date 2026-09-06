//! Read-only backing-device discovery. Call only on a single-flight background
//! worker: filesystem/driver calls can block indefinitely, even on local paths.
//! OS disk numbers do not prove physical independence behind RAID/virtual disks.

use std::path::Path;

pub fn probe_root(root: &Path) -> Result<Vec<String>, String> {
    platform::probe_root(root)
}

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub fn probe_root(_root: &Path) -> Result<Vec<String>, String> {
        // In particular, an APFS volume ID/st_dev is not a physical-store ID.
        Err("backing-device discovery is unavailable on this platform; using the shared fallback budget".into())
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};
    use std::path::{Component, Prefix};
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::{
            CreateFileW, GetDriveTypeW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, OPEN_EXISTING,
        },
        System::{
            Ioctl::{DISK_EXTENT, VOLUME_DISK_EXTENTS},
            IO::DeviceIoControl,
        },
    };

    const MAX_EXTENTS: usize = 64;
    const EXTENTS_OFFSET: usize = std::mem::offset_of!(VOLUME_DISK_EXTENTS, Extents);
    const EXTENT_SIZE: usize = std::mem::size_of::<DISK_EXTENT>();
    const MAX_BYTES: usize = EXTENTS_OFFSET + MAX_EXTENTS * EXTENT_SIZE;

    // Reject relative/NT/device namespaces rather than accepting the boot-volume
    // fallback GetVolumePathNameW documents for invalid volume qualifiers. UNC
    // roots return without a filesystem or network syscall.
    fn local_drive(root: &Path) -> Result<[u16; 4], String> {
        if !root.has_root() {
            return Err("backing-device lookup requires an absolute local path".into());
        }
        let Some(Component::Prefix(prefix)) = root.components().next() else {
            return Err("backing-device lookup requires a local drive qualifier".into());
        };
        match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                Ok([u16::from(letter), b':' as u16, b'\\' as u16, 0])
            }
            Prefix::UNC(..) | Prefix::VerbatimUNC(..) => {
                Err("network storage has no supported local backing-device identity".into())
            }
            _ => Err("unsupported path namespace for backing-device lookup".into()),
        }
    }

    fn nul_terminated(root: &Path) -> Result<Vec<u16>, String> {
        let mut wide: Vec<_> = root.as_os_str().encode_wide().collect();
        if wide.len() > 32_766 || wide.contains(&0) {
            return Err("path exceeds the device probe bound or contains NUL".into());
        }
        wide.push(0);
        Ok(wide)
    }

    fn os_error(operation: &str) -> String {
        format!("{operation}: {}", std::io::Error::last_os_error())
    }

    pub fn probe_root(root: &Path) -> Result<Vec<String>, String> {
        let drive = local_drive(root)?;
        let wide = nul_terminated(root)?;
        // DRIVE_FIXED / DRIVE_REMOVABLE only. In particular, mapped network
        // drives stop here before a potentially remote volume-path lookup.
        let kind = unsafe { GetDriveTypeW(drive.as_ptr()) };
        if kind != 2 && kind != 3 {
            return Err("root is not on an available local fixed/removable drive".into());
        }
        let mut mount = vec![0u16; 32_768];
        // SAFETY: all input strings are NUL-terminated, outputs are writable,
        // and every buffer size is supplied in UTF-16 code units.
        if unsafe { GetVolumePathNameW(wide.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) } == 0
        {
            return Err(os_error("resolve source mount point"));
        }
        let mut volume = [0u16; 64];
        if unsafe {
            GetVolumeNameForVolumeMountPointW(
                mount.as_ptr(),
                volume.as_mut_ptr(),
                volume.len() as u32,
            )
        } == 0
        {
            return Err(os_error("resolve source volume"));
        }
        let end = volume
            .iter()
            .position(|c| *c == 0)
            .ok_or("volume path was not terminated")?;
        if end == 0 || volume[end - 1] != b'\\' as u16 {
            return Err("volume path did not end at a mount root".into());
        }
        // A volume handle (not its root directory) omits the trailing slash.
        volume[end - 1] = 0;
        // SAFETY: validated volume path; access 0 requests metadata only. The
        // handle is never used for writes, locking, dismounting or raw reads.
        let handle = unsafe {
            CreateFileW(
                volume.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(os_error("open volume metadata handle"));
        }
        // SAFETY: successful CreateFileW transfers one owned handle. Drop closes
        // it on every return; no process-global handle or volume lock is held.
        let _handle = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) };
        // u64 storage provides native structure alignment. One fixed-size query
        // avoids unbounded allocation/retry if the driver reports more extents.
        let mut storage = [0u64; MAX_BYTES.div_ceil(8)];
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
                std::ptr::null(),
                0,
                storage.as_mut_ptr().cast(),
                MAX_BYTES as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            // ERROR_MORE_DATA is not a partial success: no guessed subset of
            // disks may establish independent admission capacity.
            return Err(os_error("read bounded volume disk extents"));
        }
        if returned as usize > MAX_BYTES {
            return Err("volume driver returned an invalid byte count".into());
        }
        // SAFETY: initialized, aligned storage; bound checked above, and the
        // parser reads bytes (never creates references to driver structures).
        let bytes =
            unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<u8>(), returned as usize) };
        parse_extents(bytes)
    }

    fn parse_extents(bytes: &[u8]) -> Result<Vec<String>, String> {
        let count = bytes
            .get(..4)
            .map(|v| u32::from_ne_bytes(v.try_into().unwrap()) as usize)
            .ok_or("truncated volume extent count")?;
        if !(1..=MAX_EXTENTS).contains(&count) || bytes.len() < EXTENTS_OFFSET + count * EXTENT_SIZE
        {
            return Err("volume extent count is empty, excessive or truncated".into());
        }
        let mut devices = std::collections::BTreeSet::new();
        for index in 0..count {
            let offset = EXTENTS_OFFSET + index * EXTENT_SIZE;
            let number_at = offset + std::mem::offset_of!(DISK_EXTENT, DiskNumber);
            let start_at = offset + std::mem::offset_of!(DISK_EXTENT, StartingOffset);
            let length_at = offset + std::mem::offset_of!(DISK_EXTENT, ExtentLength);
            let number = u32::from_ne_bytes(bytes[number_at..number_at + 4].try_into().unwrap());
            let start = i64::from_ne_bytes(bytes[start_at..start_at + 8].try_into().unwrap());
            let length = i64::from_ne_bytes(bytes[length_at..length_at + 8].try_into().unwrap());
            if start < 0 || length <= 0 || start.checked_add(length).is_none() {
                return Err("invalid volume disk extent range".into());
            }
            devices.insert(format!("windows-disk-{number}"));
        }
        Ok(devices.into_iter().collect())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn fixture(numbers: &[u32]) -> Vec<u8> {
            let mut bytes = vec![0; EXTENTS_OFFSET + numbers.len() * EXTENT_SIZE];
            bytes[..4].copy_from_slice(&(numbers.len() as u32).to_ne_bytes());
            for (i, number) in numbers.iter().enumerate() {
                let offset = EXTENTS_OFFSET + i * EXTENT_SIZE;
                let number_at = offset + std::mem::offset_of!(DISK_EXTENT, DiskNumber);
                let length_at = offset + std::mem::offset_of!(DISK_EXTENT, ExtentLength);
                bytes[number_at..number_at + 4].copy_from_slice(&number.to_ne_bytes());
                bytes[length_at..length_at + 8].copy_from_slice(&4096i64.to_ne_bytes());
            }
            bytes
        }

        #[test]
        fn all_backing_disks_are_deduplicated_without_dropping_overlap() {
            assert_eq!(
                parse_extents(&fixture(&[7, 2, 7])).unwrap(),
                vec!["windows-disk-2", "windows-disk-7"]
            );
        }

        #[test]
        fn partial_excessive_and_invalid_driver_results_stay_unknown() {
            let bytes = fixture(&[1, 2]);
            for len in 0..bytes.len() {
                assert!(
                    parse_extents(&bytes[..len]).is_err(),
                    "accepted {len} bytes"
                );
            }
            assert!(parse_extents(&fixture(&[])).is_err());
            assert!(parse_extents(&fixture(&[1; MAX_EXTENTS + 1])).is_err());
            let mut bytes = fixture(&[1]);
            let length_at = EXTENTS_OFFSET + std::mem::offset_of!(DISK_EXTENT, ExtentLength);
            bytes[length_at..length_at + 8].copy_from_slice(&(-1i64).to_ne_bytes());
            assert!(parse_extents(&bytes).is_err());
            bytes[length_at..length_at + 8].copy_from_slice(&i64::MAX.to_ne_bytes());
            let start_at = EXTENTS_OFFSET + std::mem::offset_of!(DISK_EXTENT, StartingOffset);
            bytes[start_at..start_at + 8].copy_from_slice(&1i64.to_ne_bytes());
            assert!(parse_extents(&bytes).is_err());
        }

        #[test]
        fn network_relative_and_device_namespaces_do_not_reach_the_os_probe() {
            for root in [
                r"\\fileserver\share\fixture",
                r"\\?\UNC\fileserver\share\fixture",
                r"\\.\PhysicalDrive0",
                r"\Device\HarddiskVolume1",
                r"C:relative",
                r"relative",
            ] {
                assert!(local_drive(Path::new(root)).is_err(), "accepted {root}");
                assert!(probe_root(Path::new(root)).is_err());
            }
            assert!(local_drive(Path::new(r"C:\fixture")).is_ok());
            assert!(local_drive(Path::new(r"\\?\C:\fixture")).is_ok());
            assert!(nul_terminated(Path::new("C:\\fixture\0hidden")).is_err());
        }

        #[test]
        #[ignore = "bounded OS topology smoke; requires a supported local temp volume"]
        fn two_temporary_roots_report_the_same_backing_disks() {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "eidos-device-topology-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir(&root).unwrap();
            struct Cleanup(std::path::PathBuf);
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    // Only these newly created empty directories, never a
                    // recursive removal or a pre-existing source tree.
                    let _ = std::fs::remove_dir(self.0.join("first"));
                    let _ = std::fs::remove_dir(self.0.join("second"));
                    let _ = std::fs::remove_dir(&self.0);
                }
            }
            let _cleanup = Cleanup(root.clone());
            let first = root.join("first");
            let second = root.join("second");
            std::fs::create_dir(&first).unwrap();
            std::fs::create_dir(&second).unwrap();
            let devices = probe_root(&first).unwrap();
            assert!(!devices.is_empty());
            assert_eq!(devices, probe_root(&second).unwrap());
        }
    }
}
