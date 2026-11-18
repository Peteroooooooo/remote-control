//! Runs the relay: iroh-relay with [`Policy`] as its access control and the
//! claim / guests / invites / meet routes, and keeps the policy's roster in
//! step with `owners.txt` (which the management screen may change while it
//! runs). It also writes `status.txt` every few seconds: when it started
//! and which devices are connected, for the management screen to show.

use std::{
    collections::{BTreeMap, HashMap},
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
    api::{self, ClaimRequest, GuestsRequest, InvitesRequest, MeetRequest, Refusal},
    config::{Config, Resolved},
    policy::Policy,
    state::{ActiveMeet, Code, Status, Store},
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
        let guests = guest_ids(self.store.guests()?);
        let blocked = self.store.blocked()?;
        self.policy.set_roster(owners.iter().map(|o| o.id), guests, blocked.into_iter().map(|b| b.id));
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

    /// A device joins with one of the relay's codes: it is an owner now,
    /// in that code's group. The code stays valid.
    fn claim(&self, header: &str) -> Result<String, Refusal> {
        let req = ClaimRequest::decode(header).map_err(|_| Refusal::BadRequest)?;
        let _g = self.writing.lock().unwrap();
        let server = |e: anyhow::Error| {
            warn!("claim: {e:#}");
            Refusal::Server
        };
        let Some(code) = self.store.code_with(&req.claim_key).map_err(server)? else {
            info!("claim by {}: no such code", req.device.fmt_short());
            return Err(Refusal::UnknownKey);
        };
        self.store.join(req.device, code.n, &req.name).map_err(server)?;
        self.reload().map_err(server)?;
        info!("{} ({}) joined with {}: it is an owner now", req.device.fmt_short(), req.name, code.shown_name());
        Ok("ok".into())
    }

    fn set_guests(&self, header: &str) -> Result<String, Refusal> {
        let req = GuestsRequest::decode(header).map_err(|_| Refusal::BadRequest)?;
        self.signed_by_owner("guests", &req.device, req.time, req.nonce)?;
        let _g = self.writing.lock().unwrap();
        self.store.set_guests(&req.device, &req.guests).map_err(|e| {
            warn!("guests: {e:#}");
            Refusal::Server
        })?;
        info!("{} lists {} guest(s)", req.device.fmt_short(), req.guests.len());
        self.policy.set_guests(req.device, req.guests.into_iter().map(|g| g.id).collect());
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
        if self.policy.is_blocked(&req.device) {
            info!("{d} brought a connection code of {o}: blocked");
            return Err(Refusal::UnknownInvite);
        }
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
        self.policy.admit(req.owner, req.device, &req.name, unix_now() + api::MEET_KEEP);
        info!("{d} ({}) brought a connection code of {o}: the two may meet here", req.name);
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
        store.upgrade()?;
        let owners: Vec<EndpointId> = store.owners()?.iter().map(|o| o.id).collect();
        let guests = guest_ids(store.guests()?);
        let blocked: Vec<EndpointId> = store.blocked()?.into_iter().map(|b| b.id).collect();
        let policy = Arc::new(Policy::new(owners, guests, blocked));
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
            (Method::POST, api::GUESTS_PATH, route(&shared, Shared::set_guests)),
            (Method::POST, api::INVITES_PATH, route(&shared, Shared::set_invites)),
            (Method::POST, api::MEET_PATH, route(&shared, Shared::meet)),
        ];
        let mut sc = ServerConfig::default();
        sc.relay = Some(rc);
        sc.quic = cfg.enable_quic_addr_discovery.then(|| QuicConfig::new(cfg.quic_bind_addr()));
        let server = relay::Server::spawn(sc).await.map_err(|e| anyhow!("start the relay: {e:#}"))?;
        let watch = tokio::spawn(watch_owners(shared.clone(), unix_now()));
        Ok(Relay { server, shared, _watch: watch })
    }

    pub fn https_addr(&self) -> Option<std::net::SocketAddr> {
        self.server.https_addr()
    }

    pub fn quic_addr(&self) -> Option<std::net::SocketAddr> {
        self.server.quic_addr()
    }

    /// The code to print at start: the first one (made on a new relay).
    pub fn first_code(&self) -> Result<Option<String>> {
        Ok(self.shared.store.first_code()?.map(|c| self.code_text(&c)))
    }

    /// What a device pastes to join with `code`.
    pub fn code_text(&self, code: &Code) -> String {
        RelayCode { urls: self.shared.urls.clone(), claim_key: code.key }.encode()
    }

    pub fn online(&self) -> std::collections::HashSet<EndpointId> {
        self.shared.policy.online()
    }

    pub async fn shutdown(self) -> Result<()> {
        self.server.shutdown().await.map_err(|e| anyhow!("{e:#}"))
    }

    pub async fn join(&mut self) -> Result<()> {
        self.server.join().await.map_err(|e| anyhow!("{e:#}"))?.map_err(|e| anyhow!("{e:#}"))
    }
}

fn guest_ids(m: BTreeMap<EndpointId, Vec<crate::api::Guest>>) -> BTreeMap<EndpointId, Vec<EndpointId>> {
    m.into_iter().map(|(o, gs)| (o, gs.into_iter().map(|g| g.id).collect())).collect()
}

/// Picks up changes other processes make to `owners.txt` and `blocked.txt`,
/// and writes `status.txt` (connected devices and temporary guests) when they change.
async fn watch_owners(sh: Arc<Shared>, started: u64) {
    let op = sh.store.owners_path();
    let bp = sh.store.blocked_path();
    let stamps = || {
        (
            std::fs::metadata(&op).and_then(|m| m.modified()).ok(),
            std::fs::metadata(&bp).and_then(|m| m.modified()).ok(),
        )
    };
    let mut last: (Option<SystemTime>, Option<SystemTime>) = stamps();
    let mut shown: Option<(Vec<EndpointId>, Vec<ActiveMeet>)> = None;
    let mut written = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let mut online: Vec<EndpointId> = sh.policy.online().into_iter().collect();
        online.sort();
        let meets: Vec<ActiveMeet> = sh
            .policy
            .admitted_list()
            .into_iter()
            .map(|(owner, guest, name)| ActiveMeet { owner, guest, name })
            .collect();
        let snap = (online, meets);
        // Again every minute even unchanged: the management screen reads its age.
        if shown.as_ref() != Some(&snap) || unix_now() >= written + 60 {
            let seen = sh.store.status().map(|s| s.seen).unwrap_or_default();
            let mut seen: BTreeMap<EndpointId, u64> = seen;
            let now = unix_now();
            for id in &snap.0 {
                seen.insert(*id, now);
            }
            let status = Status { started, written: now, online: snap.0.clone(), seen, meets: snap.1.clone() };
            match sh.store.set_status(&status) {
                Ok(()) => {
                    shown = Some(snap);
                    written = now;
                }
                Err(e) => warn!("status.txt: {e:#}"),
            }
        }
        let now = stamps();
        if now == last {
            continue;
        }
        last = now;
        match sh.reload() {
            Ok(()) => info!("roster changed: {} owner(s)", sh.policy.owners().len()),
            Err(e) => warn!("roster reload: {e:#}; keeping the previous list"),
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
    println!("rc-relay 已启动  {}", hosts.join(" · "));
    match relay.first_code()? {
        Some(code) => println!("中转码（你的每台电脑都贴这一个，贴到软件「设置 → 自建中转」）：
{code}"),
        None => println!("没有可用的中转码：在容器控制台执行 rc-relay，按 n 新建"),
    }
    println!("管理设备、分组和踢人：在容器控制台执行 rc-relay");
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
