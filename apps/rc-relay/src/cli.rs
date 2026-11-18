//! Command line: `serve` runs the relay; without a command, the management
//! screen ([`crate::tui`]) for its codes and the devices that joined with
//! them. `codes` prints the same as plain text (for scripts and terminals
//! without a full screen), and `claim` joins a relay from a computer with a
//! device key file (what the app does with a pasted code; for tests).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use iroh_base::SecretKey;
use iroh_relay::tls::CaTlsConfig;
use rc_ident::relay_code::RelayCode;
use rustls::pki_types::{CertificateDer, pem::PemObject};

use crate::{client, config::Config, state::Store};

#[cfg(unix)]
const DEFAULT_CONFIG: &str = "/etc/rc-relay/config.toml";
#[cfg(not(unix))]
const DEFAULT_CONFIG: &str = "rc-relay.toml";

#[derive(Parser)]
#[command(name = "rc-relay", version, about = "Self-hosted relay for remote-control")]
struct Cli {
    /// Config file; without one, the defaults (see `config`).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the relay.
    Serve {
        /// This relay's public address (IP or host name), instead of the IPs
        /// found at start; repeatable, e.g. one IPv4 and one IPv6 address.
        #[arg(long, env = "RC_RELAY_PUBLIC", value_delimiter = ',')]
        public: Vec<String>,
    },
    /// The management screen (also what `rc-relay` alone opens).
    Ui {
        /// Made-up codes and devices in a temporary directory, to try the screen.
        #[arg(long)]
        demo: bool,
    },
    /// Print the codes and the devices that joined with each, as text.
    Codes,
    /// Join a relay with one of its codes, as the device whose key is given.
    Claim {
        code: String,
        /// The name the relay shows for this device.
        #[arg(long, default_value = "")]
        name: String,
        #[command(flatten)]
        client: ClientArgs,
    },
}

#[derive(clap::Args)]
struct ClientArgs {
    /// Device key file (default: %APPDATA%\remote-control\device.key).
    #[arg(long)]
    key: Option<PathBuf>,
    /// Trust only the certificates in this PEM file (for tests).
    #[arg(long)]
    ca: Option<PathBuf>,
}

impl ClientArgs {
    fn key(&self) -> Result<SecretKey> {
        let p = self.key.clone().unwrap_or_else(|| {
            let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
            base.join("remote-control").join("device.key")
        });
        let b = std::fs::read(&p).with_context(|| format!("read {}", p.display()))?;
        let arr: [u8; 32] = b.as_slice().try_into().map_err(|_| anyhow!("{}: a key file is 32 bytes", p.display()))?;
        Ok(SecretKey::from_bytes(&arr))
    }

    fn ca(&self) -> Result<CaTlsConfig> {
        let Some(p) = &self.ca else {
            return Ok(crate::tls::ca());
        };
        let certs = CertificateDer::pem_file_iter(p)
            .with_context(|| format!("open {}", p.display()))?
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("read {}", p.display()))?;
        Ok(CaTlsConfig::custom_roots(certs))
    }
}

pub fn main() -> Result<()> {
    let cli = Cli::parse();
    let serving = matches!(cli.cmd, Some(Cmd::Serve { .. }));
    if !matches!(cli.cmd, None | Some(Cmd::Ui { .. })) {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(if serving { "info,iroh_relay=warn" } else { "warn" })),
            )
            .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
            .init();
    }
    match cli.cmd {
        Some(Cmd::Serve { public }) => tokio::runtime::Runtime::new()?.block_on(crate::server::run(load(&cli.config)?, &public)),
        None => crate::tui::run(&load(&cli.config)?, false),
        Some(Cmd::Ui { demo }) => crate::tui::run(&load(&cli.config)?, demo),
        Some(Cmd::Codes) => codes(&load(&cli.config)?),
        Some(Cmd::Claim { code, name, client }) => {
            let code = match RelayCode::find(&code) {
                Some(c) => c,
                None => RelayCode::decode(&code).map_err(|e| anyhow!("relay code: {e}"))?,
            };
            let key = client.key()?;
            let rt = tokio::runtime::Runtime::new()?;
            let urls = rt.block_on(client::claim(&code, &key, &name, &client.ca()?))?;
            println!("已加入：{} 现在是 {} 的主人", key.public(), urls[0]);
            Ok(())
        }
    }
}

/// `--config`: that file (it must exist), else the default one if there is one, else the defaults.
fn load(path: &Option<PathBuf>) -> Result<Config> {
    match path {
        Some(p) => Config::load(p, true),
        None => Config::load(Path::new(DEFAULT_CONFIG), false),
    }
}

fn codes(cfg: &Config) -> Result<()> {
    use std::collections::HashSet;
    let store = Store::new(&cfg.state_dir);
    store.upgrade()?;
    let urls = cfg.saved_urls()?;
    let owners = store.owners()?;
    let codes = store.codes()?;
    let guests = store.guests()?;
    let kicked: HashSet<_> = store.kicked()?.into_iter().map(|k| (k.owner, k.guest)).collect();
    let status = store.status().unwrap_or_default();
    let owner_ids: HashSet<_> = owners.iter().map(|o| o.id).collect();
    if codes.is_empty() {
        println!("没有中转码。执行 rc-relay，按 n 新建。");
    }
    for c in codes {
        println!("{}\n  {}", c.shown_name(), RelayCode { urls: urls.clone(), claim_key: c.key }.encode());
        for o in owners.iter().filter(|o| o.code == c.n) {
            println!("  - {}\t{}\t{}", if o.name.is_empty() { "（没有名字）" } else { &o.name }, o.id.fmt_short(), date(o.added));
            let mut seen = HashSet::new();
            if let Some(gs) = guests.get(&o.id) {
                for g in gs {
                    if !owner_ids.contains(&g.id) && !kicked.contains(&(o.id, g.id)) && seen.insert(g.id) {
                        println!("    └ {}\t{}", if g.name.is_empty() { "（没有名字）" } else { &g.name }, g.id.fmt_short());
                    }
                }
            }
            for m in status.meets.iter().filter(|m| m.owner == o.id) {
                if !owner_ids.contains(&m.guest) && !kicked.contains(&(o.id, m.guest)) && seen.insert(m.guest) {
                    println!("    └ {}\t{}\t临时", if m.name.is_empty() { "（没有名字）" } else { &m.name }, m.guest.fmt_short());
                }
            }
        }
    }
    Ok(())
}

/// Unix seconds as a UTC date (YYYY-MM-DD).
pub fn date(secs: u64) -> String {
    // Howard Hinnant's days-to-civil.
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn dates() {
        assert_eq!(super::date(0), "1970-01-01");
        assert_eq!(super::date(951_782_400), "2000-02-29");
        assert_eq!(super::date(1_794_787_200), "2026-11-16");
    }
}
