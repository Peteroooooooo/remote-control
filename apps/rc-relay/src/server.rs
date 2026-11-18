//! Runs the relay: iroh-relay with [`Policy`] as its access control and the
//! claim / add-owner / guests / invites / meet routes, and keeps the policy's roster in step
//! with `owners.txt` (which `rc-relay owners …` may change while it runs).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use anyhow::{Result, anyhow};
use bytes::Bytes;
use http::{Method, Request, Response, StatusCode, response::Builder};
use http_body_util::Full;
use hyper::body::Incoming;
use iroh_base::{EndpointId, RelayUrl};
use iroh_relay::server::{
    self as relay, CertConfig, QuicConfig, RelayConfig, ServerConfig, TlsConfig,
    http_server::{BytesBody, HyperHandler, HyperResult},
};
use rc_ident::{proof::unix_now, relay_code::RelayCode};
use tracing::{info, warn};

use crate::{
    api::{self, AddOwnerRequest, ClaimRequest, GuestsRequest, InvitesRequest, MeetRequest, Refusal},
    config::{Config, Resolved},
    policy::Policy,
    state::{Claim, Store},
};

struct Shared {
    store: Store,
    policy: Arc<Policy>,
    urls: Vec<RelayUrl>,
    /// Signed requests seen (device, nonce) and when they may be forgotten.
    seen: Mutex<HashMap<(EndpointId, [u8; 16]), u64>>,
    /// One change to the state files at a time within this process.
    writing: Mutex<()>,
}

impl Shared {
    fn reload(&self) -> Result<()> {
        let owners = self.store.owners()?;
        let guests = self.store.guests()?;
        self.policy.set_roster(owners.iter().map(|o| o.id), guests);
        Ok(())
    }

    /// The checks every signed owner request goes through: time, owner, once.
    fn signed_by_owner(&self, what: &str, device: &EndpointId, time: u64, nonce: [u8; 16]) -> Result<(), Refusal> {
        let now = unix_now();
        if time.abs_diff(now) > api::MAX_SKEW {
            info!("{what} by {}: clock off by {} s", device.fmt_short(), time.abs_diff(now));
            return Err(Refusal::Clock);
        }
        if !self.policy.owners().contains(device) {
            info!("{what} by {}: not an owner", device.fmt_short());
            return Err(Refusal::NotOwner);
        }
        let mut seen = self.seen.lock().unwrap();
        seen.retain(|_, until| *until > now);
        if seen.insert((*device, nonce), time + 2 * api::MAX_SKEW).is_some() {
            return Err(Refusal::Replay);
        }
        Ok(())
    }

    fn claim(&self, header: &str) -> Result<String, Refusal> {
        let req = ClaimRequest::decode(header).map_err(|_| Refusal::BadRequest)?;
        let _g = self.writing.lock().unwrap();
        let server = |e: anyhow::Error| {
            warn!("claim: {e:#}");
            Refusal::Server
        };
        if !self.store.take_claim(&req.claim_key).map_err(server)? {
            info!("claim by {}: unknown key", req.device.fmt_short());
            return Err(Refusal::UnknownKey);
        }
        self.store.add_owner(req.device, "claimed").map_err(server)?;
        self.reload().map_err(server)?;
        info!("{} claimed the relay: it is an owner now", req.device);
        Ok("ok".into())
    }

    fn add_owner(&self, header: &str) -> Result<String, Refusal> {
        let req = AddOwnerRequest::decode(header).map_err(|_| Refusal::BadRequest)?;
        self.signed_by_owner("add-owner", &req.device, req.time, req.nonce)?;
        let _g = self.writing.lock().unwrap();
        let claim = Claim::valid_for(api::ADDED_CODE_LIFETIME);
        self.store.add_claim(claim.clone()).map_err(|e| {
            warn!("add-owner: {e:#}");
            Refusal::Server
        })?;
        info!("{} asked for a relay code for another computer", req.device.fmt_short());
        Ok(RelayCode { urls: self.urls.clone(), claim_key: claim.key }.encode())
    }

    fn set_guests(&self, header: &str) -> Result<String, Refusal> {
        let req = GuestsRequest::decode(header).map_err(|_| Refusal::BadRequest)?;
        self.signed_by_owner("guests", &req.device, req.time, req.nonce)?;
        let _g = self.writing.lock().unwrap();
        self.store.set_guests(&req.device, &req.guests).map_err(|e| {
            warn!("guests: {e:#}");
            Refusal::Server
        })?;
        let mine = req.guests.iter().filter(|g| g.mine).count();
        info!("{} lists {} guest(s), {mine} of them its own computers", req.device.fmt_short(), req.guests.len());
        let (came, went) = self.policy.set_guests(req.device, req.guests);
        for o in came {
            info!("{} owns the relay now (one of {}'s computers)", o.fmt_short(), req.device.fmt_short());
        }
        for o in went {
            info!("{} no longer owns the relay", o.fmt_short());
        }
        Ok("ok".into())
    }

    fn set_invites(&self, header: &str) -> Result<String, Refusal> {
        let req = InvitesRequest::decode(header).map_err(|_| Refusal::BadRequest)?;
        self.signed_by_owner("invites", &req.device, req.time, req.nonce)?;
        let _g = self.writing.lock().unwrap();
        self.store.set_invites(&req.device, &req.invites).map_err(|e| {
            warn!("invites: {e:#}");
            Refusal::Server
        })?;
        info!("{} has {} unused connection code(s)", req.device.fmt_short(), req.invites.len());
        Ok("ok".into())
    }

    /// A device not on the roster with an owner's connection code: the two
    /// may reach each other for a while (see `policy`).
    fn meet(&self, header: &str) -> Result<String, Refusal> {
        let req = MeetRequest::decode(header).map_err(|_| Refusal::BadRequest)?;
        let (o, d) = (req.owner.fmt_short(), req.device.fmt_short());
        if self.policy.admitted(&req.owner, &req.device) {
            return Ok("ok".into());
        }
        if !self.policy.owners().contains(&req.owner) {
            info!("{d} brought a connection code of {o}: not an owner");
            return Err(Refusal::UnknownInvite);
        }
        let _g = self.writing.lock().unwrap();
        let taken = self.store.take_invite(&req.owner, &req.token).map_err(|e| {
            warn!("meet: {e:#}");
            Refusal::Server
        })?;
        if !taken {
            info!("{d} brought a connection code of {o}: unknown, used or expired");
            return Err(Refusal::UnknownInvite);
        }
        self.policy.admit(req.owner, req.device, unix_now() + api::MEET_KEEP);
        info!("{d} brought a connection code of {o}: the two may meet here");
        Ok("ok".into())
    }
}

fn reply(res: Builder, r: Result<String, Refusal>) -> HyperResult<Response<BytesBody>> {
    let (status, body) = match r {
        Ok(b) => (StatusCode::OK, b),
        Err(e) => (e.status(), e.word().to_string()),
    };
    let body: BytesBody = Box::new(Full::new(Bytes::from(body)));
    Ok(res.status(status).header("content-type", "text/plain; charset=utf-8").body(body)?)
}

fn route(sh: &Arc<Shared>, f: fn(&Shared, &str) -> Result<String, Refusal>) -> HyperHandler {
    let sh = sh.clone();
    Box::new(move |req: Request<Incoming>, res: Builder| {
        let r = match req.headers().get(api::HEADER).and_then(|v| v.to_str().ok()) {
            Some(h) => f(&sh, h),
            None => Err(Refusal::BadRequest),
        };
        reply(res, r)
    })
}

/// A running relay. Dropping it stops the server.
pub struct Relay {
    server: relay::Server,
    shared: Arc<Shared>,
    _watch: tokio::task::JoinHandle<()>,
}

impl Relay {
    pub async fn spawn(cfg: &Config, resolved: Resolved) -> Result<Relay> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let store = Store::new(&cfg.state_dir);
        let owners: Vec<EndpointId> = store.owners()?.iter().map(|o| o.id).collect();
        let policy = Arc::new(Policy::new(owners, store.guests()?));
        let shared = Arc::new(Shared {
            store,
            policy: policy.clone(),
            urls: resolved.urls,
            seen: Mutex::default(),
            writing: Mutex::default(),
        });
        let tls = resolved.server_tls;
        let mut rc = RelayConfig::new(cfg.http_bind_addr);
        rc.tls = Some(TlsConfig::new(cfg.tls.https_bind_addr, CertConfig::Manual { server_config: tls }));
        rc.access = policy;
        rc.handlers = vec![
            (Method::POST, api::CLAIM_PATH, route(&shared, Shared::claim)),
            (Method::POST, api::ADD_OWNER_PATH, route(&shared, Shared::add_owner)),
            (Method::POST, api::GUESTS_PATH, route(&shared, Shared::set_guests)),
            (Method::POST, api::INVITES_PATH, route(&shared, Shared::set_invites)),
            (Method::POST, api::MEET_PATH, route(&shared, Shared::meet)),
        ];
        let mut sc = ServerConfig::default();
        sc.relay = Some(rc);
        sc.quic = cfg.enable_quic_addr_discovery.then(|| QuicConfig::new(cfg.quic_bind_addr()));
        let server = relay::Server::spawn(sc).await.map_err(|e| anyhow!("start the relay: {e:#}"))?;
        let watch = tokio::spawn(watch_owners(shared.clone()));
        Ok(Relay { server, shared, _watch: watch })
    }

    pub fn https_addr(&self) -> Option<std::net::SocketAddr> {
        self.server.https_addr()
    }

    pub fn quic_addr(&self) -> Option<std::net::SocketAddr> {
        self.server.quic_addr()
    }

    /// The relay code to print while nobody owns the relay.
    pub fn unclaimed_code(&self) -> Result<Option<String>> {
        if !self.shared.store.owners()?.is_empty() {
            return Ok(None);
        }
        let c = self.shared.store.initial_claim()?;
        Ok(Some(RelayCode { urls: self.shared.urls.clone(), claim_key: c.key }.encode()))
    }

    pub fn connected(&self) -> Vec<(EndpointId, bool)> {
        self.shared.policy.connected()
    }

    pub async fn shutdown(self) -> Result<()> {
        self.server.shutdown().await.map_err(|e| anyhow!("{e:#}"))
    }

    pub async fn join(&mut self) -> Result<()> {
        self.server.join().await.map_err(|e| anyhow!("{e:#}"))?.map_err(|e| anyhow!("{e:#}"))
    }
}

/// Picks up changes other processes make to `owners.txt`.
async fn watch_owners(sh: Arc<Shared>) {
    let path = sh.store.owners_path();
    let stamp = || std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let mut last: Option<SystemTime> = stamp();
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let now = stamp();
        if now == last {
            continue;
        }
        last = now;
        match sh.reload() {
            Ok(()) => info!("owners.txt changed: {} owner(s)", sh.policy.owners().len()),
            Err(e) => warn!("owners.txt: {e:#}; keeping the previous list"),
        }
    }
}

/// `rc-relay serve`: runs until Ctrl+C or SIGTERM.
pub async fn run(cfg: Config, public: &[String]) -> Result<()> {
    let resolved = cfg.resolve(public).await?;
    let hosts: Vec<String> = resolved
        .urls
        .iter()
        .map(|u| format!("{}{}", u.host_str().unwrap_or("?"), u.port().map(|p| format!(":{p}")).unwrap_or_default()))
        .collect();
    let mut relay = Relay::spawn(&cfg, resolved).await?;
    println!("中转已启动：{}（HTTPS {}，QUIC {}）", hosts.join("  "), cfg.tls.https_bind_addr, cfg.quic_bind_addr());
    match relay.unclaimed_code()? {
        Some(code) => println!("还没有主人。中转码（贴到软件「设置 → 自建中转」）：\n{code}"),
        None => println!("主人 {} 台", relay.shared.store.owners()?.len()),
    }
    tokio::select! {
        r = relay.join() => r?,
        _ = stop_signal() => info!("stopping"),
    }
    relay.shutdown().await
}

async fn stop_signal() {
    #[cfg(unix)]
    {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
