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

pub trait AsyncIo: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T> AsyncIo for T where T: AsyncRead + AsyncWrite + Send + Unpin {}

/// Stable IronMesh identity expected from a directly-connected server node.
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
    let stream = open_websocket_io(
        url,
        server_ca_pem,
        client_identity_pem,
        expected_server_identity,
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
                TlsConnector::from(std::sync::Arc::new(build_tls_client_config(
                    server_ca_pem,
                    client_identity_pem,
                    expected_server_identity,
                )?))
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

pub fn build_tls_client_config(
    server_ca_pem: Option<&str>,
    client_identity_pem: Option<&[u8]>,
    expected_server_identity: Option<ExpectedNodeServerIdentity>,
) -> Result<rustls::ClientConfig> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut roots = RootCertStore::empty();
    if let Some(server_ca_pem) = server_ca_pem {
        let mut reader = std::io::Cursor::new(server_ca_pem.as_bytes());
        for cert in CertificateDer::pem_reader_iter(&mut reader) {
            let cert = cert.context("failed parsing websocket server CA certificate")?;
            roots
                .add(cert)
                .context("failed adding websocket server CA certificate")?;
        }
    } else {
        let native = rustls_native_certs::load_native_certs();
        for cert in native.certs {
            roots
                .add(cert)
                .context("failed adding native websocket root certificate")?;
        }
        if !native.errors.is_empty() && roots.is_empty() {
            bail!("failed loading native root certificates for websocket TLS");
        }
    }

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

fn expected_node_server_certificate_error(message: impl Into<String>) -> rustls::Error {
    rustls::Error::InvalidCertificate(rustls::CertificateError::Other(OtherError(Arc::new(
        io::Error::other(message.into()),
    ))))
}

fn validate_expected_node_server_identity(
    cert: &CertificateDer<'_>,
    expected: ExpectedNodeServerIdentity,
) -> Result<()> {
    let (_, parsed) = x509_parser::certificate::X509Certificate::from_der(cert.as_ref())
        .context("failed parsing server certificate")?;
    let expected_node_uri = format!("urn:ironmesh:node:{}", expected.node_id);
    let expected_cluster_uri = format!("urn:ironmesh:cluster:{}", expected.cluster_id);
    let mut saw_node = false;
    let mut saw_cluster = false;

    for extension in parsed.extensions() {
        if let x509_parser::extensions::ParsedExtension::SubjectAlternativeName(san) =
            extension.parsed_extension()
        {
            for name in &san.general_names {
                if let x509_parser::extensions::GeneralName::URI(uri) = name {
                    saw_node |= *uri == expected_node_uri;
                    saw_cluster |= *uri == expected_cluster_uri;
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
        let mut params = CertificateParams::new(Vec::new()).expect("certificate params");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.subject_alt_names = vec![
            SanType::DnsName("old-address.example".try_into().expect("DNS SAN")),
            SanType::URI(
                format!("urn:ironmesh:node:{}", expected.node_id)
                    .try_into()
                    .expect("node URI SAN"),
            ),
            SanType::URI(
                format!("urn:ironmesh:cluster:{}", expected.cluster_id)
                    .try_into()
                    .expect("cluster URI SAN"),
            ),
        ];
        let key = KeyPair::generate().expect("certificate key");
        let cert = params.self_signed(&key).expect("self-signed certificate");
        let cert_der =
            CertificateDer::from_pem_slice(cert.pem().as_bytes()).expect("certificate DER");

        validate_expected_node_server_identity(&cert_der, expected)
            .expect("IronMesh identity should not depend on the locator DNS SAN");
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
                format!("urn:ironmesh:node:{}", expected.node_id)
                    .try_into()
                    .expect("node URI SAN"),
            ),
            SanType::URI(
                format!("urn:ironmesh:cluster:{}", expected.cluster_id)
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
            .expect("CA-signed server certificate should be accepted by its stable IronMesh ID");

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
}
