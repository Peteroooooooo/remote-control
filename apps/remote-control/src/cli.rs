//! `devices` and `ctl`: the device list and the running node from the
//! command line (for tests and for computers without the lobby window).

use std::{path::PathBuf, time::Duration};

use anyhow::{Result, bail};
use clap::Args;

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
