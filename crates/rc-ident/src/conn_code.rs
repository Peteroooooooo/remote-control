//! Connection code `rc1…`: what one person sends another so their two
//! computers meet. It says whom to meet (a key of the sender's: its device
//! key, or the key it meets new devices with), proves the sender asked (a
//! random token the sender keeps in memory) and where to find it: the
//! public relay it is connected to, or its self-hosted relay (all of that
//! relay's addresses, so IPv4 and IPv6 both), and up to three addresses to
//! try directly.
//!
//! Payload: version 2, key, 8-byte token, where to find it (0 nowhere,
//! 1 relay: URL count, per URL length + UTF-8, 2 public relay number),
//! address count, per address 4 or 6, the IP and the port (big endian).
//! Version 1 had a single relay URL (length + UTF-8) after the 1; it is
//! still read.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use iroh_base::{PublicKey, RelayUrl};

use crate::{Error, text, wire::Reader};

pub const PREFIX: &str = "rc1";
const VERSION: u8 = 2;
pub const MAX_ADDRS: usize = 3;
/// A self-hosted relay reachable at an IPv4 and an IPv6 address has two.
pub const MAX_RELAY_URLS: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Find {
    Nowhere,
    /// The sender's own relay: its addresses (1 to [`MAX_RELAY_URLS`]).
    Relay(Vec<RelayUrl>),
    /// The number of the public relay the sender is connected to.
    Public(u8),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnCode {
    pub device: PublicKey,
    pub token: [u8; 8],
    pub find: Find,
    pub addrs: Vec<SocketAddr>,
}

impl ConnCode {
    pub fn encode(&self) -> String {
        let mut v = vec![VERSION];
        v.extend_from_slice(self.device.as_bytes());
        v.extend_from_slice(&self.token);
        match &self.find {
            Find::Relay(urls) if !urls.is_empty() => {
                let urls: Vec<String> =
                    urls.iter().map(|u| u.to_string()).filter(|s| s.len() <= 255).take(MAX_RELAY_URLS).collect();
                v.push(1);
                v.push(urls.len() as u8);
                for s in urls {
                    v.push(s.len() as u8);
                    v.extend_from_slice(s.as_bytes());
                }
            }
            Find::Relay(_) | Find::Nowhere => v.push(0),
            Find::Public(n) => v.extend_from_slice(&[2, *n]),
        }
        let addrs = &self.addrs[..self.addrs.len().min(MAX_ADDRS)];
        v.push(addrs.len() as u8);
        for a in addrs {
            match a.ip() {
                IpAddr::V4(ip) => {
                    v.push(4);
                    v.extend_from_slice(&ip.octets());
                }
                IpAddr::V6(ip) => {
                    v.push(6);
                    v.extend_from_slice(&ip.octets());
                }
            }
            v.extend_from_slice(&a.port().to_be_bytes());
        }
        text::encode(PREFIX, &v)
    }

    pub fn decode(code: &str) -> Result<ConnCode, Error> {
        let payload = text::decode(PREFIX, code)?;
        let mut r = Reader::new(&payload);
        let version = match r.u8()? {
            v @ (1 | VERSION) => v,
            v => return Err(Error::Version(v)),
        };
        let device = PublicKey::from_bytes(&r.array()?).map_err(|_| Error::Field("device key"))?;
        let token = r.array()?;
        let find = match r.u8()? {
            0 => Find::Nowhere,
            1 => {
                let count = if version == 1 { 1 } else { r.u8()? as usize };
                if count == 0 || count > MAX_RELAY_URLS {
                    return Err(Error::Field("relay URL count"));
                }
                let mut urls = Vec::with_capacity(count);
                for _ in 0..count {
                    let n = r.u8()? as usize;
                    let s = std::str::from_utf8(r.take(n)?).map_err(|_| Error::Field("relay URL"))?;
                    urls.push(s.parse().map_err(|_| Error::Field("relay URL"))?);
                }
                Find::Relay(urls)
            }
            2 => Find::Public(r.u8()?),
            _ => return Err(Error::Field("where to find")),
        };
        let n = r.u8()? as usize;
        if n > MAX_ADDRS {
            return Err(Error::Field("address count"));
        }
        let mut addrs = Vec::with_capacity(n);
        for _ in 0..n {
            let ip = match r.u8()? {
                4 => IpAddr::V4(Ipv4Addr::from(r.array::<4>()?)),
                6 => IpAddr::V6(Ipv6Addr::from(r.array::<16>()?)),
                _ => return Err(Error::Field("address")),
            };
            let port = u16::from_be_bytes(r.array()?);
            addrs.push(SocketAddr::new(ip, port));
        }
        r.finish()?;
        Ok(ConnCode { device, token, find, addrs })
    }

    /// The first connection code in `text` (a chat message, say).
    pub fn find(text: &str) -> Option<(ConnCode, &str)> {
        let s = text::find(PREFIX, text)?;
        Some((ConnCode::decode(s).ok()?, s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SecretKey;

    fn sample(find: Find) -> ConnCode {
        ConnCode {
            device: SecretKey::from_bytes(&[9; 32]).public(),
            token: [0x8c, 0x1f, 0x2a, 0x9d, 0x03, 0xb4, 0x7e, 0x65],
            find,
            addrs: vec![
                "[2001:db8::1]:51420".parse().unwrap(),
                "192.168.1.19:51420".parse().unwrap(),
                "198.51.100.10:51420".parse().unwrap(),
            ],
        }
    }

    #[test]
    fn round_trip() {
        let two = vec!["https://203.0.113.7:7843".parse().unwrap(), "https://[2001:db8::7]:7843".parse().unwrap()];
        let one = vec!["https://relay.example.com".parse().unwrap()];
        for find in [Find::Nowhere, Find::Public(3), Find::Relay(one), Find::Relay(two)] {
            let c = sample(find);
            let s = c.encode();
            assert!(s.starts_with(PREFIX));
            assert_eq!(ConnCode::decode(&s).unwrap(), c);
            let msg = format!("连这个：{s} 谢谢");
            assert_eq!(ConnCode::find(&msg).map(|(c, _)| c), Some(c));
        }
    }

    #[test]
    fn length_is_as_planned() {
        // The plan says about 100 to 130 characters.
        let n = sample(Find::Public(3)).encode().len();
        assert!((90..=140).contains(&n), "{n} characters");
        let mut few = sample(Find::Public(3));
        few.addrs.truncate(1);
        assert!(few.encode().len() < n);
    }

    #[test]
    fn extra_addresses_are_left_out() {
        let mut c = sample(Find::Nowhere);
        c.addrs.push("10.0.0.1:1".parse().unwrap());
        assert_eq!(ConnCode::decode(&c.encode()).unwrap().addrs.len(), MAX_ADDRS);
        let urls = (1..=3).map(|i| format!("https://203.0.113.{i}:7843").parse().unwrap()).collect();
        let c = sample(Find::Relay(urls));
        let Find::Relay(back) = ConnCode::decode(&c.encode()).unwrap().find else { panic!() };
        assert_eq!(back.len(), MAX_RELAY_URLS);
        assert_eq!(ConnCode::decode(&sample(Find::Relay(Vec::new())).encode()).unwrap().find, Find::Nowhere);
    }

    #[test]
    fn reads_version_1() {
        let device = SecretKey::from_bytes(&[9; 32]).public();
        let url = "https://203.0.113.7:7843/";
        let mut v = vec![1];
        v.extend_from_slice(device.as_bytes());
        v.extend_from_slice(&[5; 8]);
        v.extend_from_slice(&[1, url.len() as u8]);
        v.extend_from_slice(url.as_bytes());
        v.extend_from_slice(&[1, 4, 192, 168, 1, 19, 0xc8, 0xdc]);
        let c = ConnCode::decode(&text::encode(PREFIX, &v)).unwrap();
        assert_eq!(c.find, Find::Relay(vec![url.parse().unwrap()]));
        assert_eq!(c.addrs, vec!["192.168.1.19:51420".parse().unwrap()]);
    }

    #[test]
    fn rejects_other_codes() {
        assert_eq!(ConnCode::decode("rcd1abc"), Err(Error::WrongPrefix));
        let bad = text::encode(PREFIX, &[3]);
        assert_eq!(ConnCode::decode(&bad), Err(Error::Version(3)));
        let mut v = vec![2];
        v.extend_from_slice(SecretKey::from_bytes(&[9; 32]).public().as_bytes());
        v.extend_from_slice(&[0; 8]);
        v.extend_from_slice(&[7, 0]);
        assert_eq!(ConnCode::decode(&text::encode(PREFIX, &v)), Err(Error::Field("where to find")));
        let mut v = vec![2];
        v.extend_from_slice(SecretKey::from_bytes(&[9; 32]).public().as_bytes());
        v.extend_from_slice(&[0; 8]);
        v.extend_from_slice(&[1, 0, 0]);
        assert_eq!(ConnCode::decode(&text::encode(PREFIX, &v)), Err(Error::Field("relay URL count")));
    }
}
