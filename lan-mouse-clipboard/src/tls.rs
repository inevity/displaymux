use crate::{ClipboardHello, HostId};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName, Error as RustlsError,
    ServerConfig, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
    version::TLS13,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, RwLock},
};
use thiserror::Error;

const ALPN_PROTOCOL: &[u8] = b"lan-mouse-clipboard/1";

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct CertificateFingerprint([u8; 32]);

impl CertificateFingerprint {
    pub fn from_certificate(certificate: &[u8]) -> Self {
        Self(Sha256::digest(certificate).into())
    }

    pub fn parse(value: &str) -> Result<Self, TlsError> {
        let mut bytes = [0_u8; 32];
        let mut fields = value.split(':');
        for byte in &mut bytes {
            let field = fields.next().ok_or(TlsError::InvalidFingerprint)?;
            if field.len() != 2 {
                return Err(TlsError::InvalidFingerprint);
            }
            *byte = u8::from_str_radix(field, 16).map_err(|_| TlsError::InvalidFingerprint)?;
        }
        if fields.next().is_some() {
            return Err(TlsError::InvalidFingerprint);
        }
        Ok(Self(bytes))
    }
}

impl fmt::Debug for CertificateFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CertificateFingerprint(")?;
        for (index, byte) in self.0.iter().enumerate() {
            if index != 0 {
                formatter.write_str(":")?;
            }
            write!(formatter, "{byte:02x}")?;
        }
        formatter.write_str(")")
    }
}

impl fmt::Display for CertificateFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if index != 0 {
                formatter.write_str(":")?;
            }
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct TlsIdentity {
    certificates: Vec<CertificateDer<'static>>,
    private_key_pkcs8: Arc<[u8]>,
}

impl TlsIdentity {
    pub fn new(
        certificates: Vec<CertificateDer<'static>>,
        private_key_pkcs8: impl Into<Arc<[u8]>>,
    ) -> Result<Self, TlsError> {
        if certificates.is_empty() {
            return Err(TlsError::MissingCertificate);
        }
        let private_key_pkcs8 = private_key_pkcs8.into();
        if private_key_pkcs8.is_empty() {
            return Err(TlsError::MissingPrivateKey);
        }
        Ok(Self {
            certificates,
            private_key_pkcs8,
        })
    }

    pub fn certificates(&self) -> &[CertificateDer<'static>] {
        &self.certificates
    }

    pub fn fingerprint(&self) -> CertificateFingerprint {
        CertificateFingerprint::from_certificate(self.certificates[0].as_ref())
    }

    fn key(&self) -> PrivateKeyDer<'static> {
        PrivatePkcs8KeyDer::from(self.private_key_pkcs8.to_vec()).into()
    }
}

impl fmt::Debug for TlsIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TlsIdentity")
            .field("certificate_count", &self.certificates.len())
            .field("fingerprint", &self.fingerprint())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Default)]
pub struct AuthorizedPeers {
    inner: Arc<RwLock<HashMap<CertificateFingerprint, HostId>>>,
}

impl AuthorizedPeers {
    pub fn new(peers: impl IntoIterator<Item = (CertificateFingerprint, HostId)>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(peers.into_iter().collect())),
        }
    }

    pub fn replace(&self, peers: impl IntoIterator<Item = (CertificateFingerprint, HostId)>) {
        *self.inner.write().expect("authorized peer lock poisoned") = peers.into_iter().collect();
    }

    pub fn host_for(&self, fingerprint: CertificateFingerprint) -> Option<HostId> {
        self.inner
            .read()
            .expect("authorized peer lock poisoned")
            .get(&fingerprint)
            .cloned()
    }

    fn contains(&self, fingerprint: CertificateFingerprint) -> bool {
        self.inner
            .read()
            .expect("authorized peer lock poisoned")
            .contains_key(&fingerprint)
    }
}

impl fmt::Debug for AuthorizedPeers {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthorizedPeers")
            .field(
                "count",
                &self
                    .inner
                    .read()
                    .expect("authorized peer lock poisoned")
                    .len(),
            )
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthenticatedPeer {
    pub host_id: HostId,
    pub fingerprint: CertificateFingerprint,
}

#[derive(Debug, Error)]
pub enum TlsError {
    #[error("clipboard TLS identity has no certificate")]
    MissingCertificate,
    #[error("clipboard TLS identity has no private key")]
    MissingPrivateKey,
    #[error("invalid clipboard certificate fingerprint")]
    InvalidFingerprint,
    #[error("clipboard TLS peer did not present a certificate")]
    MissingPeerCertificate,
    #[error("clipboard TLS peer certificate is not authorized")]
    UnauthorizedPeer,
    #[error("clipboard hello host does not match authenticated certificate")]
    HostIdentityMismatch,
    #[error("clipboard TLS ALPN protocol was not negotiated")]
    ProtocolMismatch,
    #[error("clipboard TLS configuration failed: {0}")]
    Rustls(#[from] RustlsError),
}

pub fn server_config(
    identity: &TlsIdentity,
    authorized_peers: AuthorizedPeers,
) -> Result<Arc<ServerConfig>, TlsError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(FingerprintClientVerifier {
        authorized_peers,
        provider: provider.clone(),
        root_hints: Vec::new(),
    });
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(identity.certificates.clone(), identity.key())?;
    config.alpn_protocols = vec![ALPN_PROTOCOL.to_vec()];
    Ok(Arc::new(config))
}

pub fn client_config(
    identity: &TlsIdentity,
    expected_server: CertificateFingerprint,
) -> Result<Arc<ClientConfig>, TlsError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(FingerprintServerVerifier {
        expected: expected_server,
        provider: provider.clone(),
    });
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(identity.certificates.clone(), identity.key())?;
    config.alpn_protocols = vec![ALPN_PROTOCOL.to_vec()];
    config.resumption = rustls::client::Resumption::disabled();
    Ok(Arc::new(config))
}

pub fn clipboard_server_name() -> ServerName<'static> {
    ServerName::try_from("lan-mouse-clipboard.invalid").expect("static DNS name is valid")
}

pub fn authenticate_peer_certificates(
    certificates: Option<&[CertificateDer<'_>]>,
    authorized_peers: &AuthorizedPeers,
) -> Result<AuthenticatedPeer, TlsError> {
    let certificate = certificates
        .and_then(|certificates| certificates.first())
        .ok_or(TlsError::MissingPeerCertificate)?;
    let fingerprint = CertificateFingerprint::from_certificate(certificate.as_ref());
    let host_id = authorized_peers
        .host_for(fingerprint)
        .ok_or(TlsError::UnauthorizedPeer)?;
    Ok(AuthenticatedPeer {
        host_id,
        fingerprint,
    })
}

pub fn authenticate_hello(
    peer: &AuthenticatedPeer,
    hello: &ClipboardHello,
) -> Result<(), TlsError> {
    if peer.host_id == hello.host_id {
        Ok(())
    } else {
        Err(TlsError::HostIdentityMismatch)
    }
}

pub fn authenticate_alpn(protocol: Option<&[u8]>) -> Result<(), TlsError> {
    if protocol == Some(ALPN_PROTOCOL) {
        Ok(())
    } else {
        Err(TlsError::ProtocolMismatch)
    }
}

#[derive(Debug)]
struct FingerprintServerVerifier {
    expected: CertificateFingerprint,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for FingerprintServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        verify_fingerprint(end_entity, |fingerprint| fingerprint == self.expected)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls12_signature(
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
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls13_signature(
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

#[derive(Debug)]
struct FingerprintClientVerifier {
    authorized_peers: AuthorizedPeers,
    provider: Arc<CryptoProvider>,
    root_hints: Vec<DistinguishedName>,
}

impl ClientCertVerifier for FingerprintClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &self.root_hints
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, RustlsError> {
        verify_fingerprint(end_entity, |fingerprint| {
            self.authorized_peers.contains(fingerprint)
        })?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls12_signature(
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
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls13_signature(
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

fn verify_fingerprint(
    certificate: &CertificateDer<'_>,
    is_authorized: impl FnOnce(CertificateFingerprint) -> bool,
) -> Result<(), RustlsError> {
    if is_authorized(CertificateFingerprint::from_certificate(
        certificate.as_ref(),
    )) {
        Ok(())
    } else {
        Err(RustlsError::InvalidCertificate(
            CertificateError::ApplicationVerificationFailure,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CLIPBOARD_TEXT_V1, ClipboardHello, PROTOCOL_VERSION, WireMessage, encode_message,
        read_frame, write_frame,
    };
    use rcgen::CertifiedKey;
    use std::time::Duration;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_rustls::{TlsAcceptor, TlsConnector};
    use tokio_util::sync::CancellationToken;

    fn identity(name: &str) -> TlsIdentity {
        let CertifiedKey { cert, key_pair } =
            rcgen::generate_simple_self_signed([name.to_string()]).unwrap();
        TlsIdentity::new(
            vec![cert.der().to_owned()],
            Arc::<[u8]>::from(key_pair.serialize_der()),
        )
        .unwrap()
    }

    #[test]
    fn configured_fingerprint_format_round_trips() {
        let identity = identity("server");
        let fingerprint = identity.fingerprint();
        assert_eq!(
            CertificateFingerprint::parse(&fingerprint.to_string()).unwrap(),
            fingerprint
        );
        assert!(CertificateFingerprint::parse("00:11").is_err());
        assert!(CertificateFingerprint::parse(&format!("{}:00", fingerprint)).is_err());
    }

    #[tokio::test]
    async fn mutual_tls_13_binds_certificate_to_hello_host() {
        let server_identity = identity("server");
        let client_identity = identity("remote");
        let authorized =
            AuthorizedPeers::new([(client_identity.fingerprint(), HostId::from("remote"))]);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let acceptor =
            TlsAcceptor::from(server_config(&server_identity, authorized.clone()).unwrap());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(stream).await.unwrap();
            assert_eq!(
                stream.get_ref().1.protocol_version(),
                Some(rustls::ProtocolVersion::TLSv1_3)
            );
            authenticate_alpn(stream.get_ref().1.alpn_protocol()).unwrap();
            let peer =
                authenticate_peer_certificates(stream.get_ref().1.peer_certificates(), &authorized)
                    .unwrap();
            let message = read_frame(
                &mut stream,
                64,
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            let WireMessage::ClipboardHello(hello) = message else {
                panic!("client did not send hello")
            };
            authenticate_hello(&peer, &hello).unwrap();
            peer
        });

        let connector = TlsConnector::from(
            client_config(&client_identity, server_identity.fingerprint()).unwrap(),
        );
        let stream = TcpStream::connect(address).await.unwrap();
        let mut stream = connector
            .connect(clipboard_server_name(), stream)
            .await
            .unwrap();
        authenticate_alpn(stream.get_ref().1.alpn_protocol()).unwrap();
        let hello = WireMessage::ClipboardHello(ClipboardHello {
            host_id: HostId::from("remote"),
            process_session_id: crate::ProcessSessionId::new(22),
            offered_capabilities: CLIPBOARD_TEXT_V1,
            max_receive_bytes: 64,
        });
        let frame = encode_message(&hello, 64).unwrap();
        write_frame(
            &mut stream,
            &frame,
            Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let peer = server.await.unwrap();
        assert_eq!(peer.host_id, HostId::from("remote"));
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn authenticated_host_not_hello_text_controls_identity() {
        let peer = AuthenticatedPeer {
            host_id: HostId::from("remote-a"),
            fingerprint: identity("remote-a").fingerprint(),
        };
        let hello = ClipboardHello {
            host_id: HostId::from("remote-b"),
            process_session_id: crate::ProcessSessionId::new(22),
            offered_capabilities: CLIPBOARD_TEXT_V1,
            max_receive_bytes: 64,
        };
        assert!(matches!(
            authenticate_hello(&peer, &hello),
            Err(TlsError::HostIdentityMismatch)
        ));
        assert!(matches!(
            authenticate_alpn(None),
            Err(TlsError::ProtocolMismatch)
        ));
    }

    #[tokio::test]
    async fn unauthorized_client_certificate_fails_handshake() {
        let server_identity = identity("server");
        let rogue_identity = identity("rogue");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let acceptor =
            TlsAcceptor::from(server_config(&server_identity, AuthorizedPeers::default()).unwrap());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            acceptor.accept(stream).await
        });
        let connector = TlsConnector::from(
            client_config(&rogue_identity, server_identity.fingerprint()).unwrap(),
        );
        let stream = TcpStream::connect(address).await.unwrap();
        let client_result = connector.connect(clipboard_server_name(), stream).await;
        let server_result = server.await.unwrap();
        assert!(client_result.is_err() || server_result.is_err());
        assert!(server_result.is_err());
    }
}
