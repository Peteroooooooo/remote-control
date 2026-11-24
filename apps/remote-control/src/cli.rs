//! `devices`, `set` and `ctl`: the device list, the settings and the running
//! node from the command line (for scripts, tests and computers without the
//! lobby window).

use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use rc_ident::relay_code::RelayCode;

use crate::{
    log, net, node,
    store::{self, Settings, Store},
};

#[derive(Args)]
pub struct DevicesArgs {
    /// Device key file (default: %APPDATA%\remote-control\device.key).
    #[arg(long)]
    key: Option<PathBuf>,
    /// Also look for devices on the LAN for this many seconds, printing each
    /// as it answers.
    #[arg(long)]
    scan: Option<f64>,
}

pub fn devices(a: DevicesArgs) -> Result<()> {
    // What the running lobby or service shows (its list, also in service mode).
    if a.key.is_none()
        && a.scan.is_none()
        && let Ok(list) = crate::term::devices()
    {
        if list.is_empty() {
            println!("no devices");
        }
        for (id, name, status) in list {
            println!("{name:<16} {status:<12} {id}");
        }
        return Ok(());
    }
    let key = a.key.unwrap_or_else(net::default_key_path);
    let s = Store::load(&store::devices_path(&key))?;
    let me = net::load_key(&key)?.public();
    println!("this device: {} ({me})", Settings::load(&store::settings_path(&key)).shown_name());
    if s.listed().count() == 0 {
        println!("no devices in the list");
    }
    for d in s.listed() {
        let role = if d.auto_accept { "my computer" } else { "trusted" };
        let addrs: Vec<String> = d.addrs.iter().map(|a| a.to_string()).collect();
        println!("{}  {:<16} {role}  {}", d.id, d.name, addrs.join(" "));
    }
    if let Some(secs) = a.scan {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        rt.block_on(scan(Duration::from_secs_f64(secs), &s, &me))?;
    }
    Ok(())
}

/// Queries once a second; prints each device when it first answers.
async fn scan(total: Duration, s: &Store, me: &iroh::EndpointId) -> Result<()> {
    let end = std::time::Instant::now() + total;
    let mut seen = std::collections::BTreeSet::new();
    println!("looking on the LAN for {:.0} s ...", total.as_secs_f64());
    while std::time::Instant::now() < end {
        for f in crate::discovery::scan(Duration::from_secs(1), me).await? {
            if seen.insert(f.id) {
                let listed = s.get(&f.id).is_some();
                log!("found {} {:<16} at {}{}", f.id, f.name, f.addr, if listed { ", in the device list" } else { "" });
            }
        }
    }
    if seen.is_empty() {
        println!("no device answered");
    }
    Ok(())
}

#[derive(Args)]
pub struct CtlArgs {
    /// The request and its fields (see `node::Node::request`), e.g. `snapshot`,
    /// `code`, `paste <text>`, `auto <id> 1`.
    request: Vec<String>,
}

/// Sends one request to the running node (the service's, else this
/// session's lobby or host) and prints the answer.
pub fn ctl(a: CtlArgs) -> Result<()> {
    if a.request.is_empty() {
        bail!("no request");
    }
    let req = a.request.join("\t");
    let answer = node::call(&req)?;
    print!("{answer}");
    if !answer.ends_with('\n') {
        println!();
    }
    Ok(())
}

pub const SET_HELP: &str = "\
Settings (as on the Settings page; the ones not given stay as they are):
  relay <rcr1…>        join a private relay with its relay code (repeatable: the first is the chosen one)
  public on|off        public relay assist
  lan on|off           LAN discovery
  name <name>          this computer's name (\"\" for the computer name)
  language auto|zh|en
  boot on|off          start with Windows (background service)
  lock on|off          lock screen control (background service)
  awake on|off         keep awake on AC power (background service)
  host-only on|off     only be controlled: the lobby leaves out every way of controlling others

The running lobby or background service takes them at once; with neither
running they are saved for the next start. Settings belong to the Windows
user who runs this.

Example:
  remote-control set relay rcr1… public off host-only on";

#[derive(Args)]
pub struct SetArgs {
    /// Change the settings next to this device key file, not the running
    /// lobby's or service's (default: this user's, %APPDATA%\remote-control).
    #[arg(long)]
    key: Option<PathBuf>,
    /// Settings and their values, in pairs.
    #[arg(required = true, allow_hyphen_values = true, value_name = "SETTING VALUE")]
    items: Vec<String>,
}

/// The node requests for `set`'s pairs, each with what it sets: the name
/// first (a relay shows it), then the switches, then the relays last to
/// first, so that the first one given ends up the chosen one. Nothing is
/// sent when a pair is wrong.
fn set_requests(items: &[String]) -> Result<Vec<(String, String)>> {
    let mut name = Vec::new();
    let mut switches = Vec::new();
    let mut relays = Vec::new();
    let mut it = items.iter();
    while let Some(key) = it.next() {
        let value = it.next().ok_or_else(|| anyhow!("`{key}` needs a value"))?;
        let value = value.replace(['\t', '\r', '\n'], " ").trim().to_string();
        let on = || match value.as_str() {
            "on" => Ok(1),
            "off" => Ok(0),
            _ => Err(anyhow!("`{key}` is on or off, not `{value}`")),
        };
        match key.as_str() {
            "relay" => {
                if RelayCode::find(&value).is_none() {
                    bail!("`{value}` is not a relay code (relay codes start with rcr1; copy it again from the relay)");
                }
                relays.push(("relay".to_string(), format!("relay-claim\t{value}")));
            }
            "name" => name.push((format!("name {value}"), format!("set-name\t{value}"))),
            "language" => {
                if !matches!(value.as_str(), "auto" | "zh" | "en") {
                    bail!("`language` is auto, zh or en, not `{value}`");
                }
                switches.push((format!("language {value}"), format!("set-language\t{value}")));
            }
            "public" | "lan" | "boot" | "lock" | "awake" | "host-only" => {
                switches.push((format!("{key} {value}"), format!("set\t{key}\t{}", on()?)));
            }
            _ => bail!("unknown setting `{key}` (one of relay, public, lan, name, language, boot, lock, awake, host-only)"),
        }
    }
    relays.reverse();
    Ok(name.into_iter().chain(switches).chain(relays).collect())
}

/// Hands the settings to the running node, or with none running here (or
/// with `--key`) to one started for the purpose, which goes away again.
pub fn set(a: SetArgs) -> Result<()> {
    let requests = set_requests(&a.items)?;
    let mut here = None;
    if a.key.is_some() || node::call_here("peek").is_err() {
        crate::stats::QUIET.store(true, std::sync::atomic::Ordering::Relaxed);
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        let mut opts = crate::host::HostArgs::defaults().node_options(false);
        opts.lan = false;
        if let Some(k) = a.key {
            opts.key = k;
        }
        let n = rt.block_on(node::Node::start(opts)).context("start remote-control")?;
        here = Some((rt, n));
    }
    let mut failed = None;
    for (what, line) in &requests {
        let answer = match &here {
            Some((_, n)) => n.request(line),
            None => match node::call_here(line) {
                Ok(a) => a,
                Err(e) => {
                    failed = Some(anyhow!("{what}: {e:#}"));
                    break;
                }
            },
        };
        match answer.split_once('\t') {
            Some(("ok", rest)) if !rest.trim().is_empty() => println!("{what}: ok ({})", rest.trim()),
            Some(("ok", _)) => println!("{what}: ok"),
            _ if answer == "ok" => println!("{what}: ok"),
            Some(("err", e)) => {
                failed = Some(anyhow!("{what}: {e}"));
                break;
            }
            _ => {
                failed = Some(anyhow!("{what}: unexpected answer {answer:?}"));
                break;
            }
        }
    }
    if let Some((rt, n)) = here {
        rt.block_on(n.stop());
        rt.shutdown_timeout(Duration::from_secs(1));
    }
    match failed {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    fn lines(s: &[&str]) -> Vec<String> {
        set_requests(&items(s)).unwrap().into_iter().map(|(_, l)| l).collect()
    }

    #[test]
    fn set_pairs_become_requests() {
        let a = RelayCode { urls: vec!["https://203.0.113.7:7843".parse().unwrap()], claim_key: [1; 16] }.encode();
        let b = RelayCode { urls: vec!["https://203.0.113.8:7843".parse().unwrap()], claim_key: [2; 16] }.encode();
        assert_eq!(
            lines(&["relay", &a, "public", "off", "relay", &b, "host-only", "on", "name", "财务-01"]),
            [
                "set-name\t财务-01".to_string(),
                "set\tpublic\t0".into(),
                "set\thost-only\t1".into(),
                format!("relay-claim\t{b}"),
                format!("relay-claim\t{a}"),
            ],
            "the name first, the first relay last (it ends up the chosen one)"
        );
        assert_eq!(lines(&["lan", "on", "language", "en", "name", ""]), ["set-name\t", "set\tlan\t1", "set-language\ten"]);
        assert_eq!(lines(&["name", "a\tb"]), ["set-name\ta b"]);
    }

    #[test]
    fn wrong_pairs_change_nothing() {
        for bad in [
            &["public"][..],
            &["public", "no"],
            &["relay", "https://203.0.113.7:7843"],
            &["language", "fr"],
            &["colour", "on"],
            &["lan", "off", "relay", "rcr1xyz"],
        ] {
            assert!(set_requests(&items(bad)).is_err(), "{bad:?}");
        }
    }
}
