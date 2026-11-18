//! Device proof `rcd1…`: "the endpoint with key `ephemeral` speaks for
//! device `device` until `expires`", signed by the device key. A session
//! window connects with a key of its own while the device key stays online
//! in one process; peers and relays that check the proof treat the session
//! as the device. The verifier must also check that the connection really
//! comes from `ephemeral` (iroh and the relay handshake prove that).
//!
//! Payload: version 1, device key, ephemeral key, expiry (Unix seconds, big
//! endian), Ed25519 signature over [`DOMAIN`] and the fields before it.

use std::time::{SystemTime, UNIX_EPOCH};

use iroh_base::{PublicKey, SecretKey, Signature};

use crate::{Error, text, wire::Reader};

pub const PREFIX: &str = "rcd1";
const VERSION: u8 = 1;
/// Keeps a proof signature from being valid as any other signed message.
const DOMAIN: &[u8] = b"remote-control device proof v1\0";
const SIGNED_LEN: usize = 1 + 32 + 32 + 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceProof {
    pub device: PublicKey,
    pub ephemeral: PublicKey,
    /// Unix seconds.
    pub expires: u64,
    pub signature: Signature,
}

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn signed_part(device: &PublicKey, ephemeral: &PublicKey, expires: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(SIGNED_LEN);
    v.push(VERSION);
    v.extend_from_slice(device.as_bytes());
    v.extend_from_slice(ephemeral.as_bytes());
    v.extend_from_slice(&expires.to_be_bytes());
    v
}

fn message(signed: &[u8]) -> Vec<u8> {
    [DOMAIN, signed].concat()
}

impl DeviceProof {
    pub fn issue(device: &SecretKey, ephemeral: PublicKey, expires: u64) -> DeviceProof {
        let signed = signed_part(&device.public(), &ephemeral, expires);
        DeviceProof { device: device.public(), ephemeral, expires, signature: device.sign(&message(&signed)) }
    }

    pub fn encode(&self) -> String {
        let mut v = signed_part(&self.device, &self.ephemeral, self.expires);
        v.extend_from_slice(&self.signature.to_bytes());
        text::encode(PREFIX, &v)
    }

    /// Parses without checking the signature or the expiry (see [`Self::verify`]).
    pub fn decode(code: &str) -> Result<DeviceProof, Error> {
        let payload = text::decode(PREFIX, code)?;
        let mut r = Reader::new(&payload);
        match r.u8()? {
            VERSION => {}
            v => return Err(Error::Version(v)),
        }
        let key = |b: [u8; 32], name| PublicKey::from_bytes(&b).map_err(|_| Error::Field(name));
        let device = key(r.array()?, "device key")?;
        let ephemeral = key(r.array()?, "ephemeral key")?;
        let expires = r.u64()?;
        let signature = Signature::from_bytes(&r.array()?);
        r.finish()?;
        Ok(DeviceProof { device, ephemeral, expires, signature })
    }

    /// The signature is the device's and `now` (Unix seconds) is before the expiry.
    pub fn verify(&self, now: u64) -> Result<(), Error> {
        let signed = signed_part(&self.device, &self.ephemeral, self.expires);
        self.device.verify(&message(&signed), &self.signature).map_err(|_| Error::Signature)?;
        if now >= self.expires {
            return Err(Error::Expired);
        }
        Ok(())
    }

    /// Decodes and verifies; `connected_as` is the key the connection was made with.
    pub fn check(code: &str, connected_as: &PublicKey, now: u64) -> Result<DeviceProof, Error> {
        let p = DeviceProof::decode(code)?;
        p.verify(now)?;
        if p.ephemeral != *connected_as {
            return Err(Error::Field("ephemeral key"));
        }
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> SecretKey {
        SecretKey::from_bytes(&[n; 32])
    }

    #[test]
    fn issue_encode_check() {
        let (dev, eph) = (key(1), key(2));
        let p = DeviceProof::issue(&dev, eph.public(), 1_000);
        let code = p.encode();
        assert!(code.starts_with(PREFIX));
        assert_eq!(DeviceProof::decode(&code).unwrap(), p);
        assert_eq!(DeviceProof::check(&code, &eph.public(), 999).unwrap().device, dev.public());
        assert_eq!(DeviceProof::check(&code, &eph.public(), 1_000), Err(Error::Expired));
        // Shown by another connection than the one it was made for.
        assert_eq!(DeviceProof::check(&code, &key(3).public(), 999), Err(Error::Field("ephemeral key")));
    }

    #[test]
    fn forged_or_altered_proofs_fail() {
        let (dev, eph, other) = (key(1), key(2), key(3));
        // Signed by a key that is not the device's.
        let mut p = DeviceProof::issue(&other, eph.public(), 1_000);
        p.device = dev.public();
        assert_eq!(DeviceProof::decode(&p.encode()).unwrap().verify(0), Err(Error::Signature));
        // A longer expiry than the device signed.
        let mut p = DeviceProof::issue(&dev, eph.public(), 1_000);
        p.expires = 2_000;
        assert_eq!(DeviceProof::decode(&p.encode()).unwrap().verify(0), Err(Error::Signature));
        // Handed to another ephemeral key.
        let mut p = DeviceProof::issue(&dev, eph.public(), 1_000);
        p.ephemeral = other.public();
        assert_eq!(DeviceProof::check(&p.encode(), &other.public(), 0), Err(Error::Signature));
        // One character changed in the text.
        let code = DeviceProof::issue(&dev, eph.public(), 1_000).encode();
        let mut chars: Vec<char> = code.chars().collect();
        let i = chars.len() / 2;
        chars[i] = if chars[i] == 'z' { 'y' } else { 'z' };
        let s: String = chars.into_iter().collect();
        assert!(DeviceProof::check(&s, &eph.public(), 0).is_err());
    }

    #[test]
    fn signature_is_bound_to_the_domain() {
        let dev = key(1);
        let signed = signed_part(&dev.public(), &key(2).public(), 1_000);
        // A signature over the bare fields (no domain) is not a proof.
        let p = DeviceProof { device: dev.public(), ephemeral: key(2).public(), expires: 1_000, signature: dev.sign(&signed) };
        assert_eq!(p.verify(0), Err(Error::Signature));
    }
}
