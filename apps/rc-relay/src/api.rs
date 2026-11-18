//! The HTTPS calls the relay answers next to `/relay`, on the same port.
//! The request rides in the `x-rc-request` header (the server's route
//! handlers cannot read a body), as a checked Base58 code.
//!
//! - `POST /rc/claim`: a claim key from a relay code, the device key that
//!   becomes an owner, and that key's signature. Uses the claim key up.
//! - `POST /rc/add-owner`: an owner asks for a new relay code for another
//!   of its computers.
//! - `POST /rc/guests`: an owner's whole guest list (the devices it lets
//!   use the relay to reach it), replacing the one it sent before. A guest
//!   marked `mine` (the owner's "my computer") owns the relay too.
//! - `POST /rc/invites`: an owner's connection codes not used yet (their
//!   tokens and expiry), replacing the ones it sent before.
//! - `POST /rc/meet`: a device with one of those codes, signed by that
//!   device: the relay uses the invite up and lets the device and the owner
//!   reach each other for [`MEET_KEEP`], until the owner lists it as a guest.
//!
//! Add-owner, guests and invites are signed by the owner with a time and a
//! random nonce; the relay takes such a request only within [`MAX_SKEW`] of
//! its own clock and only once.
//!
//! Answers are plain text: `ok` or a relay code, else one of the [`Refusal`] words.

use iroh_base::{PublicKey, SecretKey, Signature};
use rc_ident::{Error, text, wire::Reader};

pub const CLAIM_PATH: &str = "/rc/claim";
pub const ADD_OWNER_PATH: &str = "/rc/add-owner";
pub const GUESTS_PATH: &str = "/rc/guests";
pub const INVITES_PATH: &str = "/rc/invites";
pub const MEET_PATH: &str = "/rc/meet";
pub const HEADER: &str = "x-rc-request";
/// How far the time in a signed request may be from the relay's clock (seconds).
pub const MAX_SKEW: u64 = 300;
/// How long a relay code from add-owner stays valid (seconds).
pub const ADDED_CODE_LIFETIME: u64 = 24 * 3600;
/// Most guests one owner may list (keeps the request header small).
pub const MAX_GUESTS: usize = 256;
/// Most unused connection codes one owner may have on the relay.
pub const MAX_INVITES: usize = 64;
/// How long a device that brought an invite and the owner may reach each
/// other through the relay without the owner listing it (seconds).
pub const MEET_KEEP: u64 = 600;

const CLAIM_PREFIX: &str = "rcqc1";
const ADD_PREFIX: &str = "rcqa1";
const GUESTS_PREFIX: &str = "rcqg1";
const INVITES_PREFIX: &str = "rcqi1";
const MEET_PREFIX: &str = "rcqm1";
const VERSION: u8 = 1;
const CLAIM_DOMAIN: &[u8] = b"remote-control relay claim v1\0";
const ADD_DOMAIN: &[u8] = b"remote-control relay add owner v1\0";
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
    /// The claim key is unknown, used already or expired.
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
            Refusal::UnknownKey => "the relay code is used up, expired or not from this relay",
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
    pub signature: Signature,
}

fn claim_signed(claim_key: &[u8; 16], device: &PublicKey) -> Vec<u8> {
    [CLAIM_DOMAIN, &[VERSION], claim_key, device.as_bytes()].concat()
}

impl ClaimRequest {
    pub fn new(claim_key: [u8; 16], device: &SecretKey) -> ClaimRequest {
        let signature = device.sign(&claim_signed(&claim_key, &device.public()));
        ClaimRequest { claim_key, device: device.public(), signature }
    }

    pub fn encode(&self) -> String {
        let v = [&[VERSION][..], &self.claim_key, self.device.as_bytes(), &self.signature.to_bytes()].concat();
        text::encode(CLAIM_PREFIX, &v)
    }

    /// Decodes and checks the signature.
    pub fn decode(s: &str) -> Result<ClaimRequest, Error> {
        let p = text::decode(CLAIM_PREFIX, s)?;
        let mut r = Reader::new(&p);
        match r.u8()? {
            VERSION => {}
            v => return Err(Error::Version(v)),
        }
        let claim_key = r.array()?;
        let device = PublicKey::from_bytes(&r.array()?).map_err(|_| Error::Field("device key"))?;
        let signature = Signature::from_bytes(&r.array()?);
        r.finish()?;
        device.verify(&claim_signed(&claim_key, &device), &signature).map_err(|_| Error::Signature)?;
        Ok(ClaimRequest { claim_key, device, signature })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddOwnerRequest {
    pub device: PublicKey,
    /// Unix seconds.
    pub time: u64,
    pub nonce: [u8; 16],
    pub signature: Signature,
}

fn add_signed(device: &PublicKey, time: u64, nonce: &[u8; 16]) -> Vec<u8> {
    [ADD_DOMAIN, &[VERSION], device.as_bytes(), &time.to_be_bytes(), nonce].concat()
}

impl AddOwnerRequest {
    pub fn new(device: &SecretKey, time: u64) -> AddOwnerRequest {
        let nonce = random_key();
        let signature = device.sign(&add_signed(&device.public(), time, &nonce));
        AddOwnerRequest { device: device.public(), time, nonce, signature }
    }

    pub fn encode(&self) -> String {
        let v =
            [&[VERSION][..], self.device.as_bytes(), &self.time.to_be_bytes(), &self.nonce, &self.signature.to_bytes()]
                .concat();
        text::encode(ADD_PREFIX, &v)
    }

    /// Decodes and checks the signature (not the time).
    pub fn decode(s: &str) -> Result<AddOwnerRequest, Error> {
        let p = text::decode(ADD_PREFIX, s)?;
        let mut r = Reader::new(&p);
        match r.u8()? {
            VERSION => {}
            v => return Err(Error::Version(v)),
        }
        let device = PublicKey::from_bytes(&r.array()?).map_err(|_| Error::Field("device key"))?;
        let time = r.u64()?;
        let nonce = r.array()?;
        let signature = Signature::from_bytes(&r.array()?);
        r.finish()?;
        device.verify(&add_signed(&device, time, &nonce), &signature).map_err(|_| Error::Signature)?;
        Ok(AddOwnerRequest { device, time, nonce, signature })
    }
}

/// A device an owner lets use the relay. `mine`: the owner's own computer
/// ("my computer" there, it controls the owner without asking), which then
/// owns the relay too, for as long as the owner does and lists it so.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Guest {
    pub id: PublicKey,
    pub mine: bool,
}

impl From<PublicKey> for Guest {
    fn from(id: PublicKey) -> Guest {
        Guest { id, mine: false }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestsRequest {
    /// The owner.
    pub device: PublicKey,
    /// Unix seconds.
    pub time: u64,
    pub nonce: [u8; 16],
    pub guests: Vec<Guest>,
    pub signature: Signature,
}

/// Version 1 lists keys only; version 2 adds a `mine` byte after each.
const GUESTS_MINE: u8 = 2;

fn guests_signed(version: u8, device: &PublicKey, time: u64, nonce: &[u8; 16], guests: &[Guest]) -> Vec<u8> {
    let mut v = [GUESTS_DOMAIN, &[version], device.as_bytes(), &time.to_be_bytes(), nonce].concat();
    put_guests(&mut v, version, guests);
    v
}

fn put_guests(v: &mut Vec<u8>, version: u8, guests: &[Guest]) {
    for g in guests {
        v.extend_from_slice(g.id.as_bytes());
        if version == GUESTS_MINE {
            v.push(g.mine as u8);
        }
    }
}

impl GuestsRequest {
    /// At most [`MAX_GUESTS`] of `guests` go in.
    pub fn new(device: &SecretKey, time: u64, guests: &[Guest]) -> GuestsRequest {
        let guests = guests[..guests.len().min(MAX_GUESTS)].to_vec();
        let nonce = random_key();
        let signature = device.sign(&guests_signed(Self::version_for(&guests), &device.public(), time, &nonce, &guests));
        GuestsRequest { device: device.public(), time, nonce, guests, signature }
    }

    /// Version 1 when no guest is `mine`, so a relay from before version 2
    /// still takes the list.
    fn version_for(guests: &[Guest]) -> u8 {
        if guests.iter().any(|g| g.mine) { GUESTS_MINE } else { VERSION }
    }

    pub fn encode(&self) -> String {
        let version = Self::version_for(&self.guests);
        let mut v = [&[version][..], self.device.as_bytes(), &self.time.to_be_bytes(), &self.nonce, &self.signature.to_bytes()]
            .concat();
        put_guests(&mut v, version, &self.guests);
        text::encode(GUESTS_PREFIX, &v)
    }

    /// Decodes and checks the signature (not the time).
    pub fn decode(s: &str) -> Result<GuestsRequest, Error> {
        let p = text::decode(GUESTS_PREFIX, s)?;
        let mut r = Reader::new(&p);
        let version = match r.u8()? {
            v @ (VERSION | GUESTS_MINE) => v,
            v => return Err(Error::Version(v)),
        };
        let device = PublicKey::from_bytes(&r.array()?).map_err(|_| Error::Field("device key"))?;
        let time = r.u64()?;
        let nonce = r.array()?;
        let signature = Signature::from_bytes(&r.array()?);
        let mut guests = Vec::new();
        while let Ok(g) = r.array::<32>() {
            if guests.len() == MAX_GUESTS {
                return Err(Error::Length);
            }
            let id = PublicKey::from_bytes(&g).map_err(|_| Error::Field("guest key"))?;
            let mine = version == GUESTS_MINE && r.u8()? != 0;
            guests.push(Guest { id, mine });
        }
        r.finish()?;
        device.verify(&guests_signed(version, &device, time, &nonce, &guests), &signature).map_err(|_| Error::Signature)?;
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
    pub signature: Signature,
}

fn meet_signed(device: &PublicKey, owner: &PublicKey, token: &[u8; 8]) -> Vec<u8> {
    [MEET_DOMAIN, &[VERSION], device.as_bytes(), owner.as_bytes(), token].concat()
}

impl MeetRequest {
    pub fn new(device: &SecretKey, owner: PublicKey, token: [u8; 8]) -> MeetRequest {
        let signature = device.sign(&meet_signed(&device.public(), &owner, &token));
        MeetRequest { device: device.public(), owner, token, signature }
    }

    pub fn encode(&self) -> String {
        let v = [&[VERSION][..], self.device.as_bytes(), self.owner.as_bytes(), &self.token, &self.signature.to_bytes()]
            .concat();
        text::encode(MEET_PREFIX, &v)
    }

    /// Decodes and checks the signature.
    pub fn decode(s: &str) -> Result<MeetRequest, Error> {
        let p = text::decode(MEET_PREFIX, s)?;
        let mut r = Reader::new(&p);
        match r.u8()? {
            VERSION => {}
            v => return Err(Error::Version(v)),
        }
        let device = PublicKey::from_bytes(&r.array()?).map_err(|_| Error::Field("device key"))?;
        let owner = PublicKey::from_bytes(&r.array()?).map_err(|_| Error::Field("owner key"))?;
        let token = r.array()?;
        let signature = Signature::from_bytes(&r.array()?);
        r.finish()?;
        device.verify(&meet_signed(&device, &owner, &token), &signature).map_err(|_| Error::Signature)?;
        Ok(MeetRequest { device, owner, token, signature })
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
        let r = ClaimRequest::new([9; 16], &key(1));
        assert_eq!(ClaimRequest::decode(&r.encode()).unwrap(), r);
        // Someone else's key with a signature not made by it.
        let mut f = ClaimRequest::new([9; 16], &key(2));
        f.device = key(1).public();
        assert_eq!(ClaimRequest::decode(&f.encode()), Err(Error::Signature));
        // Same signature, another claim key.
        let mut f = r.clone();
        f.claim_key = [8; 16];
        assert_eq!(ClaimRequest::decode(&f.encode()), Err(Error::Signature));
    }

    #[test]
    fn add_owner_round_trip_and_forgery() {
        let r = AddOwnerRequest::new(&key(1), 1_000);
        assert_eq!(AddOwnerRequest::decode(&r.encode()).unwrap(), r);
        let mut f = r.clone();
        f.time = 2_000;
        assert_eq!(AddOwnerRequest::decode(&f.encode()), Err(Error::Signature));
        assert_ne!(AddOwnerRequest::new(&key(1), 1_000).nonce, r.nonce);
        // A claim request is not an add-owner request.
        assert_eq!(AddOwnerRequest::decode(&ClaimRequest::new([1; 16], &key(1)).encode()), Err(Error::WrongPrefix));
    }

    #[test]
    fn guests_round_trip_and_forgery() {
        let guests: Vec<Guest> = [key(2).public(), key(3).public()].map(Guest::from).into();
        let r = GuestsRequest::new(&key(1), 1_000, &guests);
        assert_eq!(GuestsRequest::decode(&r.encode()).unwrap(), r);
        let empty = GuestsRequest::new(&key(1), 1_000, &[]);
        assert_eq!(GuestsRequest::decode(&empty.encode()).unwrap().guests, []);
        // A guest slipped in after signing.
        let mut f = r.clone();
        f.guests.push(key(4).public().into());
        assert_eq!(GuestsRequest::decode(&f.encode()), Err(Error::Signature));
        let many: Vec<Guest> = (0..MAX_GUESTS + 5).map(|_| SecretKey::generate().public().into()).collect();
        assert_eq!(GuestsRequest::new(&key(1), 1_000, &many).guests.len(), MAX_GUESTS);
        assert_eq!(GuestsRequest::decode(&AddOwnerRequest::new(&key(1), 1_000).encode()), Err(Error::WrongPrefix));
    }

    #[test]
    fn guests_with_my_computers() {
        let guests = [Guest { id: key(2).public(), mine: true }, key(3).public().into()];
        let r = GuestsRequest::new(&key(1), 1_000, &guests);
        assert!(r.encode().len() > GuestsRequest::new(&key(1), 1_000, &[key(2).public().into(), key(3).public().into()]).encode().len());
        assert_eq!(GuestsRequest::decode(&r.encode()).unwrap(), r);
        // Made one of the owner's computers after signing.
        let mut f = r.clone();
        f.guests[1].mine = true;
        assert_eq!(GuestsRequest::decode(&f.encode()), Err(Error::Signature));
        // Without any, the list goes as version 1, as older relays read it.
        let plain = GuestsRequest::new(&key(1), 1_000, &[key(2).public().into()]);
        assert_eq!(text::decode(GUESTS_PREFIX, &plain.encode()).unwrap()[0], VERSION);
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
        let r = MeetRequest::new(&key(2), key(1).public(), [7; 8]);
        assert_eq!(MeetRequest::decode(&r.encode()).unwrap(), r);
        // Someone else's device named in a request it did not sign.
        let mut f = MeetRequest::new(&key(3), key(1).public(), [7; 8]);
        f.device = key(2).public();
        assert_eq!(MeetRequest::decode(&f.encode()), Err(Error::Signature));
        let mut f = r.clone();
        f.token = [8; 8];
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
