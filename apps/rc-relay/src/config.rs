//! `config.toml`, all keys optional: the keys iroh-relay uses for the same
//! things, plus where the relay keeps its state and the addresses clients
//! reach it at. Without a file the relay runs on its defaults: its public IP
//! addresses, ports 7843 (HTTPS) and 7842 (QUIC), and a certificate of its
//! own whose fingerprint rides in the addresses it hands out (see
//! `rc_relay::tls`), so nothing needs setting up.
//!
//! With a domain name and a certificate for it:
//!
//! ```toml
//! state_dir = "/var/lib/rc-relay"
//! # What goes into relay codes: a domain name, or one URL per address family.
//! public_urls = ["https://relay.example.com:7843"]
//! http_bind_addr = "127.0.0.1:7841"      # captive-portal probe (plain HTTP)
//! enable_quic_addr_discovery = true
//!
//! [tls]
//! https_bind_addr = "[::]:7843"           # relay, /rc/claim, /rc/guests, /rc/invites, /rc/meet
//! quic_bind_addr = "[::]:7842"            # QUIC address discovery
//! cert_mode = "Reloading"                 # or "Manual" (read once); default "SelfSigned"
//! manual_cert_path = "/etc/rc-relay/tls/cert.pem"
//! manual_key_path = "/etc/rc-relay/tls/key.pem"
//! ```

use std::{
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use iroh_base::RelayUrl;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use serde::Deserialize;
use tracing::{info, warn};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_state")]
    pub state_dir: PathBuf,
    /// Empty: `https://<ip>:<port>` for each public IP of this server (one
    /// IPv4, one IPv6), found at start.
    #[serde(default)]
    pub public_urls: Vec<String>,
    #[serde(default = "default_http")]
    pub http_bind_addr: SocketAddr,
    #[serde(default = "yes")]
    pub enable_quic_addr_discovery: bool,
    #[serde(default)]
    pub tls: Tls,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    #[serde(default = "default_https")]
    pub https_bind_addr: SocketAddr,
    /// Default: the HTTPS address with port 7842.
    pub quic_bind_addr: Option<SocketAddr>,
    #[serde(default)]
    pub cert_mode: CertMode,
    pub manual_cert_path: Option<PathBuf>,
    pub manual_key_path: Option<PathBuf>,
}

impl Default for Tls {
    fn default() -> Tls {
        Tls {
            https_bind_addr: default_https(),
            quic_bind_addr: None,
            cert_mode: CertMode::default(),
            manual_cert_path: None,
            manual_key_path: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub enum CertMode {
    /// Read the files once at start.
    Manual,
    /// Read them again every day (a renewed certificate is picked up).
    Reloading,
    /// A certificate the relay makes itself (kept in the state directory);
    /// clients trust it by the fingerprint in the relay's addresses.
    #[default]
    SelfSigned,
}

/// What the relay serves with: its addresses and its TLS setup.
pub struct Resolved {
    pub urls: Vec<RelayUrl>,
    pub server_tls: rustls::ServerConfig,
}

fn default_state() -> PathBuf {
    PathBuf::from(if cfg!(unix) { "/var/lib/rc-relay" } else { "rc-relay-state" })
}

fn default_http() -> SocketAddr {
    "127.0.0.1:7841".parse().expect("valid")
}

fn default_https() -> SocketAddr {
    "[::]:7843".parse().expect("valid")
}

fn yes() -> bool {
    true
}

impl Default for Config {
    fn default() -> Config {
        Config {
            state_dir: default_state(),
            public_urls: Vec::new(),
            http_bind_addr: default_http(),
            enable_quic_addr_discovery: true,
            tls: Tls::default(),
        }
    }
}

impl Config {
    /// The file at `path`; the defaults when there is none and `required` is false.
    pub fn load(path: &Path, required: bool) -> Result<Config> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => return Ok(Config::default()),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let c: Config = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        c.check()?;
        Ok(c)
    }

    fn check(&self) -> Result<()> {
        if self.public_urls.len() > 4 {
            bail!("public_urls: give at most 4 URLs clients reach this relay at");
        }
        if self.tls.cert_mode != CertMode::SelfSigned {
            if self.tls.manual_cert_path.is_none() || self.tls.manual_key_path.is_none() {
                bail!("[tls]: cert_mode {:?} needs manual_cert_path and manual_key_path", self.tls.cert_mode);
            }
            if self.public_urls.is_empty() {
                bail!("public_urls: a certificate file is for a name; give the URLs it is for");
            }
        }
        self.configured_urls().map(|_| ())
    }

    fn configured_urls(&self) -> Result<Vec<RelayUrl>> {
        self.public_urls
            .iter()
            .map(|u| {
                if !u.starts_with("https://") {
                    bail!("public_urls: {u} must start with https://");
                }
                u.parse().map_err(|e| anyhow!("public_urls: {u}: {e}"))
            })
            .collect()
    }

    pub fn quic_bind_addr(&self) -> SocketAddr {
        self.tls.quic_bind_addr.unwrap_or_else(|| SocketAddr::new(self.tls.https_bind_addr.ip(), 7842))
    }

    fn urls_file(&self) -> PathBuf {
        self.state_dir.join("urls.txt")
    }

    /// The addresses `serve` printed last time (for the management screen and `codes`).
    pub fn saved_urls(&self) -> Result<Vec<RelayUrl>> {
        let text = std::fs::read_to_string(self.urls_file())
            .with_context(|| format!("{}: run `rc-relay serve` once first", self.urls_file().display()))?;
        text.lines().filter(|l| !l.trim().is_empty()).map(|l| l.trim().parse().map_err(|e| anyhow!("{l}: {e}"))).collect()
    }

    /// The addresses and the TLS setup. `public`: hosts or IPs to use instead
    /// of the ones found (`serve --public`).
    pub async fn resolve(&self, public: &[String]) -> Result<Resolved> {
        let provider = iroh_relay::tls::default_provider();
        let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .expect("protocols supported by ring")
            .with_no_client_auth();
        let port = self.tls.https_bind_addr.port();
        let (urls, server_tls) = match self.tls.cert_mode {
            CertMode::SelfSigned => {
                let (cert, key) = self_signed(&self.state_dir.join("tls"))?;
                let mut urls = self.configured_urls()?;
                if urls.is_empty() {
                    let hosts = if public.is_empty() { public_ips().await? } else { public.to_vec() };
                    urls = hosts.iter().map(|h| host_url(h, port)).collect::<Result<_>>()?;
                }
                let urls = urls.iter().map(|u| crate::tls::pinned_url(u, &cert)).collect();
                let tls = builder.with_single_cert(vec![cert], key).context("certificate and key")?;
                (urls, tls)
            }
            mode => {
                let (cert, key) = (self.tls.manual_cert_path.clone().expect("checked"), self.tls.manual_key_path.clone().expect("checked"));
                let mut urls = self.configured_urls()?;
                if !public.is_empty() {
                    urls = public.iter().map(|h| host_url(h, port)).collect::<Result<_>>()?;
                }
                let tls = if mode == CertMode::Manual {
                    let certs = CertificateDer::pem_file_iter(&cert)
                        .with_context(|| format!("open {}", cert.display()))?
                        .collect::<Result<Vec<_>, _>>()
                        .with_context(|| format!("read certificates from {}", cert.display()))?;
                    let key = PrivateKeyDer::from_pem_file(&key).with_context(|| format!("read key from {}", key.display()))?;
                    builder.with_single_cert(certs, key).context("certificate and key")?
                } else {
                    let resolver = iroh_relay::server::reloading_resolver(
                        &provider,
                        cert,
                        key,
                        iroh_relay::server::DEFAULT_CERT_RELOAD_INTERVAL,
                    )
                    .await
                    .map_err(|e| anyhow!("certificate: {e:#}"))?;
                    builder.with_cert_resolver(resolver)
                };
                (urls, tls)
            }
        };
        if urls.is_empty() {
            bail!("no address for this relay: give one with --public <IP>");
        }
        std::fs::create_dir_all(&self.state_dir).with_context(|| format!("create {}", self.state_dir.display()))?;
        let text: String = urls.iter().map(|u: &RelayUrl| format!("{u}\n")).collect();
        std::fs::write(self.urls_file(), text).with_context(|| format!("write {}", self.urls_file().display()))?;
        Ok(Resolved { urls, server_tls })
    }
}

/// `https://<host>:<port>` (an IPv6 address in brackets), or `host` itself if it is a URL.
fn host_url(host: &str, port: u16) -> Result<RelayUrl> {
    let h = host.trim();
    let s = if h.starts_with("https://") {
        h.to_string()
    } else if let Ok(IpAddr::V6(ip)) = h.trim_matches(['[', ']']).parse::<IpAddr>() {
        format!("https://[{ip}]:{port}")
    } else if h.contains(':') {
        format!("https://{h}")
    } else {
        format!("https://{h}:{port}")
    };
    s.parse().map_err(|e| anyhow!("{host}: {e}"))
}

/// The relay's own certificate: made once, then kept (its fingerprint is in
/// the addresses the owners have).
fn self_signed(dir: &Path) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let (cp, kp) = (dir.join("cert.pem"), dir.join("key.pem"));
    if !cp.exists() || !kp.exists() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let ck = rcgen::generate_simple_self_signed(vec!["rc-relay".to_string()]).context("make a certificate")?;
        std::fs::write(&kp, ck.signing_key.serialize_pem()).with_context(|| format!("write {}", kp.display()))?;
        std::fs::write(&cp, ck.cert.pem()).with_context(|| format!("write {}", cp.display()))?;
        info!("made a certificate: {}", cp.display());
    }
    let cert = CertificateDer::from_pem_file(&cp).with_context(|| format!("read {}", cp.display()))?;
    let key = PrivateKeyDer::from_pem_file(&kp).with_context(|| format!("read {}", kp.display()))?;
    Ok((cert, key))
}

/// This server's public IPs: an IPv6 and an IPv4 address on its network
/// interfaces; when no interface has a public IPv4 (behind 1:1 NAT, as on
/// AWS, or in a container), the one api.ipify.org sees.
pub async fn public_ips() -> Result<Vec<String>> {
    let mut v6 = None;
    let mut v4 = None;
    for i in netdev::get_interfaces() {
        if !i.is_up() || i.is_loopback() {
            continue;
        }
        if v6.is_none() {
            v6 = i.ipv6.iter().map(|n| n.addr()).find(|a| (a.segments()[0] & 0xe000) == 0x2000);
        }
        if v4.is_none() {
            v4 = i.ipv4.iter().map(|n| n.addr()).find(|a| {
                !(a.is_private() || a.is_loopback() || a.is_link_local() || (a.octets()[0] == 100 && (a.octets()[1] & 0xc0) == 64))
            });
        }
    }
    let v4 = match v4 {
        Some(a) => Some(a.to_string()),
        None => match ask_ipify().await {
            Ok(a) => Some(a),
            Err(e) => {
                warn!("public IPv4: {e:#}");
                None
            }
        },
    };
    let found: Vec<String> = v6.map(|a| a.to_string()).into_iter().chain(v4).collect();
    if found.is_empty() {
        bail!("found no public IP of this server: give it with --public <IP>");
    }
    info!("public addresses: {}", found.join(", "));
    Ok(found)
}

async fn ask_ipify() -> Result<String> {
    let tls = crate::tls::ca().client_config(iroh_relay::tls::default_provider()).context("TLS setup")?;
    let client = reqwest::Client::builder().tls_backend_preconfigured(tls).timeout(Duration::from_secs(5)).build()?;
    let text = client.get("https://api.ipify.org").send().await?.error_for_status()?.text().await?;
    let ip: std::net::Ipv4Addr = text.trim().parse().map_err(|_| anyhow!("api.ipify.org answered {text:?}"))?;
    Ok(ip.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_example() {
        let doc = include_str!("config.rs");
        let toml: String = doc
            .lines()
            .skip_while(|l| !l.starts_with("//! ```toml"))
            .skip(1)
            .take_while(|l| !l.starts_with("//! ```"))
            .map(|l| l.strip_prefix("//! ").or(l.strip_prefix("//!")).unwrap_or(l).to_string() + "\n")
            .collect();
        let c: Config = toml::from_str(&toml).unwrap();
        c.check().unwrap();
        assert_eq!(c.configured_urls().unwrap().len(), 1);
        assert_eq!(c.tls.cert_mode, CertMode::Reloading);
        assert_eq!(c.quic_bind_addr(), "[::]:7842".parse().unwrap());
        assert!(
            toml::from_str::<Config>(&format!("{toml}\naccess = \"everyone\"\n")).is_err(),
            "unknown keys are errors"
        );
        let example: Config = toml::from_str(include_str!("../deploy/config.example.toml")).unwrap();
        example.check().unwrap();
    }

    #[test]
    fn nothing_to_set_up() {
        let c: Config = toml::from_str("").unwrap();
        c.check().unwrap();
        assert_eq!(c.tls.cert_mode, CertMode::SelfSigned);
        assert_eq!(c.tls.https_bind_addr.port(), 7843);
        assert_eq!(c.quic_bind_addr().port(), 7842);
        let missing = Config::load(Path::new("no-such-dir/config.toml"), false).unwrap();
        assert!(missing.public_urls.is_empty());
        assert!(Config::load(Path::new("no-such-dir/config.toml"), true).is_err());
        let bad: Config = toml::from_str("[tls]\ncert_mode = \"Manual\"\n").unwrap();
        assert!(bad.check().is_err(), "a certificate file needs its paths and URLs");
    }

    #[test]
    fn hosts_become_urls() {
        assert_eq!(host_url("203.0.113.7", 7843).unwrap().as_str(), "https://203.0.113.7:7843/");
        assert_eq!(host_url("2001:db8::7", 7843).unwrap().as_str(), "https://[2001:db8::7]:7843/");
        assert_eq!(host_url("[2001:db8::7]", 7843).unwrap().as_str(), "https://[2001:db8::7]:7843/");
        assert_eq!(host_url("relay.example.com", 443).unwrap().as_str(), "https://relay.example.com/");
        assert_eq!(host_url("relay.example.com:9000", 7843).unwrap().as_str(), "https://relay.example.com:9000/");
    }

    #[tokio::test]
    async fn self_signed_urls_carry_the_pin() {
        let dir = std::env::temp_dir().join(format!("rc-relay-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let c = Config { state_dir: dir.clone(), ..Config::default() };
        let r = c.resolve(&["203.0.113.7".into(), "2001:db8::7".into()]).await.unwrap();
        assert_eq!(r.urls.len(), 2);
        let pin = crate::tls::pin_of(&r.urls[0]).unwrap();
        assert_eq!(crate::tls::pin_of(&r.urls[1]), Some(pin));
        // The certificate is kept: the same pin next time.
        let again = c.resolve(&["203.0.113.7".into()]).await.unwrap();
        assert_eq!(crate::tls::pin_of(&again.urls[0]), Some(pin));
        assert_eq!(c.saved_urls().unwrap(), again.urls);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
