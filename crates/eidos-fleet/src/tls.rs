//! Mutual TLS with pinned self-signed certificates.
//!
//! Neither side trusts a certificate authority. A dialing peer pins the
//! fingerprint it expects (from the roster, or from an invitation) and the
//! handshake fails on any other certificate. An accepting peer requires a
//! client certificate and lets the handshake complete for any well-formed
//! one, because the *session* layer decides what the certificate may do:
//! a fingerprint in the roster may sync, an unknown one may only enroll.
//! That admission runs before any payload is processed.

use crate::identity::{fingerprint_of, NodeIdentity, TLS_SERVER_NAME};
use anyhow::{anyhow, Context};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{ring, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme,
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};

pub type ClientStream = tokio_rustls::client::TlsStream<TcpStream>;
pub type ServerStream = tokio_rustls::server::TlsStream<TcpStream>;

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

/// Accepts exactly one certificate: the pinned one.
#[derive(Debug)]
struct Pinned {
    fingerprint: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if fingerprint_of(end_entity.as_ref()) == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "peer certificate does not match the pinned fingerprint".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Requires a client certificate and proves possession of its key; who the
/// key belongs to is decided afterwards from the roster.
#[derive(Debug)]
struct AnyClient {
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for AnyClient {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }

    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }
}

pub fn server_config(identity: &NodeIdentity) -> anyhow::Result<Arc<ServerConfig>> {
    let provider = provider();
    let config = ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("TLS 1.3 unavailable")?
        .with_client_cert_verifier(Arc::new(AnyClient { provider }))
        .with_single_cert(vec![identity.certificate()], identity.private_key())
        .context("loading the node certificate")?;
    Ok(Arc::new(config))
}

pub fn client_config(
    identity: &NodeIdentity,
    pinned: [u8; 32],
) -> anyhow::Result<Arc<ClientConfig>> {
    let provider = provider();
    let config = ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("TLS 1.3 unavailable")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned {
            fingerprint: pinned,
            provider,
        }))
        .with_client_auth_cert(vec![identity.certificate()], identity.private_key())
        .context("loading the node certificate")?;
    Ok(Arc::new(config))
}

/// Fingerprint of the certificate the peer presented.
pub fn peer_fingerprint(conn: &rustls::CommonState) -> Option<[u8; 32]> {
    conn.peer_certificates()
        .and_then(|certs| certs.first())
        .map(|cert| fingerprint_of(cert.as_ref()))
}

/// Dial `endpoint`, pinning `pinned`, within `timeout`.
pub async fn connect(
    identity: &NodeIdentity,
    endpoint: &str,
    pinned: [u8; 32],
    timeout: Duration,
) -> anyhow::Result<(ClientStream, SocketAddr)> {
    let config = client_config(identity, pinned)?;
    let connector = TlsConnector::from(config);
    let tcp = tokio::time::timeout(timeout, TcpStream::connect(endpoint))
        .await
        .map_err(|_| anyhow!("connecting to {endpoint} timed out"))?
        .with_context(|| format!("connecting to {endpoint}"))?;
    tcp.set_nodelay(true).ok();
    let addr = tcp.peer_addr()?;
    let name = ServerName::try_from(TLS_SERVER_NAME.to_string()).expect("static name");
    let stream = tokio::time::timeout(timeout, connector.connect(name, tcp))
        .await
        .map_err(|_| anyhow!("TLS handshake with {endpoint} timed out"))?
        .with_context(|| format!("TLS handshake with {endpoint}"))?;
    Ok((stream, addr))
}

/// Complete the server side of a handshake on an accepted socket.
pub async fn accept(
    config: Arc<ServerConfig>,
    tcp: TcpStream,
    timeout: Duration,
) -> anyhow::Result<(ServerStream, [u8; 32])> {
    tcp.set_nodelay(true).ok();
    let acceptor = TlsAcceptor::from(config);
    let stream = tokio::time::timeout(timeout, acceptor.accept(tcp))
        .await
        .map_err(|_| anyhow!("TLS handshake timed out"))?
        .context("TLS handshake")?;
    let fingerprint = peer_fingerprint(stream.get_ref().1)
        .ok_or_else(|| anyhow!("peer presented no certificate"))?;
    Ok((stream, fingerprint))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn identity(name: &str) -> (tempfile::TempDir, NodeIdentity) {
        let dir = tempfile::tempdir().unwrap();
        let id = NodeIdentity::load_or_create(dir.path(), name).unwrap();
        (dir, id)
    }

    #[tokio::test]
    async fn pinned_handshake_succeeds_and_exposes_the_client_fingerprint() {
        let (_a, server) = identity("server");
        let (_b, client) = identity("client");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = server_config(&server).unwrap();
        let accept_task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let (mut stream, fp) = accept(config, tcp, Duration::from_secs(5)).await.unwrap();
            let mut buf = [0u8; 5];
            stream.read_exact(&mut buf).await.unwrap();
            (fp, buf)
        });
        let (mut stream, _) = connect(
            &client,
            &addr.to_string(),
            server.fingerprint,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(
            peer_fingerprint(stream.get_ref().1),
            Some(server.fingerprint)
        );
        stream.write_all(b"hello").await.unwrap();
        stream.flush().await.unwrap();
        let (fp, buf) = accept_task.await.unwrap();
        assert_eq!(fp, client.fingerprint);
        assert_eq!(&buf, b"hello");
    }

    #[tokio::test]
    async fn a_wrong_pin_fails_the_handshake() {
        let (_a, server) = identity("server");
        let (_b, client) = identity("client");
        let (_c, impostor) = identity("impostor");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = server_config(&server).unwrap();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let _ = accept(config, tcp, Duration::from_secs(5)).await;
        });
        let err = connect(
            &client,
            &addr.to_string(),
            impostor.fingerprint,
            Duration::from_secs(5),
        )
        .await
        .expect_err("handshake must fail");
        assert!(format!("{err:#}").contains("handshake"), "{err:#}");
    }
}
