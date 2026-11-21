//! The calls a remote-control client makes to a relay: join it with a relay
//! code, keep its guest list and invites, bring an invite, and see whether
//! it answers. Errors the relay answered with are a [`Refusal`] inside the
//! `anyhow` error.
//!
//! A relay may have several addresses (an IPv4 and an IPv6 one); every call
//! goes to all of them, IPv6 first and each next one a moment later, and
//! the first answer counts (see [`race`]).

use std::{future::Future, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use iroh_base::{PublicKey, RelayUrl, SecretKey};
use iroh_relay::tls::CaTlsConfig;
use rc_ident::{proof::unix_now, relay_code::RelayCode};

use crate::api::{self, ClaimRequest, Guest, GuestsRequest, Invite, InvitesRequest, Kind, MeetRequest, Refusal};

const TIMEOUT: Duration = Duration::from_secs(10);
/// The head start of each address over the next one (Happy Eyeballs).
const HEAD_START: Duration = Duration::from_millis(250);

fn http_with_timeout(ca: &CaTlsConfig, timeout: Duration) -> Result<reqwest::Client> {
    let tls = ca.client_config(iroh_relay::tls::default_provider()).context("TLS setup")?;
    // iroh reaches relays without the system proxy; these calls go the same way.
    reqwest::Client::builder().tls_backend_preconfigured(tls).no_proxy().timeout(timeout).build().context("HTTP client")
}

fn http(ca: &CaTlsConfig) -> Result<reqwest::Client> {
    http_with_timeout(ca, TIMEOUT)
}

/// `relay` with `path` (its query, the certificate pin, stays).
fn api_url(relay: &RelayUrl, path: &str) -> String {
    let mut u: url::Url = (**relay).clone();
    u.set_path(path);
    u.to_string()
}

/// IPv6 addresses first (`target-plan` 5.5), otherwise as given.
pub fn eyeballs_order(urls: &[RelayUrl]) -> Vec<RelayUrl> {
    let v6 = |u: &RelayUrl| matches!(u.host(), Some(url::Host::Ipv6(_)));
    let mut v: Vec<RelayUrl> = urls.iter().filter(|u| v6(u)).cloned().collect();
    v.extend(urls.iter().filter(|u| !v6(u)).cloned());
    v
}

/// Runs `call` on every address of a relay, IPv6 first, each next one
/// [`HEAD_START`] later or as soon as an earlier one could not reach the
/// relay. The first answer that is not a refusal wins; a refusal counts only
/// when no address gives anything better (one address may refuse a claim
/// key that another just used). `Err` when no address answered at all.
pub async fn race<T, F, Fut>(urls: &[RelayUrl], call: F) -> Result<(RelayUrl, Result<T, Refusal>)>
where
    T: Send + 'static,
    F: Fn(RelayUrl) -> Fut,
    Fut: Future<Output = Result<Result<T, Refusal>>> + Send + 'static,
{
    crate::tls::learn_all(urls);
    let order = eyeballs_order(urls);
    if order.is_empty() {
        bail!("no relay address");
    }
    let mut set = tokio::task::JoinSet::new();
    let mut next = 0;
    let mut refused: Option<(RelayUrl, Refusal)> = None;
    let mut errors = Vec::new();
    loop {
        if next < order.len() && (set.is_empty() || next > 0) {
            let url = order[next].clone();
            let fut = call(url.clone());
            set.spawn(async move { (url, fut.await) });
            next += 1;
        }
        let waited = if next < order.len() {
            tokio::time::timeout(HEAD_START, set.join_next()).await.ok()
        } else {
            Some(set.join_next().await)
        };
        match waited {
            None => continue,
            Some(None) => break,
            Some(Some(Err(e))) => errors.push(format!("{e}")),
            Some(Some(Ok((url, r)))) => match r {
                Ok(Ok(v)) => return Ok((url, Ok(v))),
                Ok(Err(r)) => {
                    refused.get_or_insert((url, r));
                }
                Err(e) => errors.push(format!("{e:#}")),
            },
        }
        if set.is_empty() && next >= order.len() {
            break;
        }
    }
    match refused {
        Some((url, r)) => Ok((url, Err(r))),
        None => Err(anyhow!("could not reach the relay: {}", errors.join("; "))),
    }
}

/// `Err` only if the relay could not be reached; its answer is the inner result.
async fn call(client: reqwest::Client, relay: RelayUrl, path: &'static str, request: String) -> Result<Result<String, Refusal>> {
    let url = api_url(&relay, path);
    let res = client.post(&url).header(api::HEADER, request).send().await.with_context(|| format!("{relay}"))?;
    let ok = res.status().is_success();
    let body = res.text().await.with_context(|| format!("{relay}"))?;
    if ok {
        return Ok(Ok(body.trim().to_string()));
    }
    match Refusal::from_word(&body) {
        Some(r) => Ok(Err(r)),
        None => bail!("{relay}: not an rc-relay (answered {})", body.chars().take(80).collect::<String>()),
    }
}

/// Makes `device` (named `name`) an owner of the relay in `code`. Returns
/// the relay's addresses, the one that answered first.
pub async fn claim(code: &RelayCode, device: &SecretKey, name: &str, ca: &CaTlsConfig) -> Result<Vec<RelayUrl>> {
    let client = http(ca)?;
    let req = ClaimRequest::new(code.claim_key, device, name).encode();
    let (url, r) = race(&code.urls, |u| call(client.clone(), u, api::CLAIM_PATH, req.clone())).await?;
    r?;
    let mut urls = vec![url.clone()];
    urls.extend(code.urls.iter().filter(|u| **u != url).cloned());
    Ok(urls)
}

/// Tells the relay, as its owner `device`, the whole list of devices that
/// may use it to reach this owner (replaces the list sent before).
/// Returns any guest IDs the relay kicked and asks this owner to drop.
/// An older relay that does not take guest names gets the keys alone.
pub async fn set_guests(relay: &[RelayUrl], device: &SecretKey, ca: &CaTlsConfig, guests: &[Guest]) -> Result<Vec<PublicKey>> {
    let client = http(ca)?;
    let send = |guests: &[Guest]| {
        let req = GuestsRequest::new(device, unix_now(), guests).encode();
        let client = client.clone();
        async move { race(relay, |u| call(client.clone(), u, api::GUESTS_PATH, req.clone())).await }
    };
    let body = match send(guests).await?.1 {
        Err(Refusal::BadRequest) if guests.iter().any(|g| !g.name.is_empty() || g.kind != Kind::Unknown) => {
            let plain: Vec<Guest> = guests.iter().map(|g| Guest::from(g.id)).collect();
            send(&plain).await?.1?
        }
        r => r?,
    };
    Ok(parse_dropped(&body))
}

fn parse_dropped(body: &str) -> Vec<PublicKey> {
    let Some(rest) = body.trim().strip_prefix("drop\t") else {
        return Vec::new();
    };
    rest.split('\t').filter_map(|s| s.trim().parse().ok()).collect()
}

/// Tells the relay, as its owner `device`, its connection codes not used
/// yet (replaces the ones sent before).
pub async fn set_invites(relay: &[RelayUrl], device: &SecretKey, ca: &CaTlsConfig, invites: &[Invite]) -> Result<()> {
    let client = http(ca)?;
    let req = InvitesRequest::new(device, unix_now(), invites).encode();
    race(relay, |u| call(client.clone(), u, api::INVITES_PATH, req.clone())).await?.1?;
    Ok(())
}

/// Brings `owner`'s connection code (its token) to the owner's relay, so
/// that relay lets `device` (named `name`) and the owner reach each other.
pub async fn meet(relay: &[RelayUrl], device: &SecretKey, name: &str, ca: &CaTlsConfig, owner: PublicKey, token: [u8; 8]) -> Result<()> {
    let client = http(ca)?;
    let send = |name: &str| {
        let req = MeetRequest::new(device, owner, token, name).encode();
        let client = client.clone();
        async move { race(relay, |u| call(client.clone(), u, api::MEET_PATH, req.clone())).await }
    };
    match send(name).await?.1 {
        Err(Refusal::BadRequest) if !name.is_empty() => {
            send("").await?.1?;
        }
        r => {
            r?;
        }
    }
    Ok(())
}

/// The relay is older than `/rc/watch`: its owners learn of kicks only when
/// they send their guest list.
#[derive(Debug)]
pub struct NoWatch;

impl std::fmt::Display for NoWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the relay is too old to tell of changes at once")
    }
}

impl std::error::Error for NoWatch {}

/// Keeps `device`'s watch open on the relay (see `api`): calls `sync` on
/// every `sync` line, the first one as soon as it is open. Returns when the
/// relay or the network ends it, or nothing came for twice the beat. The
/// error is a [`Refusal`] or [`NoWatch`] when the relay said so.
pub async fn watch(relay: &[RelayUrl], device: &SecretKey, ca: &CaTlsConfig, sync: impl Fn()) -> Result<()> {
    let tls = ca.client_config(iroh_relay::tls::default_provider()).context("TLS setup")?;
    let client = reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .no_proxy()
        .connect_timeout(TIMEOUT)
        .read_timeout(Duration::from_secs(2 * api::WATCH_BEAT + 5))
        .build()
        .context("HTTP client")?;
    let req = api::WatchRequest::new(device, unix_now()).encode();
    let open = |u: RelayUrl| {
        let (client, req) = (client.clone(), req.clone());
        async move {
            let res = client.post(api_url(&u, api::WATCH_PATH)).header(api::HEADER, req).send().await.with_context(|| format!("{u}"))?;
            if res.status().is_success() {
                return Ok(Ok(Some(res)));
            }
            if res.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(Ok(None));
            }
            let body = res.text().await.with_context(|| format!("{u}"))?;
            match Refusal::from_word(&body) {
                Some(r) => Ok(Err(r)),
                None => bail!("{u}: not an rc-relay (answered {})", body.chars().take(80).collect::<String>()),
            }
        }
    };
    let (url, r) = race(relay, open).await?;
    let Some(mut res) = r? else { return Err(anyhow::Error::new(NoWatch)) };
    const MAX_WATCH_LINE: usize = 8192;
    let mut line = Vec::new();
    while let Some(chunk) = res.chunk().await.with_context(|| format!("{url}"))? {
        for &b in chunk.iter() {
            if b != b'\n' {
                if line.len() >= MAX_WATCH_LINE {
                    bail!("{url}: watch line exceeded {MAX_WATCH_LINE} bytes");
                }
                line.push(b);
                continue;
            }
            if line == b"sync" {
                sync();
            }
            line.clear();
        }
    }
    Ok(())
}

/// Whether the relay answers its probe (`/ping`) at one of its addresses.
pub async fn ping(relay: &[RelayUrl], ca: &CaTlsConfig) -> Result<RelayUrl> {
    let client = http(ca)?;
    let probe = |u: RelayUrl| {
        let client = client.clone();
        async move {
            let res = client.get(api_url(&u, "/ping")).send().await.with_context(|| format!("{u}"))?;
            if !res.status().is_success() {
                bail!("{u}: answered {}", res.status());
            }
            Ok(Ok(()))
        }
    };
    Ok(race(relay, probe).await?.0)
}

/// Measures a warm HTTP `/ping` round-trip time against one of the relay's
/// addresses (IPv6 first via `race`), using two sequential requests over the
/// same keep-alive connection so TCP/TLS handshake and transparent proxy
/// TCP-ack shortcuts do not skew the result.
pub async fn ping_rtt(relay: &[RelayUrl], ca: &CaTlsConfig, timeout: Duration) -> Result<Duration> {
    let client = http_with_timeout(ca, timeout)?;
    let probe = |u: RelayUrl| {
        let client = client.clone();
        async move {
            let target = api_url(&u, "/ping");
            let r1 = client.get(&target).send().await.with_context(|| format!("{u}"))?;
            if !r1.status().is_success() {
                bail!("{u}: answered {}", r1.status());
            }
            let _ = r1.bytes().await.with_context(|| format!("{u}"))?;
            let t0 = std::time::Instant::now();
            let r2 = client.get(&target).send().await.with_context(|| format!("{u}"))?;
            if !r2.status().is_success() {
                bail!("{u}: answered {}", r2.status());
            }
            let _ = r2.bytes().await.with_context(|| format!("{u}"))?;
            let rtt = t0.elapsed();
            Ok(Ok(rtt))
        }
    };
    let rtt = race(relay, probe).await?.1?;
    Ok(rtt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(v: &[&str]) -> Vec<RelayUrl> {
        v.iter().map(|u| u.parse().unwrap()).collect()
    }

    #[test]
    fn ipv6_goes_first() {
        let v = urls(&["https://203.0.113.7:7843", "https://[2001:db8::7]:7843", "https://relay.example.com"]);
        let o = eyeballs_order(&v);
        assert_eq!(o[0].host_str(), Some("[2001:db8::7]"));
        assert_eq!(o[1], v[0]);
        assert_eq!(o[2], v[2]);
    }

    #[test]
    fn api_urls_keep_the_pin() {
        let u: RelayUrl = "https://203.0.113.7:7843/?rc-cert=abc".parse().unwrap();
        assert_eq!(api_url(&u, api::CLAIM_PATH), "https://203.0.113.7:7843/rc/claim?rc-cert=abc");
        let d: RelayUrl = "https://relay.example.com:7843".parse().unwrap();
        assert_eq!(api_url(&d, "/ping"), "https://relay.example.com:7843/ping");
    }

    #[tokio::test]
    async fn race_takes_the_first_good_answer() {
        let v = urls(&["https://203.0.113.7:7843", "https://[2001:db8::7]:7843"]);
        // The IPv6 address is down: the IPv4 one answers.
        let (u, r) = race(&v, |u: RelayUrl| async move {
            if u.host_str() == Some("[2001:db8::7]") { bail!("unreachable") } else { Ok(Ok(1)) }
        })
        .await
        .unwrap();
        assert_eq!((u, r.unwrap()), (v[0].clone(), 1));
        // One refuses, the other (slower) takes it: the take wins.
        let (_, r) = race(&v, |u: RelayUrl| async move {
            if u.host_str() == Some("[2001:db8::7]") {
                Ok(Err(Refusal::UnknownKey))
            } else {
                tokio::time::sleep(Duration::from_millis(400)).await;
                Ok(Ok(2))
            }
        })
        .await
        .unwrap();
        assert_eq!(r.unwrap(), 2);
        // Both refuse.
        let (_, r) = race(&v, |_| async { Ok::<_, anyhow::Error>(Err::<(), _>(Refusal::NotOwner)) }).await.unwrap();
        assert_eq!(r.unwrap_err(), Refusal::NotOwner);
        // Neither answers.
        assert!(race(&v, |_| async { Err::<Result<(), Refusal>, _>(anyhow!("down")) }).await.is_err());
    }

    #[test]
    fn http_with_timeout_builds() {
        assert!(http_with_timeout(&crate::tls::ca(), Duration::from_millis(500)).is_ok());
    }

    #[tokio::test]
    async fn ping_rtt_empty_relay_errors() {
        let err = ping_rtt(&[], &crate::tls::ca(), Duration::from_secs(1)).await;
        assert!(err.is_err());
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn ping_rtt_loopback() {
        use std::net::{Ipv4Addr, SocketAddr, TcpListener};

        let dir = std::env::temp_dir().join(format!("rc-relay-ping-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let (relay, url) = {
            let mut spawned = None;
            for _ in 0..5 {
                let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
                let cfg = crate::config::Config {
                    state_dir: dir.join("state"),
                    http_bind_addr: "127.0.0.1:0".parse().unwrap(),
                    enable_quic_addr_discovery: false,
                    tls: crate::config::Tls {
                        https_bind_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let resolved = cfg.resolve(&["127.0.0.1".to_string()]).await.unwrap();
                let url = resolved.urls[0].clone();
                if let Ok(r) = crate::server::Relay::spawn(&cfg, resolved).await {
                    spawned = Some((r, url));
                    break;
                }
            }
            spawned.expect("could not bind loopback relay")
        };

        let rtt = ping_rtt(std::slice::from_ref(&url), &crate::tls::ca(), Duration::from_secs(4)).await.unwrap();
        assert!(rtt < Duration::from_secs(2), "unexpectedly slow loopback RTT: {rtt:?}");
        drop(relay);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
