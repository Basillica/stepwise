//! rustls server configuration, including optional mutual TLS.

use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::{crypto::ring, server::WebPkiClientVerifier, RootCertStore, ServerConfig};
use rustls_pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};

use crate::config::TlsConfig;

pub fn build_server_config(t: &TlsConfig) -> Result<Arc<ServerConfig>> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&t.cert_path)
        .with_context(|| format!("cannot read certificate {}", t.cert_path))?
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("bad certificate PEM in {}", t.cert_path))?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in {}", t.cert_path);
    }
    let key = PrivateKeyDer::from_pem_file(&t.key_path)
        .with_context(|| format!("cannot read private key {}", t.key_path))?;

    let provider = Arc::new(ring::default_provider());
    let builder = ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .context("TLS protocol setup")?;

    let builder = match &t.client_ca_path {
        Some(ca_path) => {
            let mut roots = RootCertStore::empty();
            for ca in CertificateDer::pem_file_iter(ca_path)
                .with_context(|| format!("cannot read client CA bundle {ca_path}"))?
            {
                roots.add(ca.with_context(|| format!("bad PEM in {ca_path}"))?)?;
            }
            let mut vb = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider);
            if t.require_client_cert == Some(false) {
                vb = vb.allow_unauthenticated();
            }
            builder.with_client_cert_verifier(vb.build().context("client certificate verifier")?)
        }
        None => builder.with_no_client_auth(),
    };

    let mut cfg = builder
        .with_single_cert(certs, key)
        .context("certificate/key mismatch")?;
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}
