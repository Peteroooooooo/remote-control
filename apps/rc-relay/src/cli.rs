//! Command line: `serve` on the relay's machine, `reset-claim` and `owners …`
//! next to it, and `claim` / `add-owner` from an owner's computer (what the
//! app does with a pasted relay code; for setup and tests).

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use iroh_base::{EndpointId, RelayUrl, SecretKey};
use iroh_relay::tls::CaTlsConfig;
use rc_ident::relay_code::RelayCode;
use rustls::pki_types::{CertificateDer, pem::PemObject};

use crate::{client, config::Config, policy::owners_of, state::Store};

#[cfg(unix)]
const DEFAULT_CONFIG: &str = "/etc/rc-relay/config.toml";
#[cfg(not(unix))]
const DEFAULT_CONFIG: &str = "rc-relay.toml";

#[derive(Parser)]
#[command(name = "rc-relay", version, about = "Self-hosted relay for remote-control")]
struct Cli {
    /// Config file (for serve, reset-claim, owners); without one, the defaults (see `config`).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
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
    /// Print a new relay code for claiming; the previous one stops working.
    ResetClaim,
    /// The devices that own this relay.
    Owners {
        #[command(subcommand)]
        cmd: OwnersCmd,
    },
    /// Claim a relay with a relay code, as the device whose key is given.
    Claim {
        code: String,
        #[command(flatten)]
        client: ClientArgs,
    },
    /// As an owner, get a relay code for another computer (valid one day, once).
    AddOwner {
        /// The relay's URL, e.g. https://relay.example.com:7843 (several: comma-separated)
        relay: String,
        #[command(flatten)]
        client: ClientArgs,
    },
}

#[derive(Subcommand)]
enum OwnersCmd {
    List,
    Add {
        id: String,
        #[arg(long, default_value = "")]
        note: String,
    },
    Remove {
        id: String,
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
    let default_log = if matches!(cli.cmd, Cmd::Serve { .. }) { "info,iroh_relay=warn" } else { "warn" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_log)),
        )
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();
    let rt = tokio::runtime::Runtime::new()?;
    match cli.cmd {
        Cmd::Serve { public } => rt.block_on(crate::server::run(load(&cli.config)?, &public)),
        Cmd::ResetClaim => {
            let cfg = load(&cli.config)?;
            let claim = Store::new(&cfg.state_dir).reset_initial_claim()?;
            println!("新的中转码（旧的作废）：\n{}", RelayCode { urls: cfg.saved_urls()?, claim_key: claim.key }.encode());
            Ok(())
        }
        Cmd::Owners { cmd } => owners(&load(&cli.config)?, cmd),
        Cmd::Claim { code, client } => {
            let code = match RelayCode::find(&code) {
                Some(c) => c,
                None => RelayCode::decode(&code).map_err(|e| anyhow!("relay code: {e}"))?,
            };
            let key = client.key()?;
            let urls = rt.block_on(client::claim(&code, &key, &client.ca()?))?;
            println!("认领成功：{} 现在是 {} 的主人", key.public(), urls[0]);
            Ok(())
        }
        Cmd::AddOwner { relay, client } => {
            let relay: Vec<RelayUrl> = relay.split(',').map(|u| u.trim().parse().map_err(|e| anyhow!("{u}: {e}"))).collect::<Result<_>>()?;
            let code = rt.block_on(client::add_owner(&relay, &client.key()?, &client.ca()?))?;
            println!("给另一台电脑的中转码（24 小时内有效，只能用一次）：\n{}", code.encode());
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

fn owners(cfg: &Config, cmd: OwnersCmd) -> Result<()> {
    let store = Store::new(&cfg.state_dir);
    let id = |s: &str| -> Result<EndpointId> { s.trim().parse().map_err(|e| anyhow!("{s}: not a device id: {e}")) };
    // An owner's own computers own the relay too; who gave each that.
    let derived = || -> Result<Vec<(EndpointId, EndpointId)>> {
        let stored: HashSet<EndpointId> = store.owners()?.iter().map(|o| o.id).collect();
        let guests = store.guests()?;
        let all = owners_of(&stored, &guests);
        let mut v = Vec::new();
        for (o, gs) in &guests {
            for g in gs.iter().filter(|g| g.mine && all.contains(o) && !stored.contains(&g.id)) {
                if !v.iter().any(|(d, _)| *d == g.id) {
                    v.push((g.id, *o));
                }
            }
        }
        Ok(v)
    };
    match cmd {
        OwnersCmd::List => {
            let owners = store.owners()?;
            if owners.is_empty() {
                println!("还没有主人");
            }
            let guests = store.guests()?;
            let n = |o: &EndpointId| guests.get(o).map_or(0, Vec::len);
            for o in owners {
                println!("{}\t{}\t名单上 {} 台\t{}", o.id, date(o.added), n(&o.id), o.note);
            }
            for (d, by) in derived()? {
                println!("{d}\t-\t名单上 {} 台\t{} 的我的电脑", n(&d), by.fmt_short());
            }
        }
        OwnersCmd::Add { id: s, note } => {
            if !store.add_owner(id(&s)?, &note)? {
                println!("已经是主人");
            }
        }
        OwnersCmd::Remove { id: s } => {
            let d = id(&s)?;
            if !store.remove_owner(&d)? {
                if let Some((_, by)) = derived()?.into_iter().find(|(x, _)| *x == d) {
                    bail!("{s} 是 {by} 的我的电脑：在那台电脑上把它删掉或者关掉它的免确认");
                }
                bail!("{s} 不是主人");
            }
        }
    }
    Ok(())
}

/// Unix seconds as a UTC date (YYYY-MM-DD).
fn date(secs: u64) -> String {
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
