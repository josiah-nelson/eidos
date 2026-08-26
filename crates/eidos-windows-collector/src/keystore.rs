//! The study key: 32 random bytes protected with DPAPI in machine scope so
//! the LocalSystem service and an elevated `eidos observe init` share it.
//! The key never leaves this file except into process memory; exports
//! carry only tokens derived from it.

use eidos_observe::StudyKey;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN,
    CRYPT_INTEGER_BLOB,
};

pub const KEY_FILE: &str = "study.key";
/// Application-bound entropy: keeps another machine-scope DPAPI caller from
/// decrypting the blob without also naming this purpose.
const ENTROPY: &[u8] = b"eidos-observe-study-key/1";

pub fn key_path(data_dir: &Path) -> PathBuf {
    data_dir.join(KEY_FILE)
}

pub fn exists(data_dir: &Path) -> bool {
    key_path(data_dir).exists()
}

/// Create (or with `force`, replace) the study key. `key` imports a
/// cohort-shared key so content fingerprints compare across hosts; `None`
/// generates a fresh random one. Returns `false` when a key already exists
/// and `force` is not set.
pub fn create(data_dir: &Path, key: Option<[u8; 32]>, force: bool) -> anyhow::Result<bool> {
    let path = key_path(data_dir);
    if path.exists() && !force {
        return Ok(false);
    }
    std::fs::create_dir_all(data_dir)?;
    let mut bytes = [0u8; 32];
    match key {
        Some(imported) => bytes = imported,
        None => getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?,
    }
    let blob = protect(&bytes)?;
    let temporary = path.with_extension("key.tmp");
    std::fs::write(&temporary, &blob)?;
    std::fs::rename(&temporary, &path)?;
    Ok(true)
}

pub fn load(data_dir: &Path) -> anyhow::Result<Option<StudyKey>> {
    let blob = match std::fs::read(key_path(data_dir)) {
        Ok(blob) => blob,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let plain = unprotect(&blob)?;
    let bytes: [u8; 32] = plain
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("study key has an unexpected length"))?;
    Ok(Some(StudyKey::from_bytes(bytes)))
}

fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    }
}

fn take_output(output: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    // SAFETY: DPAPI returned a LocalAlloc'd buffer of cbData bytes.
    let bytes =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    unsafe { LocalFree(output.pbData as *mut _) };
    bytes
}

fn protect(plain: &[u8]) -> anyhow::Result<Vec<u8>> {
    let description: Vec<u16> = "eidos study key\0".encode_utf16().collect();
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: input blobs point at live slices for the duration of the call.
    let ok = unsafe {
        CryptProtectData(
            &blob(plain),
            description.as_ptr(),
            &blob(ENTROPY),
            std::ptr::null_mut(),
            std::ptr::null(),
            CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        anyhow::bail!("CryptProtectData: {}", std::io::Error::last_os_error());
    }
    Ok(take_output(output))
}

fn unprotect(protected: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: input blobs point at live slices for the duration of the call.
    let ok = unsafe {
        CryptUnprotectData(
            &blob(protected),
            std::ptr::null_mut(),
            &blob(ENTROPY),
            std::ptr::null_mut(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        anyhow::bail!(
            "CryptUnprotectData: {} (the key was protected on another machine or under another scope)",
            std::io::Error::last_os_error()
        );
    }
    Ok(take_output(output))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_round_trips_through_dpapi_and_is_never_stored_plain() {
        let temp = tempfile::tempdir().unwrap();
        assert!(load(temp.path()).unwrap().is_none());
        assert!(create(temp.path(), Some([9; 32]), false).unwrap());
        assert!(!create(temp.path(), Some([1; 32]), false).unwrap());
        let stored = std::fs::read(key_path(temp.path())).unwrap();
        assert!(!stored.windows(32).any(|window| window == [9u8; 32]));
        let key = load(temp.path()).unwrap().unwrap();
        assert_eq!(
            key.token("object", b"x"),
            StudyKey::from_bytes([9; 32]).token("object", b"x")
        );
        assert!(create(temp.path(), None, true).unwrap());
        let replaced = load(temp.path()).unwrap().unwrap();
        assert_ne!(replaced.token("object", b"x"), key.token("object", b"x"));
    }
}
