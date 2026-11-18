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

use crate::api::{self, ClaimRequest, GuestsRequest, Invite, InvitesRequest, MeetRequest, Refusal};

const TIMEOUT: Duration = Duration::from_secs(10);
/// The head start of each address over the next one (Happy Eyeballs).
const HEAD_START: Duration = Duration::from_millis(250);

fn http(ca: &CaTlsConfig) -> Result<reqwest::Client> {
    let tls = ca.client_config(iroh_relay::tls::default_provider()).context("TLS setup")?;
    // iroh reaches relays without the system proxy; these calls go the same way.
    reqwest::Client::builder().tls_backend_preconfigured(tls).no_proxy().timeout(TIMEOUT).build().context("HTTP client")
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
pub async fn set_guests(relay: &[RelayUrl], device: &SecretKey, ca: &CaTlsConfig, guests: &[PublicKey]) -> Result<()> {
    let client = http(ca)?;
    let req = GuestsRequest::new(device, unix_now(), guests).encode();
    race(relay, |u| call(client.clone(), u, api::GUESTS_PATH, req.clone())).await?.1?;
    Ok(())
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
/// that relay lets `device` and the owner reach each other.
pub async fn meet(relay: &[RelayUrl], device: &SecretKey, ca: &CaTlsConfig, owner: PublicKey, token: [u8; 8]) -> Result<()> {
    let client = http(ca)?;
    let req = MeetRequest::new(device, owner, token).encode();
    race(relay, |u| call(client.clone(), u, api::MEET_PATH, req.clone())).await?.1?;
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
}
