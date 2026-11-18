//! End to end: a real rc-relay on 127.0.0.1 and iroh endpoints that can only
//! reach each other through it.

use std::{
    net::{Ipv4Addr, SocketAddr, TcpListener},
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use iroh::{
    Endpoint, EndpointAddr, RelayConfig, RelayMap, RelayMode, RelayUrl, SecretKey, TransportAddr, endpoint::presets,
};
use iroh_relay::tls::CaTlsConfig;
use rc_ident::{
    proof::{DeviceProof, unix_now},
    relay_code::RelayCode,
};
use rc_relay::{
    api::{self, GuestsRequest, Invite, Refusal},
    client,
    config::{Config, Tls},
    server::Relay,
    state::Store,
};

const ALPN: &[u8] = b"rc-relay/test";

struct Setup {
    relay: Relay,
    url: RelayUrl,
    dir: PathBuf,
}

impl Setup {
    /// A relay as it runs without a config file: its own certificate, pinned in its address.
    async fn new(name: &str) -> Setup {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rc-relay-it-{}-{name}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The port is picked before the relay binds it; another test may take it first.
        for _ in 0..5 {
            let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
            let cfg = Config {
                state_dir: dir.join("state"),
                http_bind_addr: "127.0.0.1:0".parse().unwrap(),
                enable_quic_addr_discovery: false,
                tls: Tls { https_bind_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, port)), ..Tls::default() },
                ..Config::default()
            };
            let resolved = cfg.resolve(&["127.0.0.1".to_string()]).await.unwrap();
            let url = resolved.urls[0].clone();
            assert!(rc_relay::tls::pin_of(&url).is_some(), "{url}");
            match Relay::spawn(&cfg, resolved).await {
                Ok(relay) => return Setup { relay, url, dir },
                Err(e) => eprintln!("relay on port {port}: {e:#}"),
            }
        }
        panic!("could not start the relay");
    }

    /// What the app trusts; the relay's pin is learnt from its address.
    fn ca(&self) -> CaTlsConfig {
        rc_relay::tls::learn(&self.url);
        rc_relay::tls::ca()
    }

    fn store(&self) -> Store {
        Store::new(self.dir.join("state"))
    }

    /// The code the relay prints at start.
    fn first_code(&self) -> RelayCode {
        RelayCode::decode(&self.relay.first_code().unwrap().expect("a new relay has a code")).unwrap()
    }

    /// Joins the relay with the code it prints at start.
    async fn claim(&self, device: &SecretKey) {
        assert_eq!(client::claim(&self.first_code(), device, "test", &self.ca()).await.unwrap(), vec![self.url.clone()]);
    }

    /// An endpoint that uses only this relay, optionally with a device proof.
    async fn endpoint(&self, key: SecretKey, token: Option<String>) -> Endpoint {
        let mut rc = RelayConfig::new(self.url.clone(), None);
        if let Some(t) = token {
            rc = rc.with_auth_token(t);
        }
        let ep = Endpoint::builder(presets::Minimal)
            .secret_key(key)
            .alpns(vec![ALPN.to_vec()])
            .relay_mode(RelayMode::Custom(RelayMap::from_iter([rc])))
            .ca_tls_config(self.ca())
            .clear_ip_transports()
            .bind()
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), ep.online()).await.expect("connected to the relay");
        tokio::spawn(serve(ep.clone()));
        ep
    }

    fn addr(&self, ep: &Endpoint) -> EndpointAddr {
        EndpointAddr::from_parts(ep.id(), [TransportAddr::Relay(self.url.clone())])
    }

    /// Whether `from` gets a QUIC connection to `to` and an answer on it.
    async fn reaches(&self, from: &Endpoint, to: &Endpoint) -> bool {
        let addr = self.addr(to);
        let attempt = async {
            let conn = from.connect(addr, ALPN).await.ok()?;
            let (mut s, mut r) = conn.open_bi().await.ok()?;
            s.write_all(b"ping").await.ok()?;
            s.finish().ok()?;
            let back = r.read_to_end(16).await.ok()?;
            conn.close(0u32.into(), b"");
            (back == b"ping").then_some(())
        };
        tokio::time::timeout(Duration::from_secs(5), attempt).await.ok().flatten().is_some()
    }

    async fn set_guests(&self, owner: &SecretKey, guests: &[&SecretKey]) {
        let ids: Vec<_> = guests.iter().map(|g| g.public()).collect();
        client::set_guests(std::slice::from_ref(&self.url), owner, &self.ca(), &ids).await.unwrap();
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Answers "ping" with "ping" and anything else with its length.
async fn serve(ep: Endpoint) {
    while let Some(inc) = ep.accept().await {
        tokio::spawn(async move {
            let Ok(conn) = inc.await else { return };
            while let Ok((mut s, mut r)) = conn.accept_bi().await {
                let Ok(data) = r.read_to_end(64 << 20).await else {
                    return;
                };
                let answer = if data == b"ping" { data } else { data.len().to_string().into_bytes() };
                let _ = s.write_all(&answer).await;
                let _ = s.finish();
                let _ = s.stopped().await;
            }
        });
    }
}

fn key() -> SecretKey {
    SecretKey::generate()
}

fn proof(device: &SecretKey, ephemeral: &SecretKey, expires: u64) -> String {
    DeviceProof::issue(device, ephemeral.public(), expires).encode()
}

#[tokio::test]
async fn unclaimed_relay_forwards_nothing() {
    let s = Setup::new("unclaimed").await;
    assert_eq!(client::ping(std::slice::from_ref(&s.url), &s.ca()).await.unwrap(), s.url, "answers its probe");
    let (a, b) = (s.endpoint(key(), None).await, s.endpoint(key(), None).await);
    assert!(!s.reaches(&a, &b).await);
    assert!(!s.reaches(&b, &a).await);
}

#[tokio::test]
async fn claim_then_only_the_roster_gets_help() {
    let s = Setup::new("claim").await;
    let code = s.first_code();
    let owner = key();
    assert_eq!(client::claim(&code, &owner, "DESKTOP", &s.ca()).await.unwrap(), vec![s.url.clone()]);
    let o = s.store().owners().unwrap();
    assert_eq!((o.len(), o[0].name.as_str()), (1, "DESKTOP"));

    let (gk, gk2) = (key(), key());
    let o = s.endpoint(owner.clone(), None).await;
    let (g, g2) = (s.endpoint(gk.clone(), None).await, s.endpoint(gk2.clone(), None).await);
    let x = s.endpoint(key(), None).await;
    assert!(!s.reaches(&x, &o).await, "stranger → owner");
    assert!(!s.reaches(&o, &x).await, "owner → stranger");
    assert!(!s.reaches(&g, &o).await, "not listed yet");

    // Listed while connected: no reconnect needed.
    s.set_guests(&owner, &[&gk, &gk2]).await;
    assert_eq!(s.store().guests().unwrap()[&owner.public()], [gk.public(), gk2.public()]);
    assert!(s.reaches(&g, &o).await, "guest → owner");
    assert!(s.reaches(&o, &g).await, "owner → guest");
    assert!(!s.reaches(&g, &g2).await, "guest → guest: no owner at either end");
    assert!(!s.reaches(&g, &x).await, "guest → stranger");
    assert!(!s.reaches(&x, &g).await, "stranger → guest");

    // The new list replaces the old one, at once.
    s.set_guests(&owner, &[&gk2]).await;
    assert!(!s.reaches(&g, &o).await, "taken off the list");
    assert!(s.reaches(&g2, &o).await, "still listed");

    // Only an owner may send a list.
    let e = client::set_guests(std::slice::from_ref(&s.url), &key(), &s.ca(), &[gk.public()]).await.unwrap_err();
    assert_eq!(e.downcast_ref::<Refusal>(), Some(&Refusal::NotOwner));
}

#[tokio::test]
async fn a_code_works_for_every_computer_until_deleted() {
    let s = Setup::new("codes").await;
    let code = s.first_code();
    let (a, b) = (key(), key());
    client::claim(&code, &a, "DESKTOP", &s.ca()).await.unwrap();
    client::claim(&code, &b, "laptop", &s.ca()).await.expect("the same code again, another computer");
    let (ea, eb) = (s.endpoint(a.clone(), None).await, s.endpoint(b.clone(), None).await);
    assert!(s.reaches(&ea, &eb).await, "two owners reach each other");

    // A second code, for another group.
    let other = s.store().new_code("爸妈").unwrap();
    assert_eq!(other.n, 2);
    let dad = key();
    let other_code = RelayCode { urls: code.urls.clone(), claim_key: other.key };
    client::claim(&other_code, &dad, "DAD-PC", &s.ca()).await.unwrap();
    let ed = s.endpoint(dad.clone(), None).await;
    assert!(s.reaches(&ed, &ea).await);

    // The group deleted: its device is out at once, and the code no longer works.
    assert_eq!(s.store().delete_code(other.n).unwrap(), [dad.public()]);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!s.reaches(&ed, &ea).await, "its code deleted");
    let e = client::claim(&other_code, &dad, "DAD-PC", &s.ca()).await.unwrap_err();
    assert_eq!(e.downcast_ref::<Refusal>(), Some(&Refusal::UnknownKey));

    // One device removed: out, but its code still works for it.
    assert!(s.store().remove_owner(&b.public()).unwrap());
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!s.reaches(&eb, &ea).await, "removed");
    client::claim(&code, &b, "laptop", &s.ca()).await.unwrap();
    assert!(s.reaches(&eb, &ea).await, "joined again with the same code");

    // A signed request seen twice; a request with a far-off clock.
    let http = reqwest::Client::builder()
        .tls_backend_preconfigured(s.ca().client_config(iroh_relay::tls::default_provider()).unwrap())
        .no_proxy()
        .build()
        .unwrap();
    let post = |req: String| {
        let http = http.clone();
        let mut url: url::Url = (*s.url).clone();
        url.set_path(api::GUESTS_PATH);
        async move { http.post(url).header(api::HEADER, req).send().await.unwrap().text().await.unwrap() }
    };
    let req = GuestsRequest::new(&a, unix_now(), &[]).encode();
    assert_eq!(post(req.clone()).await, "ok");
    assert_eq!(post(req).await, "replay");
    assert_eq!(post(GuestsRequest::new(&a, unix_now() - 1000, &[]).encode()).await, "clock");
    assert_eq!(post("rcqg1nonsense".into()).await, "bad-request");
}

#[tokio::test]
async fn a_device_with_a_connection_code_meets_its_owner() {
    let s = Setup::new("invites").await;
    let owner = key();
    s.claim(&owner).await;
    let (bk, xk, gk) = (key(), key(), key());
    s.set_guests(&owner, &[&gk]).await;
    let o = s.endpoint(owner.clone(), None).await;
    let (b, x, g) = (s.endpoint(bk.clone(), None).await, s.endpoint(xk.clone(), None).await, s.endpoint(gk, None).await);
    let urls = std::slice::from_ref(&s.url);
    let code = Invite { token: [5; 8], expires: unix_now() + 600 };
    client::set_invites(urls, &owner, &s.ca(), &[code]).await.unwrap();
    assert_eq!(s.store().invites().unwrap()[&owner.public()], [code]);
    assert!(!s.reaches(&b, &o).await, "before bringing the code");

    let refused = |e: anyhow::Error| e.downcast_ref::<Refusal>().copied();
    let e = client::meet(urls, &bk, &s.ca(), owner.public(), [6; 8]).await.unwrap_err();
    assert_eq!(refused(e), Some(Refusal::UnknownInvite), "another token");
    let e = client::meet(urls, &bk, &s.ca(), xk.public(), code.token).await.unwrap_err();
    assert_eq!(refused(e), Some(Refusal::UnknownInvite), "not an owner");
    client::meet(urls, &bk, &s.ca(), owner.public(), code.token).await.unwrap();
    assert!(s.reaches(&b, &o).await, "device with the code → owner");
    assert!(s.reaches(&o, &b).await, "owner → device with the code");
    assert!(!s.reaches(&b, &g).await, "only to the owner, not its guests");
    assert!(!s.reaches(&g, &b).await, "only to the owner, not its guests");
    client::meet(urls, &bk, &s.ca(), owner.public(), code.token).await.expect("the same device again");

    // Used up: another device cannot use it.
    let e = client::meet(urls, &xk, &s.ca(), owner.public(), code.token).await.unwrap_err();
    assert_eq!(refused(e), Some(Refusal::UnknownInvite));
    assert!(!s.reaches(&x, &o).await);
    assert!(!s.store().invites().unwrap().contains_key(&owner.public()));

    // A code the owner took back.
    let other = Invite { token: [7; 8], expires: unix_now() + 600 };
    client::set_invites(urls, &owner, &s.ca(), &[other]).await.unwrap();
    client::set_invites(urls, &owner, &s.ca(), &[]).await.unwrap();
    let e = client::meet(urls, &xk, &s.ca(), owner.public(), other.token).await.unwrap_err();
    assert_eq!(refused(e), Some(Refusal::UnknownInvite));

    let e = client::set_invites(urls, &xk, &s.ca(), &[other]).await.unwrap_err();
    assert_eq!(refused(e), Some(Refusal::NotOwner), "only an owner gives out codes");
}

#[tokio::test]
async fn device_proofs() {
    let s = Setup::new("proofs").await;
    let device = key();
    s.claim(&device).await;
    let guest = key();
    s.set_guests(&device, &[&guest]).await;
    let stranger = key();
    let now = unix_now();
    let o = s.endpoint(device.clone(), None).await;
    let g = s.endpoint(guest.clone(), None).await;

    let e = key();
    let ok = s.endpoint(e.clone(), Some(proof(&device, &e, now + 3600))).await;
    assert!(s.reaches(&g, &ok).await, "owner's session key: counts as the owner");
    assert!(s.reaches(&ok, &g).await, "owner's session key: counts as the owner");

    let e = key();
    let gs = s.endpoint(e.clone(), Some(proof(&guest, &e, now + 3600))).await;
    assert!(s.reaches(&gs, &o).await, "guest's session key → owner");
    assert!(s.reaches(&o, &gs).await, "owner → guest's session key");
    assert!(!s.reaches(&gs, &g).await, "guest's session key → guest: no owner");

    let e = key();
    let expired = s.endpoint(e.clone(), Some(proof(&device, &e, now - 1))).await;
    assert!(!s.reaches(&g, &expired).await, "expired proof");

    let e = key();
    let mut forged = DeviceProof::issue(&stranger, e.public(), now + 3600);
    forged.device = device.public();
    let forged = s.endpoint(e, Some(forged.encode())).await;
    assert!(!s.reaches(&g, &forged).await, "bad signature");

    // A real proof, but for another ephemeral key than the one connecting.
    let e = key();
    let borrowed = s.endpoint(e, Some(proof(&device, &key(), now + 3600))).await;
    assert!(!s.reaches(&g, &borrowed).await, "proof for another key");

    let e = key();
    let unlisted = s.endpoint(e.clone(), Some(proof(&stranger, &e, now + 3600))).await;
    assert!(!s.reaches(&unlisted, &o).await, "device not on the roster");
}

#[tokio::test]
async fn owners_file_changes_apply_while_running() {
    let s = Setup::new("owners-file").await;
    let first = key();
    s.claim(&first).await;
    let guest = key();
    s.set_guests(&first, &[&guest]).await;
    let k = key();
    let (a, b) = (s.endpoint(k.clone(), None).await, s.endpoint(first.clone(), None).await);
    let g = s.endpoint(guest, None).await;
    assert!(!s.reaches(&b, &a).await);
    // What the management screen does: change the files the relay reads.
    s.store().join(k.public(), 1, "test").unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(s.reaches(&b, &a).await, "added owner");
    assert!(s.store().remove_owner(&k.public()).unwrap());
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!s.reaches(&b, &a).await, "removed owner");
    // An owner removed: its guests go too.
    assert!(s.reaches(&g, &b).await);
    assert!(s.store().remove_owner(&first.public()).unwrap());
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!s.reaches(&g, &b).await, "the owner's guest after the owner was removed");
}

#[tokio::test]
async fn the_status_file_shows_who_is_online() {
    let s = Setup::new("status").await;
    let owner = key();
    s.claim(&owner).await;
    let _o = s.endpoint(owner.clone(), None).await;
    let t = std::time::Instant::now();
    while !s.store().status().unwrap().online.contains(&owner.public()) {
        assert!(t.elapsed() < Duration::from_secs(10), "the relay writes who is connected");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let st = s.store().status().unwrap();
    assert!(st.started > 0 && st.written >= st.started);
    assert!(st.seen.contains_key(&owner.public()));
}
