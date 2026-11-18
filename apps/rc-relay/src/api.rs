//! The HTTPS calls the relay answers next to `/relay`, on the same port.
//! The request rides in the `x-rc-request` header (the server's route
//! handlers cannot read a body), as a checked Base58 code.
//!
//! - `POST /rc/claim`: the key of one of the relay's codes, the device key
//!   that becomes an owner, the device's name, and that key's signature. A
//!   code works any number of times, until it is deleted on the relay.
//! - `POST /rc/guests`: an owner's whole guest list (the devices it lets
//!   use the relay to reach it), replacing the one it sent before.
//! - `POST /rc/invites`: an owner's connection codes not used yet (their
//!   tokens and expiry), replacing the ones it sent before.
//! - `POST /rc/meet`: a device with one of those codes, signed by that
//!   device: the relay uses the invite up and lets the device and the owner
//!   reach each other for [`MEET_KEEP`], until the owner lists it as a guest.
//!
//! Guests and invites are signed by the owner with a time and a
//! random nonce; the relay takes such a request only within [`MAX_SKEW`] of
//! its own clock and only once.
//!
//! Answers are plain text: `ok`, else one of the [`Refusal`] words.

use iroh_base::{PublicKey, SecretKey, Signature};
use rc_ident::{Error, text, wire::Reader};

pub const CLAIM_PATH: &str = "/rc/claim";
pub const GUESTS_PATH: &str = "/rc/guests";
pub const INVITES_PATH: &str = "/rc/invites";
pub const MEET_PATH: &str = "/rc/meet";
pub const HEADER: &str = "x-rc-request";
/// How far the time in a signed request may be from the relay's clock (seconds).
pub const MAX_SKEW: u64 = 300;
/// Most guests one owner may list (keeps the request header small).
pub const MAX_GUESTS: usize = 256;
/// Most unused connection codes one owner may have on the relay.
pub const MAX_INVITES: usize = 64;
/// How long a device that brought an invite and the owner may reach each
/// other through the relay without the owner listing it (seconds).
pub const MEET_KEEP: u64 = 600;

const CLAIM_PREFIX: &str = "rcqc1";
const GUESTS_PREFIX: &str = "rcqg1";
const INVITES_PREFIX: &str = "rcqi1";
const MEET_PREFIX: &str = "rcqm1";
const VERSION: u8 = 1;
const CLAIM_DOMAIN: &[u8] = b"remote-control relay claim v1\0";
const GUESTS_DOMAIN: &[u8] = b"remote-control relay guests v1\0";
const INVITES_DOMAIN: &[u8] = b"remote-control relay invites v1\0";
const MEET_DOMAIN: &[u8] = b"remote-control relay meet v1\0";

/// A random claim key or nonce.
pub fn random_key() -> [u8; 16] {
    let mut k = [0; 16];
    getrandom::fill(&mut k).expect("system random numbers");
    k
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    BadRequest,
    /// No code of the relay has that key (never had, or it was deleted).
    UnknownKey,
    /// No such unused connection code of that owner on the relay.
    UnknownInvite,
    NotOwner,
    /// The request's time is too far from the relay's clock.
    Clock,
    /// The same signed request came before.
    Replay,
    Server,
}

impl Refusal {
    pub fn word(self) -> &'static str {
        match self {
            Refusal::BadRequest => "bad-request",
            Refusal::UnknownKey => "unknown-key",
            Refusal::UnknownInvite => "unknown-invite",
            Refusal::NotOwner => "not-owner",
            Refusal::Clock => "clock",
            Refusal::Replay => "replay",
            Refusal::Server => "server",
        }
    }

    pub fn from_word(w: &str) -> Option<Refusal> {
        [
            Refusal::BadRequest,
            Refusal::UnknownKey,
            Refusal::UnknownInvite,
            Refusal::NotOwner,
            Refusal::Clock,
            Refusal::Replay,
            Refusal::Server,
        ]
        .into_iter()
            .find(|r| r.word() == w.trim())
    }

    pub fn status(self) -> http::StatusCode {
        match self {
            Refusal::BadRequest => http::StatusCode::BAD_REQUEST,
            Refusal::Server => http::StatusCode::INTERNAL_SERVER_ERROR,
            _ => http::StatusCode::FORBIDDEN,
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Refusal::BadRequest => "the relay did not understand the request",
            Refusal::UnknownKey => "the relay code was deleted or is not from this relay",
            Refusal::UnknownInvite => "the connection code is used up, expired or unknown to the relay",
            Refusal::NotOwner => "this device does not own the relay",
            Refusal::Clock => "this computer's clock is too far off the relay's",
            Refusal::Replay => "the relay saw this request before",
            Refusal::Server => "the relay could not save the change",
        })
    }
}

impl std::error::Error for Refusal {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimRequest {
    pub claim_key: [u8; 16],
    pub device: PublicKey,
    /// The computer's name, shown on the relay (empty from older versions).
    pub name: String,
    pub signature: Signature,
}

/// Version 1 has no name; version 2 ends with one (a length byte, UTF-8).
const CLAIM_NAMED: u8 = 2;
/// Longest name kept (bytes).
pub const MAX_NAME: usize = 64;

fn claim_signed(version: u8, claim_key: &[u8; 16], device: &PublicKey, name: &str) -> Vec<u8> {
    let mut v = [CLAIM_DOMAIN, &[version], claim_key, device.as_bytes()].concat();
    if version == CLAIM_NAMED {
        v.push(name.len() as u8);
        v.extend_from_slice(name.as_bytes());
    }
    v
}

/// `name` cut to `max` bytes on a character boundary.
fn cut(name: &str, max: usize) -> String {
    let mut n = name.len().min(max);
    while !name.is_char_boundary(n) {
        n -= 1;
    }
    name[..n].to_string()
}

impl ClaimRequest {
    pub fn new(claim_key: [u8; 16], device: &SecretKey, name: &str) -> ClaimRequest {
        let name = cut(name, MAX_NAME);
        let signature = device.sign(&claim_signed(CLAIM_NAMED, &claim_key, &device.public(), &name));
        ClaimRequest { claim_key, device: device.public(), name, signature }
    }

    pub fn encode(&self) -> String {
        let mut v = [&[CLAIM_NAMED][..], &self.claim_key, self.device.as_bytes(), &self.signature.to_bytes()].concat();
        v.push(self.name.len() as u8);
        v.extend_from_slice(self.name.as_bytes());
        text::encode(CLAIM_PREFIX, &v)
    }

    /// Decodes and checks the signature.
    pub fn decode(s: &str) -> Result<ClaimRequest, Error> {
        let p = text::decode(CLAIM_PREFIX, s)?;
        let mut r = Reader::new(&p);
        let version = match r.u8()? {
            v @ (VERSION | CLAIM_NAMED) => v,
            v => return Err(Error::Version(v)),
        };
        let claim_key = r.array()?;
        let device = PublicKey::from_bytes(&r.array()?).map_err(|_| Error::Field("device key"))?;
        let signature = Signature::from_bytes(&r.array()?);
        let name = if version == CLAIM_NAMED {
            let n = r.u8()? as usize;
            if n > MAX_NAME {
                return Err(Error::Field("name"));
            }
            std::str::from_utf8(r.take(n)?).map_err(|_| Error::Field("name"))?.to_string()
        } else {
            String::new()
        };
        r.finish()?;
        device.verify(&claim_signed(version, &claim_key, &device, &name), &signature).map_err(|_| Error::Signature)?;
        Ok(ClaimRequest { claim_key, device, name, signature })
    }
}

/// A device an owner lets use the relay.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Guest {
    pub id: PublicKey,
    pub name: String,
}

impl From<PublicKey> for Guest {
    fn from(id: PublicKey) -> Guest {
        Guest { id, name: String::new() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestsRequest {
    /// The owner.
    pub device: PublicKey,
    /// Unix seconds.
    pub time: u64,
    pub nonce: [u8; 16],
    /// The devices the owner lets use the relay.
    pub guests: Vec<Guest>,
    pub signature: Signature,
}

/// Version 1 lists keys only. Version 2 (older apps) adds a byte after each
/// key, which marked "my computer"; it is read and left unused. Version 3
/// adds each guest's name (length byte, UTF-8).
const GUESTS_MARKED: u8 = 2;
const GUESTS_NAMED: u8 = 3;
/// Longest guest name kept (bytes, keeps the header compact).
pub const MAX_GUEST_NAME: usize = 32;

fn guests_signed(version: u8, device: &PublicKey, time: u64, nonce: &[u8; 16], guests: &[Guest], marks: &[u8]) -> Vec<u8> {
    let mut v = [GUESTS_DOMAIN, &[version], device.as_bytes(), &time.to_be_bytes(), nonce].concat();
    for (i, g) in guests.iter().enumerate() {
        v.extend_from_slice(g.id.as_bytes());
        match version {
            GUESTS_MARKED => v.push(marks[i]),
            GUESTS_NAMED => {
                v.push(g.name.len() as u8);
                v.extend_from_slice(g.name.as_bytes());
            }
            _ => {}
        }
    }
    v
}

impl GuestsRequest {
    /// At most [`MAX_GUESTS`] of `guests` go in.
    pub fn new(device: &SecretKey, time: u64, guests: &[Guest]) -> GuestsRequest {
        let guests: Vec<Guest> = guests
            .iter()
            .take(MAX_GUESTS)
            .map(|g| Guest { id: g.id, name: cut(&g.name, MAX_GUEST_NAME) })
            .collect();
        let nonce = random_key();
        let version = Self::version_for(&guests);
        let signature = device.sign(&guests_signed(version, &device.public(), time, &nonce, &guests, &[]));
        GuestsRequest { device: device.public(), time, nonce, guests, signature }
    }

    fn version_for(guests: &[Guest]) -> u8 {
        if guests.iter().any(|g| !g.name.is_empty()) { GUESTS_NAMED } else { VERSION }
    }

    pub fn encode(&self) -> String {
        let version = Self::version_for(&self.guests);
        let mut v = [&[version][..], self.device.as_bytes(), &self.time.to_be_bytes(), &self.nonce, &self.signature.to_bytes()]
            .concat();
        for g in &self.guests {
            v.extend_from_slice(g.id.as_bytes());
            if version == GUESTS_NAMED {
                v.push(g.name.len() as u8);
                v.extend_from_slice(g.name.as_bytes());
            }
        }
        text::encode(GUESTS_PREFIX, &v)
    }

    /// Decodes and checks the signature (not the time).
    pub fn decode(s: &str) -> Result<GuestsRequest, Error> {
        let p = text::decode(GUESTS_PREFIX, s)?;
        let mut r = Reader::new(&p);
        let version = match r.u8()? {
            v @ (VERSION | GUESTS_MARKED | GUESTS_NAMED) => v,
            v => return Err(Error::Version(v)),
        };
        let device = PublicKey::from_bytes(&r.array()?).map_err(|_| Error::Field("device key"))?;
        let time = r.u64()?;
        let nonce = r.array()?;
        let signature = Signature::from_bytes(&r.array()?);
        let (mut guests, mut marks) = (Vec::new(), Vec::new());
        while let Ok(g) = r.array::<32>() {
            if guests.len() == MAX_GUESTS {
                return Err(Error::Length);
            }
            let id = PublicKey::from_bytes(&g).map_err(|_| Error::Field("guest key"))?;
            let name = match version {
                GUESTS_MARKED => {
                    marks.push(r.u8()?);
                    String::new()
                }
                GUESTS_NAMED => {
                    let n = r.u8()? as usize;
                    if n > MAX_GUEST_NAME {
                        return Err(Error::Field("guest name"));
                    }
                    std::str::from_utf8(r.take(n)?).map_err(|_| Error::Field("guest name"))?.to_string()
                }
                _ => String::new(),
            };
            guests.push(Guest { id, name });
        }
        r.finish()?;
        device
            .verify(&guests_signed(version, &device, time, &nonce, &guests, &marks), &signature)
            .map_err(|_| Error::Signature)?;
        Ok(GuestsRequest { device, time, nonce, guests, signature })
    }
}

/// A connection code an owner gave out and nobody used yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Invite {
    pub token: [u8; 8],
    /// Unix seconds.
    pub expires: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvitesRequest {
    /// The owner.
    pub device: PublicKey,
    /// Unix seconds.
    pub time: u64,
    pub nonce: [u8; 16],
    pub invites: Vec<Invite>,
    pub signature: Signature,
}

fn invites_signed(device: &PublicKey, time: u64, nonce: &[u8; 16], invites: &[Invite]) -> Vec<u8> {
    let mut v = [INVITES_DOMAIN, &[VERSION], device.as_bytes(), &time.to_be_bytes(), nonce].concat();
    for i in invites {
        v.extend_from_slice(&i.token);
        v.extend_from_slice(&i.expires.to_be_bytes());
    }
    v
}

impl InvitesRequest {
    /// At most [`MAX_INVITES`] of `invites` go in.
    pub fn new(device: &SecretKey, time: u64, invites: &[Invite]) -> InvitesRequest {
        let invites = invites[..invites.len().min(MAX_INVITES)].to_vec();
        let nonce = random_key();
        let signature = device.sign(&invites_signed(&device.public(), time, &nonce, &invites));
        InvitesRequest { device: device.public(), time, nonce, invites, signature }
    }

    pub fn encode(&self) -> String {
        let mut v = [&[VERSION][..], self.device.as_bytes(), &self.time.to_be_bytes(), &self.nonce, &self.signature.to_bytes()]
            .concat();
        for i in &self.invites {
            v.extend_from_slice(&i.token);
            v.extend_from_slice(&i.expires.to_be_bytes());
        }
        text::encode(INVITES_PREFIX, &v)
    }

    /// Decodes and checks the signature (not the time).
    pub fn decode(s: &str) -> Result<InvitesRequest, Error> {
        let p = text::decode(INVITES_PREFIX, s)?;
        let mut r = Reader::new(&p);
        match r.u8()? {
            VERSION => {}
            v => return Err(Error::Version(v)),
        }
        let device = PublicKey::from_bytes(&r.array()?).map_err(|_| Error::Field("device key"))?;
        let time = r.u64()?;
        let nonce = r.array()?;
        let signature = Signature::from_bytes(&r.array()?);
        let mut invites = Vec::new();
        while let Ok(token) = r.array::<8>() {
            if invites.len() == MAX_INVITES {
                return Err(Error::Length);
            }
            invites.push(Invite { token, expires: r.u64()? });
        }
        r.finish()?;
        device.verify(&invites_signed(&device, time, &nonce, &invites), &signature).map_err(|_| Error::Signature)?;
        Ok(InvitesRequest { device, time, nonce, invites, signature })
    }
}

/// A device with a connection code of `owner` (its token), signed by the device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeetRequest {
    pub device: PublicKey,
    pub owner: PublicKey,
    pub token: [u8; 8],
    /// The guest's name, shown on the relay (empty from older versions).
    pub name: String,
    pub signature: Signature,
}

const MEET_NAMED: u8 = 2;

fn meet_signed(version: u8, device: &PublicKey, owner: &PublicKey, token: &[u8; 8], name: &str) -> Vec<u8> {
    let mut v = [MEET_DOMAIN, &[version], device.as_bytes(), owner.as_bytes(), token].concat();
    if version == MEET_NAMED {
        v.push(name.len() as u8);
        v.extend_from_slice(name.as_bytes());
    }
    v
}

impl MeetRequest {
    pub fn new(device: &SecretKey, owner: PublicKey, token: [u8; 8], name: &str) -> MeetRequest {
        let name = cut(name, MAX_NAME);
        let version = Self::version_for(&name);
        let signature = device.sign(&meet_signed(version, &device.public(), &owner, &token, &name));
        MeetRequest { device: device.public(), owner, token, name, signature }
    }

    fn version_for(name: &str) -> u8 {
        if name.is_empty() { VERSION } else { MEET_NAMED }
    }

    pub fn encode(&self) -> String {
        let version = Self::version_for(&self.name);
        let mut v = [&[version][..], self.device.as_bytes(), self.owner.as_bytes(), &self.token, &self.signature.to_bytes()]
            .concat();
        if version == MEET_NAMED {
            v.push(self.name.len() as u8);
            v.extend_from_slice(self.name.as_bytes());
        }
        text::encode(MEET_PREFIX, &v)
    }

    /// Decodes and checks the signature.
    pub fn decode(s: &str) -> Result<MeetRequest, Error> {
        let p = text::decode(MEET_PREFIX, s)?;
        let mut r = Reader::new(&p);
        let version = match r.u8()? {
            v @ (VERSION | MEET_NAMED) => v,
            v => return Err(Error::Version(v)),
        };
        let device = PublicKey::from_bytes(&r.array()?).map_err(|_| Error::Field("device key"))?;
        let owner = PublicKey::from_bytes(&r.array()?).map_err(|_| Error::Field("owner key"))?;
        let token = r.array()?;
        let signature = Signature::from_bytes(&r.array()?);
        let name = if version == MEET_NAMED {
            let n = r.u8()? as usize;
            if n > MAX_NAME {
                return Err(Error::Field("name"));
            }
            std::str::from_utf8(r.take(n)?).map_err(|_| Error::Field("name"))?.to_string()
        } else {
            String::new()
        };
        r.finish()?;
        device
            .verify(&meet_signed(version, &device, &owner, &token, &name), &signature)
            .map_err(|_| Error::Signature)?;
        Ok(MeetRequest { device, owner, token, name, signature })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> SecretKey {
        SecretKey::from_bytes(&[n; 32])
    }

    #[test]
    fn claim_round_trip_and_forgery() {
        let r = ClaimRequest::new([9; 16], &key(1), "书房");
        assert_eq!(ClaimRequest::decode(&r.encode()).unwrap(), r);
        // Someone else's key with a signature not made by it.
        let mut f = ClaimRequest::new([9; 16], &key(2), "书房");
        f.device = key(1).public();
        assert_eq!(ClaimRequest::decode(&f.encode()), Err(Error::Signature));
        // Same signature, another claim key or another name.
        let mut f = r.clone();
        f.claim_key = [8; 16];
        assert_eq!(ClaimRequest::decode(&f.encode()), Err(Error::Signature));
        let mut f = r.clone();
        f.name = "客厅".into();
        assert_eq!(ClaimRequest::decode(&f.encode()), Err(Error::Signature));
        // A long name is cut on a character boundary.
        let long = ClaimRequest::new([9; 16], &key(1), &"设".repeat(40));
        assert_eq!(long.name, "设".repeat(21));
        assert_eq!(ClaimRequest::decode(&long.encode()).unwrap(), long);
    }

    #[test]
    fn claim_from_an_older_version_has_no_name() {
        let (k, ck) = (key(1), [9; 16]);
        let signature = k.sign(&[CLAIM_DOMAIN, &[VERSION], &ck, k.public().as_bytes()].concat());
        let v = [&[VERSION][..], &ck, k.public().as_bytes(), &signature.to_bytes()].concat();
        let r = ClaimRequest::decode(&text::encode(CLAIM_PREFIX, &v)).unwrap();
        assert_eq!((r.device, r.name.as_str()), (k.public(), ""));
    }

    #[test]
    fn guests_round_trip_and_forgery() {
        let guests = [Guest { id: key(2).public(), name: "friend-pc".into() }, Guest::from(key(3).public())];
        let r = GuestsRequest::new(&key(1), 1_000, &guests);
        assert_eq!(GuestsRequest::decode(&r.encode()).unwrap(), r);
        let empty = GuestsRequest::new(&key(1), 1_000, &[]);
        assert_eq!(GuestsRequest::decode(&empty.encode()).unwrap().guests, []);
        // A guest slipped in or renamed after signing.
        let mut f = r.clone();
        f.guests.push(Guest::from(key(4).public()));
        assert_eq!(GuestsRequest::decode(&f.encode()), Err(Error::Signature));
        let mut f = r.clone();
        f.guests[0].name = "other".into();
        assert_eq!(GuestsRequest::decode(&f.encode()), Err(Error::Signature));
        let many: Vec<Guest> = (0..MAX_GUESTS + 5).map(|_| Guest::from(SecretKey::generate().public())).collect();
        assert_eq!(GuestsRequest::new(&key(1), 1_000, &many).guests.len(), MAX_GUESTS);
        assert_eq!(GuestsRequest::decode(&ClaimRequest::new([1; 16], &key(1), "").encode()), Err(Error::WrongPrefix));
        // Without names, version 1 is used (compact and older-relay compatible).
        let plain = GuestsRequest::new(&key(1), 1_000, &[Guest::from(key(2).public())]);
        assert_eq!(text::decode(GUESTS_PREFIX, &plain.encode()).unwrap()[0], VERSION);
    }

    #[test]
    fn guests_from_an_older_version_with_marks() {
        let (k, nonce) = (key(1), [3; 16]);
        let keys = [key(2).public(), key(3).public()];
        let guests: Vec<Guest> = keys.into_iter().map(Guest::from).collect();
        let signed = guests_signed(GUESTS_MARKED, &k.public(), 1_000, &nonce, &guests, &[1, 0]);
        let mut v = [&[GUESTS_MARKED][..], k.public().as_bytes(), &1_000u64.to_be_bytes(), &nonce, &k.sign(&signed).to_bytes()].concat();
        for (g, m) in guests.iter().zip([1u8, 0]) {
            v.extend_from_slice(g.id.as_bytes());
            v.push(m);
        }
        assert_eq!(GuestsRequest::decode(&text::encode(GUESTS_PREFIX, &v)).unwrap().guests, guests);
        // A mark changed after signing.
        let n = v.len();
        v[n - 1] = 1;
        assert_eq!(GuestsRequest::decode(&text::encode(GUESTS_PREFIX, &v)), Err(Error::Signature));
    }

    #[test]
    fn invites_round_trip_and_forgery() {
        let invites = [Invite { token: [1; 8], expires: 5_000 }, Invite { token: [2; 8], expires: 6_000 }];
        let r = InvitesRequest::new(&key(1), 1_000, &invites);
        assert_eq!(InvitesRequest::decode(&r.encode()).unwrap(), r);
        assert_eq!(InvitesRequest::decode(&InvitesRequest::new(&key(1), 1_000, &[]).encode()).unwrap().invites, []);
        // A later expiry slipped in after signing.
        let mut f = r.clone();
        f.invites[0].expires = 9_000;
        assert_eq!(InvitesRequest::decode(&f.encode()), Err(Error::Signature));
        let many = vec![Invite { token: [3; 8], expires: 1 }; MAX_INVITES + 3];
        assert_eq!(InvitesRequest::new(&key(1), 1_000, &many).invites.len(), MAX_INVITES);
        assert_eq!(InvitesRequest::decode(&GuestsRequest::new(&key(1), 1_000, &[]).encode()), Err(Error::WrongPrefix));
    }

    #[test]
    fn meet_round_trip_and_forgery() {
        let r = MeetRequest::new(&key(2), key(1).public(), [7; 8], "friend-pc");
        assert_eq!(MeetRequest::decode(&r.encode()).unwrap(), r);
        // Without a name, version 1 is used.
        let plain = MeetRequest::new(&key(2), key(1).public(), [7; 8], "");
        assert_eq!(MeetRequest::decode(&plain.encode()).unwrap(), plain);
        assert_eq!(text::decode(MEET_PREFIX, &plain.encode()).unwrap()[0], VERSION);
        // Someone else's device named in a request it did not sign.
        let mut f = MeetRequest::new(&key(3), key(1).public(), [7; 8], "friend-pc");
        f.device = key(2).public();
        assert_eq!(MeetRequest::decode(&f.encode()), Err(Error::Signature));
        let mut f = r.clone();
        f.token = [8; 8];
        assert_eq!(MeetRequest::decode(&f.encode()), Err(Error::Signature));
        let mut f = r.clone();
        f.name = "other".into();
        assert_eq!(MeetRequest::decode(&f.encode()), Err(Error::Signature));
    }

    #[test]
    fn refusal_words_round_trip() {
        for r in [
            Refusal::BadRequest,
            Refusal::UnknownKey,
            Refusal::UnknownInvite,
            Refusal::NotOwner,
            Refusal::Clock,
            Refusal::Replay,
            Refusal::Server,
        ] {
            assert_eq!(Refusal::from_word(r.word()), Some(r));
        }
        assert_eq!(Refusal::from_word("nope"), None);
    }
}
