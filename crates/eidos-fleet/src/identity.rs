//! Node identity: a self-signed certificate whose fingerprint is the fleet
//! trust anchor. A node observes the master's certificate on first contact,
//! then pins it while its request waits for approval.
//!
//! There is no certificate authority. Each installation generates one
//! ECDSA P-256 key pair and a long-lived self-signed certificate the first
//! time the fleet needs it, and keeps both in `fleet/` under the data
//! directory so they survive upgrade, repair and reinstall-with-data. The
//! peer's certificate fingerprint (SHA-256 of the DER certificate) is what
//! the roster pins; the 16-byte node id is derived from it, so a different
//! key is a different node by construction.

use anyhow::{anyhow, Context};
use eidos_catalog::fleet::NodeId;
use eidos_domain::UnixNanos;
use fs4::fs_std::FileExt;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// DNS name placed in every certificate's subject alternative names. Pinning
/// makes the name irrelevant for trust; TLS still needs one to connect.
pub const TLS_SERVER_NAME: &str = "eidos-fleet";

const FLEET_DIR: &str = "fleet";
const CERT_FILE: &str = "node.crt";
const KEY_FILE: &str = "node.key";
const IDENTITY_FILE: &str = "identity.json";
const LOCK_FILE: &str = "identity.lock";

/// Certificate fingerprint: SHA-256 over the DER encoding.
pub fn fingerprint_of(cert_der: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, cert_der);
    digest.as_ref().try_into().expect("SHA-256 is 32 bytes")
}

/// The node id is the first sixteen bytes of a domain-separated hash of the
/// fingerprint, so it is stable for the key and cannot be chosen.
pub fn node_id_of(fingerprint: &[u8; 32]) -> NodeId {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    ctx.update(b"eidos-fleet-node-id/1");
    ctx.update(fingerprint);
    let digest = ctx.finish();
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest.as_ref()[..16]);
    NodeId(id)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.trim();
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IdentityRecord {
    name: String,
    created_at: UnixNanos,
}

/// This installation's fleet identity.
#[derive(Clone)]
pub struct NodeIdentity {
    pub node_id: NodeId,
    pub name: String,
    pub fingerprint: [u8; 32],
    pub created_at: UnixNanos,
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
}

impl std::fmt::Debug for NodeIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The private key never reaches a log line.
        f.debug_struct("NodeIdentity")
            .field("node_id", &self.node_id)
            .field("name", &self.name)
            .field("fingerprint", &hex(&self.fingerprint))
            .finish()
    }
}

impl NodeIdentity {
    pub fn fleet_dir(data_dir: &Path) -> PathBuf {
        data_dir.join(FLEET_DIR)
    }

    /// Whether an identity has been generated under `data_dir`.
    pub fn exists(data_dir: &Path) -> bool {
        let dir = Self::fleet_dir(data_dir);
        dir.join(CERT_FILE).is_file()
            && dir.join(KEY_FILE).is_file()
            && dir.join(IDENTITY_FILE).is_file()
    }

    /// Load the identity, generating one on first use. `name` is used only
    /// when generating; a stored identity keeps the name it was created
    /// with until [`NodeIdentity::rename`].
    pub fn load_or_create(data_dir: &Path, name: &str) -> anyhow::Result<Self> {
        let dir = Self::fleet_dir(data_dir);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        restrict_dir(&dir)?;
        let _lock = lock_identity_dir(&dir)?;
        remove_private_staging_file(&dir.join(format!("{KEY_FILE}.tmp")))?;
        if Self::exists(data_dir) {
            return Self::load(&dir);
        }
        let certified = rcgen::generate_simple_self_signed(vec![TLS_SERVER_NAME.to_string()])
            .map_err(|e| anyhow!("generating the node certificate: {e}"))?;
        let cert_der = certified.cert.der().to_vec();
        let key_der = certified.signing_key.serialize_der();
        let record = IdentityRecord {
            name: name.to_string(),
            created_at: UnixNanos::now(),
        };
        // Publish the record last. A crash before then leaves an incomplete
        // identity that the next lock holder replaces as one unit.
        let key_tmp = dir.join(format!("{KEY_FILE}.tmp"));
        let cert_tmp = dir.join(format!("{CERT_FILE}.tmp"));
        let record_tmp = dir.join(format!("{IDENTITY_FILE}.tmp"));
        remove_if_present(&key_tmp)?;
        remove_if_present(&cert_tmp)?;
        remove_if_present(&record_tmp)?;
        write_private(&key_tmp, &key_der)?;
        write_synced(&cert_tmp, &cert_der)?;
        write_synced(&record_tmp, &serde_json::to_vec_pretty(&record)?)?;
        replace_file(&key_tmp, &dir.join(KEY_FILE))?;
        restrict_private_file(&dir.join(KEY_FILE))?;
        replace_file(&cert_tmp, &dir.join(CERT_FILE))?;
        replace_file(&record_tmp, &dir.join(IDENTITY_FILE))?;
        Self::load(&dir)
    }

    fn load(dir: &Path) -> anyhow::Result<Self> {
        // Re-assert the boundary on every load. An administrator may have
        // copied/restored the directory with inherited permissions after it
        // was created; the authentication key must never be read until the
        // directory is private again.
        restrict_dir(dir)?;
        restrict_private_file(&dir.join(KEY_FILE))?;
        let cert_der = std::fs::read(dir.join(CERT_FILE))
            .with_context(|| format!("reading {}", dir.join(CERT_FILE).display()))?;
        let key_der = std::fs::read(dir.join(KEY_FILE))
            .with_context(|| format!("reading {}", dir.join(KEY_FILE).display()))?;
        let record_bytes = std::fs::read(dir.join(IDENTITY_FILE))
            .with_context(|| format!("reading {}", dir.join(IDENTITY_FILE).display()))?;
        let record: IdentityRecord = serde_json::from_slice(&record_bytes)
            .with_context(|| format!("parsing {}", dir.join(IDENTITY_FILE).display()))?;
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(cert_der.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der.clone())),
            )
            .context("fleet identity certificate and private key do not match")?;
        let fingerprint = fingerprint_of(&cert_der);
        Ok(Self {
            node_id: node_id_of(&fingerprint),
            name: record.name,
            fingerprint,
            created_at: record.created_at,
            cert_der,
            key_der,
        })
    }

    pub fn rename(&mut self, data_dir: &Path, name: &str) -> anyhow::Result<()> {
        let dir = Self::fleet_dir(data_dir);
        let _lock = lock_identity_dir(&dir)?;
        let record = IdentityRecord {
            name: name.to_string(),
            created_at: self.created_at,
        };
        let tmp = dir.join(format!("{IDENTITY_FILE}.tmp"));
        remove_if_present(&tmp)?;
        write_synced(&tmp, &serde_json::to_vec_pretty(&record)?)?;
        replace_file(&tmp, &dir.join(IDENTITY_FILE))?;
        self.name = name.to_string();
        Ok(())
    }

    pub fn certificate(&self) -> CertificateDer<'static> {
        CertificateDer::from(self.cert_der.clone())
    }

    pub fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone()))
    }

    pub fn fingerprint_hex(&self) -> String {
        hex(&self.fingerprint)
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        write_synced(path, bytes)?;
        Ok(())
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn remove_if_present(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Remove a private-key staging file left by an interrupted publication.
/// A restored file can carry its own permissive ACL, so repair that ACL before
/// removing the name; this also closes the load-time exposure reported for an
/// otherwise complete identity.
fn remove_private_staging_file(path: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            restrict_private_file(path)?;
            remove_if_present(path)
        }
        Ok(_) => remove_if_present(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("inspecting {}", path.display())),
    }
}

fn replace_file(from: &Path, to: &Path) -> anyhow::Result<()> {
    remove_if_present(to)?;
    std::fs::rename(from, to)
        .with_context(|| format!("publishing {} as {}", from.display(), to.display()))
}

fn lock_identity_dir(dir: &Path) -> anyhow::Result<std::fs::File> {
    let lock_path = dir.join(LOCK_FILE);
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    lock.lock_exclusive()
        .with_context(|| format!("locking {}", lock_path.display()))?;
    Ok(lock)
}

/// Keep the key directory to SYSTEM, Administrators and the account that
/// runs the service. The data directory's default ACL (ProgramData) lets
/// every local user read it, which must not extend to a private key.
/// Failure is fatal: starting fleet sync with an inherited ProgramData ACL
/// would expose the private credential used to impersonate this node.
fn restrict_dir(dir: &Path) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        restrict_windows_acl(dir, true)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting {} to mode 0700", dir.display()))
    }
}

/// Reassert the private-key ACL independently of its parent. A restored key
/// can carry an explicit permissive ACE that changing the directory does not
/// replace, and it must be repaired before the first byte is read.
fn restrict_private_file(path: &Path) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        restrict_windows_acl(path, false)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting {} to mode 0600", path.display()))
    }
}

#[cfg(windows)]
fn restrict_windows_acl(path: &Path, directory: bool) -> anyhow::Result<()> {
    let me = std::process::Command::new("whoami")
        .output()
        .context("finding the account that owns the fleet identity")?;
    if !me.status.success() {
        return Err(anyhow!(
            "whoami failed while restricting {}: {}",
            path.display(),
            String::from_utf8_lossy(&me.stderr).trim()
        ));
    }
    let me = String::from_utf8(me.stdout)
        .context("whoami returned a non-UTF-8 account name")?
        .trim()
        .to_string();
    if me.is_empty() {
        return Err(anyhow!("whoami returned an empty account name"));
    }

    // `/inheritance:r` alone preserves explicit ACEs. Reset first so a
    // restored `Everyone:R` (or a named-user grant) cannot survive, then
    // remove the inherited ProgramData ACL and install the complete allowlist.
    run_icacls(path, &["/reset".into()])?;
    run_icacls(path, &["/inheritance:r".into()])?;
    let full = if directory { "(OI)(CI)F" } else { "F" };
    run_icacls(
        path,
        &[
            "/grant:r".into(),
            format!("*S-1-5-18:{full}"),
            "/grant:r".into(),
            format!("*S-1-5-32-544:{full}"),
            "/grant:r".into(),
            format!("{me}:{full}"),
        ],
    )
}

#[cfg(windows)]
fn run_icacls(path: &Path, args: &[String]) -> anyhow::Result<()> {
    let output = std::process::Command::new("icacls")
        .arg(path)
        .args(args)
        .output()
        .with_context(|| format!("running icacls to restrict {}", path.display()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "icacls could not restrict {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_generated_once_and_reloaded_with_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let a = NodeIdentity::load_or_create(dir.path(), "alpha").unwrap();
        let b = NodeIdentity::load_or_create(dir.path(), "ignored").unwrap();
        assert_eq!(a.node_id, b.node_id);
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_eq!(b.name, "alpha");
        assert_eq!(a.node_id, node_id_of(&a.fingerprint));
        let other =
            NodeIdentity::load_or_create(tempfile::tempdir().unwrap().path(), "beta").unwrap();
        assert_ne!(a.node_id, other.node_id);
        assert!(!format!("{a:?}").contains("key"));
    }

    #[test]
    fn concurrent_creation_publishes_one_key_pair() {
        let dir = tempfile::tempdir().unwrap();
        let path = std::sync::Arc::new(dir.path().to_path_buf());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    NodeIdentity::load_or_create(&path, &format!("node-{i}"))
                        .unwrap()
                        .fingerprint
                })
            })
            .collect();
        let fingerprints: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert!(fingerprints
            .iter()
            .all(|fingerprint| *fingerprint == fingerprints[0]));
    }

    #[test]
    fn completed_identity_removes_interrupted_private_key_staging_file() {
        let dir = tempfile::tempdir().unwrap();
        let identity = NodeIdentity::load_or_create(dir.path(), "alpha").unwrap();
        let fleet_dir = NodeIdentity::fleet_dir(dir.path());
        let staged_key = fleet_dir.join(format!("{KEY_FILE}.tmp"));
        std::fs::copy(fleet_dir.join(KEY_FILE), &staged_key).unwrap();

        let loaded = NodeIdentity::load_or_create(dir.path(), "ignored").unwrap();

        assert_eq!(loaded.fingerprint, identity.fingerprint);
        assert!(!staged_key.exists());
    }

    #[test]
    fn a_mismatched_certificate_and_key_are_refused() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        NodeIdentity::load_or_create(first.path(), "first").unwrap();
        NodeIdentity::load_or_create(second.path(), "second").unwrap();
        std::fs::copy(
            NodeIdentity::fleet_dir(second.path()).join(KEY_FILE),
            NodeIdentity::fleet_dir(first.path()).join(KEY_FILE),
        )
        .unwrap();
        let error = NodeIdentity::load_or_create(first.path(), "ignored")
            .unwrap_err()
            .to_string();
        assert!(error.contains("do not match"), "{error}");
    }
}
