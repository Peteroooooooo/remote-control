//! Who the relay forwards for: the roster (`target-plan` 5.2). The roster
//! is the owners and each owner's guests (the devices it listed in
//! `guests.txt`). The owners are the devices that joined with one of the
//! relay's codes (`owners.txt`). A datagram is forwarded only if both ends
//! are on the roster and at least one is an owner; anything else is
//! dropped. Anyone may connect, so a device an owner lists a moment later
//! gets through without reconnecting.
//!
//! A device not on the roster that brought one of an owner's invites (a
//! connection code, `POST /rc/meet`) and that owner also get through to
//! each other, for [`crate::api::MEET_KEEP`]: long enough for the owner to
//! welcome it and put it on its guest list. Once listed, the list decides:
//! taking it off again ends the forwarding, grant or not.
//!
//! An endpoint counts as the device it is, or as the device its device
//! proof speaks for (`Authorization: Bearer rcd1…`: a session window's own
//! key). The proof's expiry is checked when the connection is made; the
//! connection then speaks for the device for as long as it lasts (a
//! reconnect brings the same token, so a client must renew it before it
//! expires). The roster is looked up for every datagram, so taking a device
//! off it stops its traffic at once.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Mutex, RwLock},
};

use iroh_base::EndpointId;
use iroh_relay::server::{Access, AccessControl, ClientRequest, ConnectionId};
use rc_ident::proof::{DeviceProof, unix_now};
use tracing::info;

#[derive(Debug, Default)]
struct Roster {
    owners: HashSet<EndpointId>,
    /// Each owner's guests, as the owner sent them.
    guests: BTreeMap<EndpointId, Vec<EndpointId>>,
    /// The owners and the guests of the current owners.
    listed: HashSet<EndpointId>,
}

impl Roster {
    fn update(&mut self) {
        self.listed = self.owners.clone();
        for (o, gs) in &self.guests {
            if self.owners.contains(o) {
                self.listed.extend(gs.iter().copied());
            }
        }
    }
}

#[derive(Debug, Default)]
struct Peer {
    conns: Vec<(ConnectionId, Option<EndpointId>)>,
}

impl Peer {
    /// The devices an endpoint may count as: itself and what its proofs speak for.
    fn any(&self, id: &EndpointId, set: &HashSet<EndpointId>) -> bool {
        set.contains(id) || self.conns.iter().any(|(_, s)| s.is_some_and(|d| set.contains(&d)))
    }
}

#[derive(Debug)]
pub struct Policy {
    roster: RwLock<Roster>,
    peers: Mutex<HashMap<EndpointId, Peer>>,
    /// (owner, device that brought its invite) and until when (Unix seconds).
    met: Mutex<HashMap<(EndpointId, EndpointId), u64>>,
}

/// The devices an endpoint counts as: itself and what its proofs speak for.
fn devices(peers: &HashMap<EndpointId, Peer>, id: &EndpointId) -> Vec<EndpointId> {
    let mut v = vec![*id];
    if let Some(p) = peers.get(id) {
        v.extend(p.conns.iter().filter_map(|(_, s)| *s));
    }
    v
}

fn counts_in(peers: &HashMap<EndpointId, Peer>, id: &EndpointId, set: &HashSet<EndpointId>) -> bool {
    match peers.get(id) {
        Some(p) => p.any(id, set),
        None => set.contains(id),
    }
}

impl Policy {
    pub fn new(owners: impl IntoIterator<Item = EndpointId>, guests: BTreeMap<EndpointId, Vec<EndpointId>>) -> Policy {
        let mut r = Roster { owners: owners.into_iter().collect(), guests, ..Roster::default() };
        r.update();
        Policy { roster: RwLock::new(r), peers: Mutex::default(), met: Mutex::default() }
    }

    pub fn set_roster(&self, owners: impl IntoIterator<Item = EndpointId>, guests: BTreeMap<EndpointId, Vec<EndpointId>>) {
        let mut r = self.roster.write().unwrap();
        r.owners = owners.into_iter().collect();
        r.guests = guests;
        r.update();
    }

    /// A device the owner lists no longer needs the grant from its invite
    /// (`admit`): from now on the list decides, so taking it off the list
    /// (the owner's lobby closed) stops the forwarding.
    pub fn set_guests(&self, owner: EndpointId, guests: Vec<EndpointId>) {
        self.met.lock().unwrap().retain(|(o, d), _| *o != owner || !guests.contains(d));
        let mut r = self.roster.write().unwrap();
        if guests.is_empty() {
            r.guests.remove(&owner);
        } else {
            r.guests.insert(owner, guests);
        }
        r.update();
    }

    /// `device` brought one of `owner`'s invites: the two may reach each
    /// other until `until` (Unix seconds).
    pub fn admit(&self, owner: EndpointId, device: EndpointId, until: u64) {
        let now = unix_now();
        let mut m = self.met.lock().unwrap();
        m.retain(|_, t| *t > now);
        m.insert((owner, device), until);
    }

    /// Whether `device` was admitted to `owner` and still is.
    pub fn admitted(&self, owner: &EndpointId, device: &EndpointId) -> bool {
        self.met.lock().unwrap().get(&(*owner, *device)).is_some_and(|t| *t > unix_now())
    }

    pub fn owners(&self) -> HashSet<EndpointId> {
        self.roster.read().unwrap().owners.clone()
    }

    /// The devices connected now (by key, and by what their proofs speak for).
    pub fn online(&self) -> HashSet<EndpointId> {
        let peers = self.peers.lock().unwrap();
        peers.keys().flat_map(|id| devices(&peers, id)).collect()
    }
}

impl AccessControl for Policy {
    async fn on_connect(&self, req: &ClientRequest) -> Access {
        let id = req.endpoint_id();
        let speaks = match req.auth_token() {
            None => None,
            Some(t) => match DeviceProof::check(&t, &id, unix_now()) {
                Ok(p) => Some(p.device),
                Err(e) => {
                    info!("{}: device proof refused: {e}", id.fmt_short());
                    None
                }
            },
        };
        let what = {
            let r = self.roster.read().unwrap();
            let dev = speaks.unwrap_or(id);
            let kind = if r.owners.contains(&id) || r.owners.contains(&dev) {
                "an owner"
            } else if r.listed.contains(&id) || r.listed.contains(&dev) {
                "a guest"
            } else {
                "not listed"
            };
            match speaks {
                Some(d) => format!("{kind} (proof for {})", d.fmt_short()),
                None => kind.to_string(),
            }
        };
        info!("{} connected ({}): {what}", id.fmt_short(), req.connection_id());
        self.peers.lock().unwrap().entry(id).or_default().conns.push((req.connection_id(), speaks));
        Access::Allow
    }

    fn on_disconnect(&self, id: EndpointId, conn: ConnectionId) {
        let mut peers = self.peers.lock().unwrap();
        if let Some(p) = peers.get_mut(&id) {
            p.conns.retain(|(c, _)| *c != conn);
            if p.conns.is_empty() {
                peers.remove(&id);
            }
        }
        info!("{} disconnected ({conn})", id.fmt_short());
    }

    fn allow_forward(&self, src: EndpointId, dst: EndpointId, _len: usize) -> bool {
        let r = self.roster.read().unwrap();
        let peers = self.peers.lock().unwrap();
        if (counts_in(&peers, &src, &r.owners) || counts_in(&peers, &dst, &r.owners))
            && counts_in(&peers, &src, &r.listed)
            && counts_in(&peers, &dst, &r.listed)
        {
            return true;
        }
        let met = self.met.lock().unwrap();
        if met.is_empty() {
            return false;
        }
        let now = unix_now();
        let (a, b) = (devices(&peers, &src), devices(&peers, &dst));
        let pair = |o: &EndpointId, d: &EndpointId| r.owners.contains(o) && met.get(&(*o, *d)).is_some_and(|t| *t > now);
        a.iter().any(|x| b.iter().any(|y| pair(x, y) || pair(y, x)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh_base::SecretKey;

    fn id(n: u8) -> EndpointId {
        SecretKey::from_bytes(&[n; 32]).public()
    }

    #[test]
    fn an_invite_holds_until_the_owner_lists_the_device_then_the_list_decides() {
        let (owner, stranger, other) = (id(1), id(2), id(3));
        let p = Policy::new([owner], BTreeMap::new());
        assert!(!p.allow_forward(stranger, owner, 0));
        p.admit(owner, stranger, unix_now() + 600);
        assert!(p.allow_forward(stranger, owner, 0));
        // A list sent before the owner welcomed the stranger leaves the grant.
        p.set_guests(owner, vec![other]);
        assert!(p.allow_forward(owner, stranger, 0));
        p.set_guests(owner, vec![stranger]);
        assert!(p.allow_forward(stranger, owner, 0));
        // Off the list (the owner's lobby closed): no more forwarding.
        p.set_guests(owner, vec![]);
        assert!(!p.allow_forward(stranger, owner, 0));
        assert!(!p.allow_forward(owner, stranger, 0));
    }

    #[test]
    fn only_owners_lists_count() {
        let p = Policy::new([id(1), id(2)], BTreeMap::new());
        assert!(p.allow_forward(id(1), id(2), 0), "owner ↔ owner");
        p.set_guests(id(1), vec![id(3)]);
        assert!(p.allow_forward(id(3), id(1), 0));
        assert!(p.allow_forward(id(3), id(2), 0), "a guest of one owner reaches the other owners");
        // A guest's own list counts for nothing.
        p.set_guests(id(3), vec![id(4)]);
        assert!(!p.allow_forward(id(4), id(3), 0));
        // An owner removed (its code deleted on the relay): its guests are out.
        p.set_roster([id(2)], BTreeMap::from([(id(1), vec![id(3)])]));
        assert!(!p.allow_forward(id(3), id(2), 0));
        assert!(!p.allow_forward(id(1), id(2), 0));
        assert_eq!(p.owners(), HashSet::from([id(2)]));
    }

    #[test]
    fn a_removed_owner_cannot_meet_with_its_codes() {
        let p = Policy::new([id(1)], BTreeMap::new());
        p.admit(id(1), id(5), unix_now() + 600);
        assert!(p.allow_forward(id(5), id(1), 0));
        p.set_roster([], BTreeMap::new());
        assert!(!p.allow_forward(id(5), id(1), 0), "no owner any more, so no grant either");
    }
}
