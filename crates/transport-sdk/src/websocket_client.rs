use anyhow::{Context, Result, anyhow, bail};
use common::{ClusterId, NodeId};
use reqwest::Url;
use rustls::RootCertStore;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{OtherError, SignatureScheme};
use std::io;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use x509_parser::prelude::FromDer;

const WEBSOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_SERVER_CERTIFICATE_DER_BYTES: usize = 64 * 1024;
const MAX_SERVER_CERTIFICATE_DER_NESTING: usize = 64;
const SERVER_CERTIFICATE_PARSER_STACK_BYTES: usize = 2 * 1024 * 1024;

pub trait AsyncIo: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T> AsyncIo for T where T: AsyncRead + AsyncWrite + Send + Unpin {}

/// Stable BerryKeep identity expected from a directly-connected server node.
///
/// A direct URL is only a locator and may therefore change when a node moves,
/// receives a new address, or is reached through NAT port forwarding.  When
/// this value is supplied, TLS validates the issuing CA and these immutable
/// URI SAN identities instead of the URL's host name or IP address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpectedNodeServerIdentity {
    pub node_id: NodeId,
    pub cluster_id: ClusterId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootTrustScope {
    ConfiguredCa,
    PlatformAndConfiguredCa,
}

pub async fn connect_websocket(
    url: &Url,
    server_ca_pem: Option<&str>,
    client_identity_pem: Option<&[u8]>,
    headers: &[(String, String)],
) -> Result<WebSocketStream<Box<dyn AsyncIo>>> {
    connect_websocket_with_expected_server_identity(
        url,
        server_ca_pem,
        client_identity_pem,
        None,
        headers,
    )
    .await
}

pub async fn connect_websocket_with_expected_server_identity(
    url: &Url,
    server_ca_pem: Option<&str>,
    client_identity_pem: Option<&[u8]>,
    expected_server_identity: Option<ExpectedNodeServerIdentity>,
    headers: &[(String, String)],
) -> Result<WebSocketStream<Box<dyn AsyncIo>>> {
    connect_websocket_with_root_trust(
        url,
        server_ca_pem,
        client_identity_pem,
        expected_server_identity,
        headers,
        RootTrustScope::ConfiguredCa,
    )
    .await
}

pub(crate) async fn connect_rendezvous_websocket(
    url: &Url,
    server_ca_pem: Option<&str>,
    client_identity_pem: Option<&[u8]>,
) -> Result<WebSocketStream<Box<dyn AsyncIo>>> {
    connect_websocket_with_root_trust(
        url,
        server_ca_pem,
        client_identity_pem,
        None,
        &[],
        RootTrustScope::PlatformAndConfiguredCa,
    )
    .await
}

async fn connect_websocket_with_root_trust(
    url: &Url,
    server_ca_pem: Option<&str>,
    client_identity_pem: Option<&[u8]>,
    expected_server_identity: Option<ExpectedNodeServerIdentity>,
    headers: &[(String, String)],
    root_trust_scope: RootTrustScope,
) -> Result<WebSocketStream<Box<dyn AsyncIo>>> {
    let stream = open_websocket_io(
        url,
        server_ca_pem,
        client_identity_pem,
        expected_server_identity,
        root_trust_scope,
    )
    .await?;
    let mut request = url
        .as_str()
        .into_client_request()
        .context("failed building websocket client request")?;
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("invalid websocket request header name {name}"))?;
        let value = HeaderValue::from_str(value)
            .with_context(|| format!("invalid websocket request header value for {name}"))?;
        request.headers_mut().insert(name, value);
    }
    let (websocket, _response) = timeout(WEBSOCKET_CONNECT_TIMEOUT, client_async(request, stream))
        .await
        .context("websocket handshake timed out")?
        .context("websocket handshake failed")?;
    Ok(websocket)
}

pub fn websocket_url(base_url: &str, path: &str) -> Result<Url> {
    let mut url = Url::parse(base_url.trim())
        .with_context(|| format!("invalid websocket base URL {base_url}"))?;
    let scheme = match url.scheme() {
        "http" => "ws",
        "https" => "wss",
        "ws" => "ws",
        "wss" => "wss",
        other => bail!("unsupported websocket base URL scheme {other}"),
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow!("failed setting websocket URL scheme"))?;
    url.join(path.trim_start_matches('/'))
        .with_context(|| format!("failed building websocket URL from {base_url} and {path}"))
}

async fn open_websocket_io(
    url: &Url,
    server_ca_pem: Option<&str>,
    client_identity_pem: Option<&[u8]>,
    expected_server_identity: Option<ExpectedNodeServerIdentity>,
    root_trust_scope: RootTrustScope,
) -> Result<Box<dyn AsyncIo>> {
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("websocket URL is missing a host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("websocket URL is missing a port"))?;
    let tcp = timeout(WEBSOCKET_CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .with_context(|| format!("timed out connecting websocket TCP stream to {host}:{port}"))?
        .with_context(|| format!("failed connecting websocket TCP stream to {host}:{port}"))?;

    match url.scheme() {
        "ws" => Ok(Box::new(tcp)),
        "wss" => {
            let server_name = ServerName::try_from(host.to_string())
                .context("failed building TLS server name")?;
            let tls_stream = timeout(
                WEBSOCKET_CONNECT_TIMEOUT,
                TlsConnector::from(std::sync::Arc::new(
                    build_tls_client_config_with_root_trust(
                        server_ca_pem,
                        client_identity_pem,
                        expected_server_identity,
                        root_trust_scope,
                    )?,
                ))
                .connect(server_name, tcp),
            )
            .await
            .with_context(|| {
                format!("timed out establishing TLS websocket stream to {host}:{port}")
            })?
            .with_context(|| {
                format!("failed establishing TLS websocket stream to {host}:{port}")
            })?;
            Ok(Box::new(tls_stream))
        }
        other => bail!("unsupported websocket URL scheme {other}"),
    }
}

/// Builds direct-connection TLS configuration. A supplied CA defines the trust scope,
/// even when the caller does not provide a stable node identity.
pub fn build_tls_client_config(
    server_ca_pem: Option<&str>,
    client_identity_pem: Option<&[u8]>,
    expected_server_identity: Option<ExpectedNodeServerIdentity>,
) -> Result<rustls::ClientConfig> {
    build_tls_client_config_with_root_trust(
        server_ca_pem,
        client_identity_pem,
        expected_server_identity,
        RootTrustScope::ConfiguredCa,
    )
}

pub(crate) fn build_rendezvous_tls_client_config(
    server_ca_pem: Option<&str>,
    client_identity_pem: Option<&[u8]>,
) -> Result<rustls::ClientConfig> {
    build_tls_client_config_with_root_trust(
        server_ca_pem,
        client_identity_pem,
        None,
        RootTrustScope::PlatformAndConfiguredCa,
    )
}

fn build_tls_client_config_with_root_trust(
    server_ca_pem: Option<&str>,
    client_identity_pem: Option<&[u8]>,
    expected_server_identity: Option<ExpectedNodeServerIdentity>,
    root_trust_scope: RootTrustScope,
) -> Result<rustls::ClientConfig> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let roots = websocket_root_certificates(
        server_ca_pem,
        root_trust_scope,
        rustls_native_certs::load_native_certs,
    )?;

    let config = match expected_server_identity {
        Some(expected) => {
            let verifier = ExpectedNodeServerCertVerifier::new(Arc::new(roots.clone()), expected)?;
            let builder = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier));
            match client_identity_pem {
                Some(identity_pem) => {
                    let (cert_chain, key) = parse_client_identity_pem(identity_pem)?;
                    builder
                        .with_client_auth_cert(cert_chain, key)
                        .context("failed building identity-bound websocket TLS client")?
                }
                None => builder.with_no_client_auth(),
            }
        }
        None => {
            let builder = rustls::ClientConfig::builder().with_root_certificates(roots);
            match client_identity_pem {
                Some(identity_pem) => {
                    let (cert_chain, key) = parse_client_identity_pem(identity_pem)?;
                    builder
                        .with_client_auth_cert(cert_chain, key)
                        .context("failed building websocket TLS client identity")?
                }
                None => builder.with_no_client_auth(),
            }
        }
    };

    Ok(config)
}

fn websocket_root_certificates(
    server_ca_pem: Option<&str>,
    root_trust_scope: RootTrustScope,
    load_native_certs: impl FnOnce() -> rustls_native_certs::CertificateResult,
) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    if let Some(server_ca_pem) = server_ca_pem {
        let mut reader = std::io::Cursor::new(server_ca_pem.as_bytes());
        for cert in CertificateDer::pem_reader_iter(&mut reader) {
            let cert = cert.context("failed parsing websocket server CA certificate")?;
            roots
                .add(cert)
                .context("failed adding websocket server CA certificate")?;
        }
        if roots.is_empty() {
            bail!("websocket server CA PEM contains no certificates");
        }
    }

    // Only rendezvous callers opt into mixing platform roots with their private CA.
    // Direct connections retain their configured CA scope, with or without a node identity.
    if server_ca_pem.is_none() || root_trust_scope == RootTrustScope::PlatformAndConfiguredCa {
        let native = load_native_certs();
        let _ = roots.add_parsable_certificates(native.certs);
        if roots.is_empty() {
            bail!("failed loading native root certificates for websocket TLS");
        }
    }
    Ok(roots)
}

fn expected_node_server_certificate_error(message: impl Into<String>) -> rustls::Error {
    rustls::Error::InvalidCertificate(rustls::CertificateError::Other(OtherError(Arc::new(
        io::Error::other(message.into()),
    ))))
}

fn validate_expected_node_server_identity(
    cert: &CertificateDer<'_>,
    expected: ExpectedNodeServerIdentity,
) -> Result<()> {
    let certificate_der = cert.as_ref();
    if certificate_der.len() > MAX_SERVER_CERTIFICATE_DER_BYTES {
        bail!(
            "server certificate exceeds the {} byte parser limit",
            MAX_SERVER_CERTIFICATE_DER_BYTES
        );
    }
    validate_der_nesting_depth(certificate_der)?;

    let certificate_der = certificate_der.to_vec();
    std::thread::Builder::new()
        .name("berrykeep-x509-parser".to_owned())
        .stack_size(SERVER_CERTIFICATE_PARSER_STACK_BYTES)
        .spawn(move || {
            validate_expected_node_server_identity_on_parser_stack(&certificate_der, expected)
        })
        .context("failed creating server certificate parser thread")?
        .join()
        .map_err(|_| anyhow!("server certificate parser thread panicked"))?
}

fn validate_expected_node_server_identity_on_parser_stack(
    certificate_der: &[u8],
    expected: ExpectedNodeServerIdentity,
) -> Result<()> {
    let (_, parsed) = x509_parser::certificate::X509Certificate::from_der(certificate_der)
        .context("failed parsing server certificate")?;
    let expected_node_uri = format!("urn:berrykeep:node:{}", expected.node_id);
    let legacy_expected_node_uri = format!("urn:ironmesh:node:{}", expected.node_id);
    let expected_cluster_uri = format!("urn:berrykeep:cluster:{}", expected.cluster_id);
    let legacy_expected_cluster_uri = format!("urn:ironmesh:cluster:{}", expected.cluster_id);
    let mut saw_node = false;
    let mut saw_cluster = false;

    for extension in parsed.extensions() {
        if let x509_parser::extensions::ParsedExtension::SubjectAlternativeName(san) =
            extension.parsed_extension()
        {
            for name in &san.general_names {
                if let x509_parser::extensions::GeneralName::URI(uri) = name {
                    saw_node |= *uri == expected_node_uri || *uri == legacy_expected_node_uri;
                    saw_cluster |=
                        *uri == expected_cluster_uri || *uri == legacy_expected_cluster_uri;
                }
            }
        }
    }

    if !saw_node {
        bail!("server certificate is missing expected node identity URI SAN {expected_node_uri}");
    }
    if !saw_cluster {
        bail!(
            "server certificate is missing expected cluster identity URI SAN {expected_cluster_uri}"
        );
    }

    Ok(())
}

fn validate_der_nesting_depth(certificate_der: &[u8]) -> Result<()> {
    if certificate_der.is_empty() {
        bail!("server certificate is empty");
    }

    let mut cursor = 0;
    let mut container_ends = vec![certificate_der.len()];

    loop {
        while container_ends.last().is_some_and(|end| *end == cursor) {
            container_ends.pop();
        }
        if container_ends.is_empty() {
            return if cursor == certificate_der.len() {
                Ok(())
            } else {
                bail!("server certificate DER has trailing data")
            };
        }

        let current_container_end = *container_ends
            .last()
            .expect("non-empty DER container stack");
        if cursor >= current_container_end {
            bail!("server certificate DER ends inside a container");
        }
        let tag = *certificate_der
            .get(cursor)
            .ok_or_else(|| anyhow!("server certificate DER ends in a tag"))?;
        cursor += 1;

        if tag & 0x1F == 0x1F {
            let mut tag_octet_count = 0;
            loop {
                let tag_octet = *certificate_der
                    .get(cursor)
                    .ok_or_else(|| anyhow!("server certificate DER ends in a tag identifier"))?;
                cursor += 1;
                tag_octet_count += 1;
                if tag_octet_count > 5 {
                    bail!("server certificate DER tag identifier is too long");
                }
                if tag_octet & 0x80 == 0 {
                    break;
                }
            }
        }

        let length_octet = *certificate_der
            .get(cursor)
            .ok_or_else(|| anyhow!("server certificate DER ends in a length"))?;
        cursor += 1;
        let content_length = if length_octet & 0x80 == 0 {
            usize::from(length_octet)
        } else {
            let length_octet_count = usize::from(length_octet & 0x7F);
            if length_octet_count == 0 {
                bail!("server certificate DER uses an indefinite length");
            }
            if length_octet_count > std::mem::size_of::<usize>() {
                bail!("server certificate DER length is too large");
            }

            let length_end = cursor
                .checked_add(length_octet_count)
                .ok_or_else(|| anyhow!("server certificate DER length overflows"))?;
            let length_bytes = certificate_der
                .get(cursor..length_end)
                .ok_or_else(|| anyhow!("server certificate DER ends in a long-form length"))?;
            cursor = length_end;
            length_bytes
                .iter()
                .fold(0usize, |length, byte| (length << 8) | usize::from(*byte))
        };
        let content_end = cursor
            .checked_add(content_length)
            .ok_or_else(|| anyhow!("server certificate DER content length overflows"))?;
        if content_end > current_container_end {
            bail!("server certificate DER element exceeds its container");
        }

        if tag & 0x20 != 0 {
            if container_ends.len() >= MAX_SERVER_CERTIFICATE_DER_NESTING {
                bail!(
                    "server certificate DER exceeds the {} level nesting limit",
                    MAX_SERVER_CERTIFICATE_DER_NESTING
                );
            }
            container_ends.push(content_end);
        } else {
            cursor = content_end;
        }
    }
}

#[derive(Debug)]
struct ExpectedNodeServerCertVerifier {
    roots: Arc<RootCertStore>,
    inner: Arc<WebPkiServerVerifier>,
    expected: ExpectedNodeServerIdentity,
}

impl ExpectedNodeServerCertVerifier {
    fn new(roots: Arc<RootCertStore>, expected: ExpectedNodeServerIdentity) -> Result<Self> {
        let inner = WebPkiServerVerifier::builder(roots.clone())
            .build()
            .context("failed building identity-bound server certificate verifier")?;
        Ok(Self {
            roots,
            inner,
            expected,
        })
    }
}

impl ServerCertVerifier for ExpectedNodeServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let cert = webpki::EndEntityCert::try_from(end_entity).map_err(|err| {
            expected_node_server_certificate_error(format!(
                "failed parsing identity-bound server certificate: {err}"
            ))
        })?;
        cert.verify_for_usage(
            webpki::ALL_VERIFICATION_ALGS,
            self.roots.roots.as_slice(),
            intermediates,
            now,
            webpki::KeyUsage::server_auth(),
            None,
            None,
        )
        .map_err(|err| {
            expected_node_server_certificate_error(format!(
                "identity-bound server certificate chain validation failed: {err}"
            ))
        })?;
        validate_expected_node_server_identity(end_entity, self.expected)
            .map_err(|err| expected_node_server_certificate_error(err.to_string()))?;

        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

fn parse_client_identity_pem(
    identity_pem: &[u8],
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let mut cert_reader = std::io::Cursor::new(identity_pem);
    let cert_chain = CertificateDer::pem_reader_iter(&mut cert_reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed parsing websocket client certificate chain")?;
    if cert_chain.is_empty() {
        bail!("websocket client identity PEM is missing a certificate chain");
    }

    let mut key_reader = std::io::Cursor::new(identity_pem);
    let key = PrivateKeyDer::from_pem_reader(&mut key_reader)
        .context("failed parsing websocket client private key")?;
    Ok((cert_chain, key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
        KeyUsagePurpose, SanType,
    };
    use std::time::Instant;

    #[test]
    fn rendezvous_websocket_trusts_native_and_custom_cas() {
        let (native_ca, _, native_server) = test_ca_and_server_certificate();
        let (_, custom_ca_pem, custom_server) = test_ca_and_server_certificate();
        let (_, _, untrusted_server) = test_ca_and_server_certificate();
        let roots = websocket_root_certificates(
            Some(&custom_ca_pem),
            RootTrustScope::PlatformAndConfiguredCa,
            || native_certificates(vec![native_ca]),
        )
        .unwrap();

        assert_server_trust(&roots, &native_server, "rendezvous.example", true);
        assert_server_trust(&roots, &custom_server, "rendezvous.example", true);
        assert_server_trust(&roots, &untrusted_server, "rendezvous.example", false);
        assert_server_trust(&roots, &native_server, "wrong-host.example", false);
    }

    #[test]
    fn configured_ca_scope_does_not_load_platform_roots() {
        let (_, custom_ca_pem, custom_server) = test_ca_and_server_certificate();
        let (_, _, unrelated_server) = test_ca_and_server_certificate();
        let roots =
            websocket_root_certificates(Some(&custom_ca_pem), RootTrustScope::ConfiguredCa, || {
                panic!("an explicit direct CA must not gain platform trust roots")
            })
            .unwrap();

        assert_server_trust(&roots, &custom_server, "rendezvous.example", true);
        assert_server_trust(&roots, &unrelated_server, "rendezvous.example", false);
    }

    #[test]
    fn direct_websocket_keeps_custom_ca_without_expected_identity() {
        let (native_ca, _, native_server) = test_ca_and_server_certificate();
        let (_, custom_ca_pem, custom_server) = test_ca_and_server_certificate();
        let roots =
            websocket_root_certificates(Some(&custom_ca_pem), RootTrustScope::ConfiguredCa, || {
                native_certificates(vec![native_ca])
            })
            .unwrap();

        assert_server_trust(&roots, &custom_server, "rendezvous.example", true);
        assert_server_trust(&roots, &native_server, "rendezvous.example", false);
    }

    #[test]
    fn websocket_trusts_native_ca_without_custom_ca() {
        let (native_ca, _, server) = test_ca_and_server_certificate();
        let roots = websocket_root_certificates(None, RootTrustScope::ConfiguredCa, || {
            native_certificates(vec![native_ca])
        })
        .unwrap();

        assert_server_trust(&roots, &server, "rendezvous.example", true);
    }

    #[test]
    fn websocket_keeps_custom_ca_when_native_loading_fails() {
        let (_, custom_ca_pem, server) = test_ca_and_server_certificate();
        let roots = websocket_root_certificates(
            Some(&custom_ca_pem),
            RootTrustScope::PlatformAndConfiguredCa,
            native_certificate_loading_failure,
        )
        .unwrap();

        assert_server_trust(&roots, &server, "rendezvous.example", true);
    }

    #[test]
    fn websocket_reports_native_loading_failure_without_custom_ca() {
        let error = websocket_root_certificates(
            None,
            RootTrustScope::ConfiguredCa,
            native_certificate_loading_failure,
        )
        .expect_err("an unavailable trust store must fail explicitly");

        assert!(
            error
                .to_string()
                .contains("failed loading native root certificates")
        );
    }

    #[test]
    fn websocket_rejects_invalid_custom_ca_even_with_native_roots() {
        let (native_ca, _, _) = test_ca_and_server_certificate();
        let result = websocket_root_certificates(
            Some("-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n"),
            RootTrustScope::PlatformAndConfiguredCa,
            || native_certificates(vec![native_ca]),
        );

        assert!(
            result.is_err(),
            "invalid explicit trust material must be rejected"
        );
    }

    #[test]
    fn websocket_rejects_custom_ca_pem_without_certificates() {
        for pem in ["", "not a certificate"] {
            let (native_ca, _, _) = test_ca_and_server_certificate();
            let error = websocket_root_certificates(
                Some(pem),
                RootTrustScope::PlatformAndConfiguredCa,
                || native_certificates(vec![native_ca]),
            )
            .expect_err("an invalid explicit CA must not silently fall back to platform roots");

            assert!(
                error
                    .to_string()
                    .contains("CA PEM contains no certificates")
            );
        }
    }

    #[test]
    fn websocket_uses_valid_native_roots_despite_unusable_entries() {
        let (native_ca, _, server) = test_ca_and_server_certificate();
        let roots = websocket_root_certificates(None, RootTrustScope::ConfiguredCa, || {
            native_certificates(vec![CertificateDer::from(vec![0, 1, 2]), native_ca])
        })
        .unwrap();

        assert_server_trust(&roots, &server, "rendezvous.example", true);
    }

    fn native_certificates(
        certs: Vec<CertificateDer<'static>>,
    ) -> rustls_native_certs::CertificateResult {
        let mut result = rustls_native_certs::CertificateResult::default();
        result.certs = certs;
        result
    }

    fn native_certificate_loading_failure() -> rustls_native_certs::CertificateResult {
        let mut result = native_certificates(Vec::new());
        result.errors.push(rustls_native_certs::Error {
            context: "test native certificate store unavailable",
            kind: rustls_native_certs::ErrorKind::Io {
                inner: io::Error::new(io::ErrorKind::NotFound, "test trust store absent"),
                path: "test-trust-store.pem".into(),
            },
        });
        result
    }

    fn test_ca_and_server_certificate() -> (CertificateDer<'static>, String, CertificateDer<'static>)
    {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = params.self_signed(&key).unwrap();
        let issuer = Issuer::from_params(&params, key);
        let mut server_params =
            CertificateParams::new(vec!["rendezvous.example".to_string()]).unwrap();
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server = server_params
            .signed_by(&KeyPair::generate().unwrap(), &issuer)
            .unwrap();
        (ca.der().clone(), ca.pem(), server.der().clone())
    }

    fn assert_server_trust(
        roots: &RootCertStore,
        server: &CertificateDer<'_>,
        hostname: &'static str,
        trusted: bool,
    ) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let verifier = WebPkiServerVerifier::builder(Arc::new(roots.clone()))
            .build()
            .unwrap();
        let result = verifier.verify_server_cert(
            server,
            &[],
            &ServerName::try_from(hostname).unwrap(),
            &[],
            UnixTime::now(),
        );
        assert!(
            result.is_ok() == trusted,
            "unexpected server trust for {hostname}"
        );
    }

    #[tokio::test]
    async fn connect_websocket_times_out_for_unreachable_host() {
        let url =
            Url::parse("ws://192.0.2.1:81/transport/ws").expect("test websocket URL should parse");

        let started_at = Instant::now();
        let error = match connect_websocket(&url, None, None, &[]).await {
            Ok(_) => panic!("unreachable websocket host should fail"),
            Err(error) => error,
        };

        assert!(
            started_at.elapsed() < Duration::from_secs(7),
            "websocket connect should fail within the timeout window"
        );
        assert!(
            error
                .to_string()
                .contains("timed out connecting websocket TCP stream")
                || error
                    .to_string()
                    .contains("failed connecting websocket TCP stream"),
            "unexpected websocket error: {error:#}"
        );
    }

    #[test]
    fn expected_node_identity_accepts_a_certificate_with_a_different_locator_san() {
        let expected = ExpectedNodeServerIdentity {
            node_id: NodeId::new_v4(),
            cluster_id: ClusterId::new_v4(),
        };
        let cert_der = server_certificate_with_identity(expected);

        validate_expected_node_server_identity(&cert_der, expected)
            .expect("BerryKeep identity should not depend on the locator DNS SAN");
        assert!(
            validate_expected_node_server_identity(
                &cert_der,
                ExpectedNodeServerIdentity {
                    node_id: expected.node_id,
                    cluster_id: ClusterId::new_v4(),
                },
            )
            .is_err(),
            "a mismatched cluster identity must still be rejected"
        );
    }

    #[test]
    fn expected_node_identity_accepts_legacy_uri_sans() {
        let expected = ExpectedNodeServerIdentity {
            node_id: NodeId::new_v4(),
            cluster_id: ClusterId::new_v4(),
        };
        let cert_der = server_certificate_with_uri_namespace(expected, "ironmesh");

        validate_expected_node_server_identity(&cert_der, expected)
            .expect("legacy identity should remain accepted during migration");
    }

    #[test]
    fn expected_node_identity_check_does_not_use_the_callers_small_stack() {
        let expected = ExpectedNodeServerIdentity {
            node_id: NodeId::new_v4(),
            cluster_id: ClusterId::new_v4(),
        };
        let cert_der = server_certificate_with_identity(expected);

        std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(move || validate_expected_node_server_identity(&cert_der, expected))
            .expect("small-stack caller thread")
            .join()
            .expect("small-stack caller must not panic")
            .expect("certificate identity validation should succeed");
    }

    #[test]
    fn rejects_excessively_nested_server_certificate_der() {
        let mut der = vec![0x05, 0x00];
        for _ in 0..MAX_SERVER_CERTIFICATE_DER_NESTING {
            der = der_sequence(der);
        }

        let error = validate_der_nesting_depth(&der)
            .expect_err("excessive DER nesting must be rejected before certificate parsing");
        assert!(
            error.to_string().contains("nesting limit"),
            "unexpected DER nesting error: {error:#}"
        );
    }

    #[test]
    fn expected_node_server_verifier_checks_ca_and_urns_without_hostname_matching() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let expected = ExpectedNodeServerIdentity {
            node_id: NodeId::new_v4(),
            cluster_id: ClusterId::new_v4(),
        };

        let ca_key = KeyPair::generate().expect("CA key");
        let mut ca_params = CertificateParams::new(Vec::new()).expect("CA params");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca_cert = ca_params.self_signed(&ca_key).expect("CA certificate");
        let ca_pem = ca_cert.pem();
        let issuer = Issuer::from_params(&ca_params, ca_key);

        let mut server_params = CertificateParams::new(Vec::new()).expect("server params");
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        server_params.subject_alt_names = vec![
            SanType::DnsName("stale-locator.example".try_into().expect("DNS SAN")),
            SanType::URI(
                format!("urn:berrykeep:node:{}", expected.node_id)
                    .try_into()
                    .expect("node URI SAN"),
            ),
            SanType::URI(
                format!("urn:berrykeep:cluster:{}", expected.cluster_id)
                    .try_into()
                    .expect("cluster URI SAN"),
            ),
        ];
        let server_key = KeyPair::generate().expect("server key");
        let server_cert = server_params
            .signed_by(&server_key, &issuer)
            .expect("server certificate");
        let server_der =
            CertificateDer::from_pem_slice(server_cert.pem().as_bytes()).expect("server DER");
        let ca_der = CertificateDer::from_pem_slice(ca_pem.as_bytes()).expect("CA DER");
        let mut roots = RootCertStore::empty();
        roots.add(ca_der).expect("trusted CA");
        let verifier = ExpectedNodeServerCertVerifier::new(Arc::new(roots), expected)
            .expect("identity verifier");
        let changed_locator =
            ServerName::try_from("changed-locator.example".to_string()).expect("server name");

        verifier
            .verify_server_cert(&server_der, &[], &changed_locator, &[], UnixTime::now())
            .expect("CA-signed server certificate should be accepted by its stable BerryKeep ID");

        let wrong_cluster_verifier = ExpectedNodeServerCertVerifier::new(
            Arc::clone(&verifier.roots),
            ExpectedNodeServerIdentity {
                node_id: expected.node_id,
                cluster_id: ClusterId::new_v4(),
            },
        )
        .expect("mismatched identity verifier");
        assert!(
            wrong_cluster_verifier
                .verify_server_cert(&server_der, &[], &changed_locator, &[], UnixTime::now())
                .is_err(),
            "the verifier must reject a certificate from a different cluster"
        );
    }

    fn der_sequence(contents: Vec<u8>) -> Vec<u8> {
        let mut der = vec![0x30];
        match contents.len() {
            0..=127 => der.push(contents.len() as u8),
            128..=255 => der.extend([0x81, contents.len() as u8]),
            256..=65_535 => der.extend([0x82, (contents.len() >> 8) as u8, contents.len() as u8]),
            _ => panic!("test DER sequence is too large"),
        }
        der.extend(contents);
        der
    }

    fn server_certificate_with_identity(
        expected: ExpectedNodeServerIdentity,
    ) -> CertificateDer<'static> {
        server_certificate_with_uri_namespace(expected, "berrykeep")
    }

    fn server_certificate_with_uri_namespace(
        expected: ExpectedNodeServerIdentity,
        namespace: &str,
    ) -> CertificateDer<'static> {
        let mut params = CertificateParams::new(Vec::new()).expect("certificate params");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.subject_alt_names = vec![
            SanType::DnsName("old-address.example".try_into().expect("DNS SAN")),
            SanType::URI(
                format!("urn:{namespace}:node:{}", expected.node_id)
                    .try_into()
                    .expect("node URI SAN"),
            ),
            SanType::URI(
                format!("urn:{namespace}:cluster:{}", expected.cluster_id)
                    .try_into()
                    .expect("cluster URI SAN"),
            ),
        ];
        let key = KeyPair::generate().expect("certificate key");
        let cert = params.self_signed(&key).expect("self-signed certificate");
        CertificateDer::from_pem_slice(cert.pem().as_bytes()).expect("certificate DER")
    }
}
