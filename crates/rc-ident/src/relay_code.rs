//! Relay code `rcr1…`: where a self-hosted relay is and a group key that
//! makes any computer that presents it an owner of the relay, until the
//! code is deleted on the relay. The relay prints one on start; more can be
//! created in its management screen.
//!
//! Payload: version 1, URL count, per URL (length, UTF-8), 16-byte claim key.
//! Up to 4 URLs, so a relay reachable as both an IPv4 and an IPv6 address
//! can list both; with a domain name one URL is enough.

use iroh_base::RelayUrl;

use crate::{Error, text, wire::Reader};

pub const PREFIX: &str = "rcr1";
const VERSION: u8 = 1;
const MAX_URLS: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayCode {
    pub urls: Vec<RelayUrl>,
    pub claim_key: [u8; 16],
}

impl RelayCode {
    pub fn encode(&self) -> String {
        assert!(!self.urls.is_empty() && self.urls.len() <= MAX_URLS, "1 to {MAX_URLS} relay URLs");
        let mut v = vec![VERSION, self.urls.len() as u8];
        for u in &self.urls {
            let s = u.to_string();
            assert!(s.len() <= 255, "relay URL too long");
            v.push(s.len() as u8);
            v.extend_from_slice(s.as_bytes());
        }
        v.extend_from_slice(&self.claim_key);
        text::encode(PREFIX, &v)
    }

    pub fn decode(code: &str) -> Result<RelayCode, Error> {
        let payload = text::decode(PREFIX, code)?;
        let mut r = Reader::new(&payload);
        match r.u8()? {
            VERSION => {}
            v => return Err(Error::Version(v)),
        }
        let n = r.u8()? as usize;
        if n == 0 || n > MAX_URLS {
            return Err(Error::Field("relay URL count"));
        }
        let mut urls = Vec::with_capacity(n);
        for _ in 0..n {
            let len = r.u8()? as usize;
            let s = std::str::from_utf8(r.take(len)?).map_err(|_| Error::Field("relay URL"))?;
            urls.push(s.parse().map_err(|_| Error::Field("relay URL"))?);
        }
        let claim_key = r.array()?;
        r.finish()?;
        Ok(RelayCode { urls, claim_key })
    }

    /// The first relay code in `text`.
    pub fn find(text: &str) -> Option<RelayCode> {
        RelayCode::decode(text::find(PREFIX, text)?).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let c = RelayCode {
            urls: vec![
                "https://203.0.113.7:7843".parse().unwrap(),
                "https://[2001:db8::7]:7843".parse().unwrap(),
            ],
            claim_key: [0x5a; 16],
        };
        let s = c.encode();
        assert!(s.starts_with(PREFIX));
        assert_eq!(RelayCode::decode(&s).unwrap(), c);
        assert_eq!(RelayCode::find(&format!("中转码：{s}")), Some(c.clone()));
        let one = RelayCode { urls: vec!["https://relay.example.com".parse().unwrap()], claim_key: [1; 16] };
        assert_eq!(RelayCode::decode(&one.encode()).unwrap(), one);
    }

    #[test]
    fn rejects_bad_payloads() {
        let enc = |v: &[u8]| text::encode(PREFIX, v);
        assert_eq!(RelayCode::decode(&enc(&[2, 1])), Err(Error::Version(2)));
        assert_eq!(RelayCode::decode(&enc(&[1, 0])), Err(Error::Field("relay URL count")));
        let mut v = vec![1, 1, 3];
        v.extend_from_slice(b"abc");
        v.extend_from_slice(&[0; 16]);
        assert_eq!(RelayCode::decode(&enc(&v)), Err(Error::Field("relay URL")));
        let mut v = vec![1, 1, 17];
        v.extend_from_slice(b"https://a.example");
        v.extend_from_slice(&[0; 15]);
        assert_eq!(RelayCode::decode(&enc(&v)), Err(Error::Length));
        v.extend_from_slice(&[0; 2]);
        assert_eq!(RelayCode::decode(&enc(&v)), Err(Error::Length));
        assert_eq!(RelayCode::decode("rcd1abc"), Err(Error::WrongPrefix));
    }
}
