//! Node identity: a self-signed certificate whose fingerprint is the trust
//! anchor, and the invitation code that carries a central's fingerprint to
//! a node about to enroll.
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
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// DNS name placed in every certificate's subject alternative names. Pinning
/// makes the name irrelevant for trust; TLS still needs one to connect.
pub const TLS_SERVER_NAME: &str = "eidos-fleet";

const FLEET_DIR: &str = "fleet";
const CERT_FILE: &str = "node.crt";
const KEY_FILE: &str = "node.key";
const IDENTITY_FILE: &str = "identity.json";

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
        dir.join(CERT_FILE).is_file() && dir.join(KEY_FILE).is_file()
    }

    /// Load the identity, generating one on first use. `name` is used only
    /// when generating; a stored identity keeps the name it was created
    /// with until [`NodeIdentity::rename`].
    pub fn load_or_create(data_dir: &Path, name: &str) -> anyhow::Result<Self> {
        let dir = Self::fleet_dir(data_dir);
        if Self::exists(data_dir) {
            return Self::load(&dir);
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        restrict_dir(&dir);
        let certified = rcgen::generate_simple_self_signed(vec![TLS_SERVER_NAME.to_string()])
            .map_err(|e| anyhow!("generating the node certificate: {e}"))?;
        let cert_der = certified.cert.der().to_vec();
        let key_der = certified.signing_key.serialize_der();
        let record = IdentityRecord {
            name: name.to_string(),
            created_at: UnixNanos::now(),
        };
        // Key first, then certificate, then the record: a crash between them
        // leaves a directory `exists` says is incomplete and regenerates.
        write_private(&dir.join(KEY_FILE), &key_der)?;
        std::fs::write(dir.join(CERT_FILE), &cert_der)?;
        std::fs::write(dir.join(IDENTITY_FILE), serde_json::to_vec_pretty(&record)?)?;
        Self::load(&dir)
    }

    fn load(dir: &Path) -> anyhow::Result<Self> {
        let cert_der = std::fs::read(dir.join(CERT_FILE))
            .with_context(|| format!("reading {}", dir.join(CERT_FILE).display()))?;
        let key_der = std::fs::read(dir.join(KEY_FILE))
            .with_context(|| format!("reading {}", dir.join(KEY_FILE).display()))?;
        let record: IdentityRecord = std::fs::read(dir.join(IDENTITY_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_else(|| IdentityRecord {
                name: eidos_domain::bench::hostname(),
                created_at: UnixNanos::now(),
            });
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
        let record = IdentityRecord {
            name: name.to_string(),
            created_at: self.created_at,
        };
        std::fs::write(
            Self::fleet_dir(data_dir).join(IDENTITY_FILE),
            serde_json::to_vec_pretty(&record)?,
        )?;
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
        use std::io::Write;
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
        std::fs::write(path, bytes)?;
        Ok(())
    }
}

/// Keep the key directory to SYSTEM, Administrators and the account that
/// runs the service. The data directory's default ACL (ProgramData) lets
/// every local user read it, which must not extend to a private key.
/// Best effort: a failure is logged, not fatal, because the service must
/// still start on a host whose ACL tooling is unavailable.
fn restrict_dir(dir: &Path) {
    #[cfg(windows)]
    {
        let me = std::process::Command::new("whoami")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let mut args: Vec<String> = vec![
            dir.display().to_string(),
            "/inheritance:r".into(),
            "/grant:r".into(),
            "*S-1-5-18:(OI)(CI)F".into(),
            "/grant:r".into(),
            "*S-1-5-32-544:(OI)(CI)F".into(),
        ];
        if let Some(me) = me {
            args.push("/grant:r".into());
            args.push(format!("{me}:(OI)(CI)F"));
        }
        match std::process::Command::new("icacls").args(&args).output() {
            Ok(o) if o.status.success() => {}
            Ok(o) => tracing::warn!(
                dir = %dir.display(),
                stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                "could not restrict the fleet key directory"
            ),
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "icacls unavailable; fleet key directory keeps the inherited ACL")
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
            tracing::warn!(dir = %dir.display(), error = %e, "could not restrict the fleet key directory");
        }
    }
}

/// What a central hands an operator to enroll one node: its own
/// fingerprint (so the node can pin it before trusting anything it says),
/// where to reach it, and a single-use secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteCode {
    pub central_fingerprint: [u8; 32],
    pub secret: [u8; 32],
    pub endpoint: String,
}

const INVITE_PREFIX: &str = "eidos-fleet-v1";

impl InviteCode {
    pub fn generate(central_fingerprint: [u8; 32], endpoint: &str) -> anyhow::Result<Self> {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).map_err(|e| anyhow!("entropy unavailable: {e}"))?;
        Ok(Self {
            central_fingerprint,
            secret,
            endpoint: endpoint.to_string(),
        })
    }

    /// The value stored by the central: only ever the hash of the secret.
    pub fn token_hash(secret: &[u8; 32]) -> [u8; 32] {
        let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
        ctx.update(b"eidos-fleet-invite/1");
        ctx.update(secret);
        ctx.finish().as_ref().try_into().expect("32 bytes")
    }

    pub fn encode(&self) -> String {
        format!(
            "{INVITE_PREFIX}:{}:{}:{}",
            hex(&self.central_fingerprint),
            hex(&self.secret),
            self.endpoint
        )
    }

    pub fn parse(code: &str) -> anyhow::Result<Self> {
        let mut parts = code.trim().splitn(4, ':');
        let prefix = parts.next().unwrap_or_default();
        if prefix != INVITE_PREFIX {
            return Err(anyhow!(
                "not an eidos fleet invitation (expected `{INVITE_PREFIX}:...`)"
            ));
        }
        let central_fingerprint = parts
            .next()
            .and_then(unhex::<32>)
            .ok_or_else(|| anyhow!("invitation has a malformed central fingerprint"))?;
        let secret = parts
            .next()
            .and_then(unhex::<32>)
            .ok_or_else(|| anyhow!("invitation has a malformed secret"))?;
        let endpoint = parts
            .next()
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .ok_or_else(|| anyhow!("invitation names no central endpoint"))?
            .to_string();
        Ok(Self {
            central_fingerprint,
            secret,
            endpoint,
        })
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
    fn invite_codes_round_trip_and_reject_garbage() {
        let code = InviteCode::generate([7u8; 32], "192.0.2.10:7710").unwrap();
        let text = code.encode();
        assert!(text.starts_with("eidos-fleet-v1:"));
        assert_eq!(InviteCode::parse(&text).unwrap(), code);
        assert_eq!(InviteCode::parse(&format!("  {text}\n")).unwrap(), code);
        assert!(InviteCode::parse("eidos-fleet-v1:zz:yy:host").is_err());
        assert!(InviteCode::parse("hello").is_err());
        assert!(InviteCode::parse(&text[..text.len() - "192.0.2.10:7710".len()]).is_err());
        // The hash never equals the secret and is stable.
        assert_ne!(InviteCode::token_hash(&code.secret), code.secret);
        assert_eq!(
            InviteCode::token_hash(&code.secret),
            InviteCode::token_hash(&code.secret)
        );
    }
}
