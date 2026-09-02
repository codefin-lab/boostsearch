//! Serving over TLS.
//!
//! OpenSearch's security plugin puts the REST layer behind TLS by default;
//! so does this server once told to. The certificate and key are read from
//! the config directory (PEM), or made up as a self-signed pair the first
//! time nothing is there, the way the plugin's demo configuration does.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use serde_json::Value;

/// What the server was told about its TLS, from settings and environment.
#[derive(Clone, Debug, Default)]
pub struct TlsSettings {
    pub enabled: bool,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub trusted_cas: Option<PathBuf>,
    /// whether a client certificate is asked for
    pub client_auth: String,
}

/// The config directory: `BOOSTSEARCH_CONFIG`, else `<data>/config`, else
/// `./config`.
pub fn config_dir() -> PathBuf {
    if let Ok(d) = std::env::var("BOOSTSEARCH_CONFIG") {
        return PathBuf::from(d);
    }
    if let Ok(d) = std::env::var("BOOSTSEARCH_DATA")
        && !d.is_empty()
    {
        return PathBuf::from(d).join("config");
    }
    PathBuf::from("config")
}

/// The node's own settings file, `config/boostsearch.yml`, read as JSON-like
/// YAML; an absent file is an empty one.
pub fn node_settings() -> Value {
    let path = config_dir().join("boostsearch.yml");
    let Ok(text) = std::fs::read_to_string(&path) else { return Value::Object(Default::default()) };
    serde_yaml::from_str::<serde_yaml::Value>(&text)
        .ok()
        .and_then(|y| serde_json::to_value(y).ok())
        .unwrap_or(Value::Object(Default::default()))
}

/// One dotted setting, from the environment first (`BOOSTSEARCH_` + the
/// dotted name upper-cased with `_`), then the settings file.
pub fn node_setting(settings: &Value, key: &str) -> Option<String> {
    let env_name = format!(
        "BOOSTSEARCH_{}",
        key.trim_start_matches("plugins.security.").replace('.', "_").to_ascii_uppercase()
    );
    if let Ok(v) = std::env::var(&env_name) {
        return Some(v);
    }
    // the full dotted name spelled out is read as well
    let full = format!("BOOSTSEARCH_{}", key.replace('.', "_").to_ascii_uppercase());
    if let Ok(v) = std::env::var(&full) {
        return Some(v);
    }
    // written flat, or nested
    if let Some(v) = settings.get(key) {
        return Some(text_of(v));
    }
    let mut cur = settings;
    for part in key.split('.') {
        cur = cur.get(part)?;
    }
    Some(text_of(cur))
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

impl TlsSettings {
    pub fn read(settings: &Value) -> TlsSettings {
        let get = |k: &str| node_setting(settings, k);
        let enabled = get("plugins.security.ssl.http.enabled")
            .or_else(|| get("http.ssl.enabled"))
            .map(|v| v == "true")
            .unwrap_or(false);
        let dir = config_dir();
        let path_of = |v: Option<String>| -> Option<PathBuf> {
            v.map(|p| {
                let p = PathBuf::from(p);
                if p.is_absolute() { p } else { dir.join(p) }
            })
        };
        TlsSettings {
            enabled,
            cert: path_of(get("plugins.security.ssl.http.pemcert_filepath")),
            key: path_of(get("plugins.security.ssl.http.pemkey_filepath")),
            trusted_cas: path_of(get("plugins.security.ssl.http.pemtrustedcas_filepath")),
            client_auth: get("plugins.security.ssl.http.clientauth_mode")
                .unwrap_or_else(|| "OPTIONAL".into()),
        }
    }
}

/// The certificate and key to serve with: the ones named, or a self-signed
/// pair written into the config directory the first time.
pub fn load_or_make(
    settings: &TlsSettings,
) -> anyhow::Result<(
    Vec<rustls::pki_types::CertificateDer<'static>>,
    rustls::pki_types::PrivateKeyDer<'static>,
)> {
    let dir = config_dir();
    let cert_path = settings.cert.clone().unwrap_or_else(|| dir.join("certs").join("node.pem"));
    let key_path = settings.key.clone().unwrap_or_else(|| dir.join("certs").join("node-key.pem"));
    if !cert_path.exists() || !key_path.exists() {
        if settings.cert.is_some() || settings.key.is_some() {
            anyhow::bail!(
                "TLS certificate or key not found: {} / {}",
                cert_path.display(),
                key_path.display()
            );
        }
        make_self_signed(&cert_path, &key_path)?;
        eprintln!("boostsearch: made a self-signed certificate at {}", cert_path.display());
    }
    let certs =
        rustls_pemfile::certs(&mut std::io::BufReader::new(std::fs::File::open(&cert_path)?))
            .collect::<Result<Vec<_>, _>>()?;
    let key =
        rustls_pemfile::private_key(&mut std::io::BufReader::new(std::fs::File::open(&key_path)?))?
            .ok_or_else(|| anyhow::anyhow!("no private key in {}", key_path.display()))?;
    Ok((certs, key))
}

fn make_self_signed(cert_path: &Path, key_path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = cert_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()])?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.distinguished_name.push(rcgen::DnType::CommonName, "boostsearch node");
    params.distinguished_name.push(rcgen::DnType::OrganizationName, "BoostSearch");
    params
        .subject_alt_names
        .push(rcgen::SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)));
    let key = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    std::fs::write(cert_path, cert.pem())?;
    std::fs::write(key_path, key.serialize_pem())?;
    Ok(())
}

/// Serve the router over TLS on the listener, one task per connection.
pub async fn serve_tls(
    listener: tokio::net::TcpListener,
    app: Router,
    settings: &TlsSettings,
) -> anyhow::Result<()> {
    let (certs, key) = load_or_make(settings)?;
    let mut config =
        rustls::ServerConfig::builder().with_no_client_auth().with_single_cert(certs, key)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    // a client that comes back resumes rather than shaking hands again:
    // tickets for TLS 1.3, a session cache for TLS 1.2 (rustls issues
    // neither unless told to)
    if let Ok(ticketer) = rustls::crypto::ring::Ticketer::new() {
        config.ticketer = ticketer;
    }
    // one ticket is enough for a client that will resume; a second is a
    // record the client must read and decrypt for nothing
    config.send_tls13_tickets = 1;
    config.session_storage = rustls::server::ServerSessionMemoryCache::new(8192);
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => continue,
        };
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let Ok(tls) = acceptor.accept(stream).await else { return };
            let io = hyper_util::rt::TokioIo::new(tls);
            let service = hyper_util::service::TowerToHyperService::new(app);
            let _ =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection_with_upgrades(io, service)
                    .await;
        });
    }
}
