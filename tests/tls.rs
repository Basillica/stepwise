mod common;
use common::*;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};
use rustls::{crypto::ring, ClientConfig, RootCertStore};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use serde_json::json;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use stepwise::{
    dataplane,
    state::{AppState, ApplyOpts},
    supervisor,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::watch,
};
use tokio_rustls::TlsConnector;

struct Ca {
    cert: Certificate,
    key: KeyPair,
}

fn make_ca(cn: &str) -> Ca {
    let key = KeyPair::generate().unwrap();
    let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
    p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    p.distinguished_name.push(rcgen::DnType::CommonName, cn);
    Ca {
        cert: p.self_signed(&key).unwrap(),
        key,
    }
}

fn issue(ca: &Ca, eku: ExtendedKeyUsagePurpose) -> (Certificate, KeyPair) {
    let key = KeyPair::generate().unwrap();
    let mut p = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    p.subject_alt_names
        .push(SanType::IpAddress("127.0.0.1".parse().unwrap()));
    p.extended_key_usages = vec![eku];
    (p.signed_by(&key, &ca.cert, &ca.key).unwrap(), key)
}

fn client_cfg(ca: &Ca, identity: Option<(Certificate, KeyPair)>) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.add(ca.cert.der().clone()).unwrap();
    let b = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots);
    Arc::new(match identity {
        Some((c, k)) => b
            .with_client_auth_cert(
                vec![CertificateDer::from(c.der().to_vec())],
                PrivateKeyDer::from(PrivatePkcs8KeyDer::from(k.serialize_der())),
            )
            .unwrap(),
        None => b.with_no_client_auth(),
    })
}

async fn https_get(addr: SocketAddr, cfg: Arc<ClientConfig>, path: &str) -> String {
    let attempt = async {
        let tcp = TcpStream::connect(addr).await?;
        let mut tls = TlsConnector::from(cfg)
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await?;
        tls.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await?;
        let mut buf = Vec::new();
        let _ = tls.read_to_end(&mut buf).await; // a missing close_notify is fine
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&buf).into_owned())
    };
    tokio::time::timeout(Duration::from_secs(5), attempt)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
}

async fn http_get(addr: SocketAddr, path: &str) -> String {
    let attempt = async {
        let mut tcp = TcpStream::connect(addr).await?;
        tcp.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await?;
        let mut buf = Vec::new();
        let _ = tcp.read_to_end(&mut buf).await;
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&buf).into_owned())
    };
    tokio::time::timeout(Duration::from_secs(5), attempt)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
}

fn ok(resp: &str) -> bool {
    resp.starts_with("HTTP/1.1 200")
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn start(st: &Arc<AppState>) -> watch::Sender<bool> {
    let (stop, rx) = watch::channel(false);
    tokio::spawn(supervisor::run_listener(
        "data_plane",
        st.clone(),
        st.data_listener.subscribe(),
        dataplane::router,
        rx,
    ));
    st.bound("data_plane"); // no-op; keeps intent obvious
    stop
}

async fn bound_addr(st: &Arc<AppState>, not: Option<SocketAddr>) -> SocketAddr {
    for _ in 0..200 {
        if let Some(a) = st.bound("data_plane").filter(|a| Some(*a) != not) {
            return a;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("listener never came up");
}

fn tls_config(
    dir: &std::path::Path,
    ca: &Ca,
    mtls: bool,
    require: Option<bool>,
) -> stepwise::config::Config {
    let (srv, srv_key) = issue(ca, ExtendedKeyUsagePurpose::ServerAuth);
    std::fs::write(dir.join("server.pem"), srv.pem()).unwrap();
    std::fs::write(dir.join("server.key"), srv_key.serialize_pem()).unwrap();
    std::fs::write(dir.join("ca.pem"), ca.cert.pem()).unwrap();
    let mut c = base_config();
    c.control.enabled = false;
    c.server.tls = stepwise::config::TlsConfig {
        enabled: true,
        cert_path: dir.join("server.pem").to_string_lossy().into(),
        key_path: dir.join("server.key").to_string_lossy().into(),
        client_ca_path: mtls.then(|| dir.join("ca.pem").to_string_lossy().into()),
        require_client_cert: require,
    };
    c
}

#[tokio::test]
async fn tls_serves_https_and_rejects_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    let ca = make_ca("test ca");
    let st = state(tls_config(dir.path(), &ca, false, None));
    let stop = start(&st).await;
    let addr = bound_addr(&st, None).await;

    let resp = https_get(addr, client_cfg(&ca, None), "/health").await;
    assert!(ok(&resp), "{resp}");
    assert!(
        resp.to_lowercase().contains("strict-transport-security"),
        "HSTS is sent over TLS:\n{resp}"
    );
    assert!(
        !ok(&http_get(addr, "/health").await),
        "plaintext HTTP must not be served on a TLS port"
    );

    // a client that does not trust our CA must refuse the server
    let stranger = make_ca("someone else");
    assert!(!ok(&https_get(
        addr,
        client_cfg(&stranger, None),
        "/health"
    )
    .await));
    let _ = stop.send(true);
}

#[tokio::test]
async fn mtls_requires_a_certificate_signed_by_the_client_ca() {
    let dir = tempfile::tempdir().unwrap();
    let ca = make_ca("test ca");
    let st = state(tls_config(dir.path(), &ca, true, None));
    let stop = start(&st).await;
    let addr = bound_addr(&st, None).await;

    let good = issue(&ca, ExtendedKeyUsagePurpose::ClientAuth);
    assert!(
        ok(&https_get(addr, client_cfg(&ca, Some(good)), "/health").await),
        "valid client cert is accepted"
    );
    assert!(
        !ok(&https_get(addr, client_cfg(&ca, None), "/health").await),
        "no client cert is rejected"
    );

    // signed by a different CA than the one the server trusts for clients
    let rogue = make_ca("rogue ca");
    let forged = issue(&rogue, ExtendedKeyUsagePurpose::ClientAuth);
    assert!(
        !ok(&https_get(addr, client_cfg(&ca, Some(forged)), "/health").await),
        "foreign-CA client cert is rejected"
    );
    let _ = stop.send(true);
}

#[tokio::test]
async fn optional_client_certs_when_require_is_false() {
    let dir = tempfile::tempdir().unwrap();
    let ca = make_ca("test ca");
    let st = state(tls_config(dir.path(), &ca, true, Some(false)));
    let stop = start(&st).await;
    let addr = bound_addr(&st, None).await;
    assert!(ok(&https_get(addr, client_cfg(&ca, None), "/health").await));
    let good = issue(&ca, ExtendedKeyUsagePurpose::ClientAuth);
    assert!(ok(
        &https_get(addr, client_cfg(&ca, Some(good)), "/health").await
    ));
    let _ = stop.send(true);
}

#[tokio::test]
async fn listener_restarts_gracefully_and_bad_edits_fall_back() {
    let mut c = base_config();
    c.control.enabled = false;
    let st = state(c);
    let stop = start(&st).await;
    let a = bound_addr(&st, None).await;
    assert!(ok(&http_get(a, "/health").await));

    // 1. move the data plane to a new port at runtime
    let port_b = free_port();
    let mut doc = serde_json::to_value(st.config().redacted()).unwrap();
    doc["server"]["port"] = json!(port_b);
    let rep = st
        .apply_json(doc.clone(), ApplyOpts::default())
        .await
        .unwrap();
    assert_eq!(rep.restarts, vec!["data_plane"]);
    let b = bound_addr(&st, Some(a)).await;
    assert_eq!(b.port(), port_b);
    assert!(ok(&http_get(b, "/health").await), "new listener serves");
    assert!(
        http_get(a, "/health").await.is_empty(),
        "old port is closed"
    );

    // 2. enable TLS with a certificate that does not exist: the listener must fall back
    doc["server"]["tls"] =
        json!({"enabled": true, "cert_path": "/nonexistent.pem", "key_path": "/nonexistent.key"});
    st.apply_json(doc, ApplyOpts::default()).await.unwrap(); // config is "valid"; the files are checked at bind time
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        ok(&http_get(b, "/health").await),
        "previous good listener config is restored after a failed restart"
    );
    let _ = stop.send(true);
}
