use std::{
    fs,
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType, string::Ia5String,
};
use uuid::Uuid;

pub(super) struct NodeTlsPaths {
    pub(super) ca_cert: PathBuf,
    pub(super) ca_key: PathBuf,
    pub(super) node_cert: PathBuf,
    pub(super) node_key: PathBuf,
}

pub(super) fn write_node_tls(
    root: &Path,
    cluster_id: Uuid,
    node_id: Uuid,
    node_ip: Ipv4Addr,
    rendezvous_ip: Ipv4Addr,
) -> Result<NodeTlsPaths> {
    fs::create_dir_all(root)
        .with_context(|| format!("failed creating TLS directory {}", root.display()))?;

    let ca_key = KeyPair::generate().context("failed generating test cluster CA key")?;
    let ca_key_pem = ca_key.serialize_pem();
    let ca_params = cluster_ca_params();
    let ca_cert = ca_params
        .self_signed(&ca_key)
        .context("failed creating test cluster CA certificate")?;
    let ca_pem = ca_cert.pem();
    let issuer = Issuer::new(ca_params, ca_key);

    let node_key = KeyPair::generate().context("failed generating test node key")?;
    let mut node_params = CertificateParams::default();
    node_params
        .distinguished_name
        .push(DnType::CommonName, format!("ironmesh-node-{node_id}"));
    node_params
        .subject_alt_names
        .push(SanType::IpAddress(IpAddr::V4(node_ip)));
    node_params
        .subject_alt_names
        .push(SanType::IpAddress(IpAddr::V4(rendezvous_ip)));
    node_params
        .subject_alt_names
        .push(SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    node_params.subject_alt_names.push(SanType::URI(
        Ia5String::try_from(format!("urn:ironmesh:node:{node_id}"))
            .context("node identity URI is invalid")?,
    ));
    node_params.subject_alt_names.push(SanType::URI(
        Ia5String::try_from(format!("urn:ironmesh:cluster:{cluster_id}"))
            .context("cluster identity URI is invalid")?,
    ));
    node_params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    let node_cert = node_params
        .signed_by(&node_key, &issuer)
        .context("failed signing test node certificate")?;

    let paths = NodeTlsPaths {
        ca_cert: root.join("ca.pem"),
        ca_key: root.join("ca.key"),
        node_cert: root.join("node.pem"),
        node_key: root.join("node.key"),
    };
    write(&paths.ca_cert, ca_pem)?;
    write(&paths.ca_key, ca_key_pem)?;
    write(&paths.node_cert, node_cert.pem())?;
    write(&paths.node_key, node_key.serialize_pem())?;
    Ok(paths)
}

fn cluster_ca_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "ironmesh-quic-network-test-ca");
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params
}

fn write(path: &Path, contents: String) -> Result<()> {
    fs::write(path, contents)
        .with_context(|| format!("failed writing TLS material {}", path.display()))
}
