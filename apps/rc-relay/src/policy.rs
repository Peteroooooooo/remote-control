//! Who the relay forwards for:
//! - Members (the devices in `owners.txt`, which joined with one of the
//!   relay's codes) may reach one another and their own guests.
//! - A guest (a device in an owner's `guests.txt`, or one that brought one
//!   of an owner's connection codes via `POST /rc/meet` within [`crate::api::MEET_KEEP`])
//!   may reach only the owner(s) that listed or admitted it — never another
//!   owner and never another guest.
//! - Any `(owner, guest)` in `kicked.txt` is never forwarded, even while the
//!   owner's client has not yet synced the drop notice.
//!
//! An endpoint counts as the device it is, or as the device its device
//! proof speaks for (`Authorization: Bearer rcd1…`: a session window's own
//! key). The roster is looked up for every datagram, so removing an owner
//! or blocking a guest stops its traffic at once.

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
    guests: BTreeMap<EndpointId, HashSet<EndpointId>>,
    /// Guests kicked on the relay (`kicked.txt`), waiting for the owner's
    /// client to sync the drop notice.
    kicked: HashSet<(EndpointId, EndpointId)>,
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
    /// (owner, device that brought its invite) and (until Unix seconds, name).
    met: Mutex<HashMap<(EndpointId, EndpointId), (u64, String)>>,
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
    pub fn new(
        owners: impl IntoIterator<Item = EndpointId>,
        guests: BTreeMap<EndpointId, Vec<EndpointId>>,
        kicked: impl IntoIterator<Item = (EndpointId, EndpointId)>,
    ) -> Policy {
        let r = Roster {
            owners: owners.into_iter().collect(),
            guests: guests.into_iter().map(|(o, gs)| (o, gs.into_iter().collect())).collect(),
            kicked: kicked.into_iter().collect(),
        };
        Policy { roster: RwLock::new(r), peers: Mutex::default(), met: Mutex::default() }
    }

    pub fn set_roster(
        &self,
        owners: impl IntoIterator<Item = EndpointId>,
        guests: BTreeMap<EndpointId, Vec<EndpointId>>,
        kicked: impl IntoIterator<Item = (EndpointId, EndpointId)>,
    ) {
        let kicked_set: HashSet<(EndpointId, EndpointId)> = kicked.into_iter().collect();
        self.met.lock().unwrap().retain(|k, _| !kicked_set.contains(k));
        let mut r = self.roster.write().unwrap();
        r.owners = owners.into_iter().collect();
        r.guests = guests.into_iter().map(|(o, gs)| (o, gs.into_iter().collect())).collect();
        r.kicked = kicked_set;
    }

    /// A device the owner lists no longer needs the grant from its invite
    /// (`admit`): from now on the list decides, so taking it off the list
    /// (the owner's lobby closed) stops the forwarding.
    pub fn set_guests(&self, owner: EndpointId, guests: Vec<EndpointId>) {
        let set: HashSet<EndpointId> = guests.into_iter().collect();
        self.met.lock().unwrap().retain(|(o, d), _| *o != owner || !set.contains(d));
        let mut r = self.roster.write().unwrap();
        if set.is_empty() {
            r.guests.remove(&owner);
        } else {
            r.guests.insert(owner, set);
        }
    }

    /// `device` brought one of `owner`'s invites: the two may reach each
    /// other until `until` (Unix seconds).
    pub fn admit(&self, owner: EndpointId, device: EndpointId, name: &str, until: u64) {
        let now = unix_now();
        let mut m = self.met.lock().unwrap();
        m.retain(|_, (t, _)| *t > now);
        m.insert((owner, device), (until, name.to_string()));
    }

    /// Whether `device` was admitted to `owner` and still is.
    pub fn admitted(&self, owner: &EndpointId, device: &EndpointId) -> bool {
        self.met.lock().unwrap().get(&(*owner, *device)).is_some_and(|(t, _)| *t > unix_now())
    }

    /// Active temporary guests (`(owner, guest, name)`) for `status.txt`.
    pub fn admitted_list(&self) -> Vec<(EndpointId, EndpointId, String)> {
        let now = unix_now();
        let r = self.roster.read().unwrap();
        let mut m = self.met.lock().unwrap();
        m.retain(|_, (t, _)| *t > now);
        let mut v: Vec<_> = m
            .iter()
            .filter(|((o, d), _)| r.owners.contains(o) && !r.kicked.contains(&(*o, *d)))
            .map(|((o, d), (_, n))| (*o, *d, n.clone()))
            .collect();
        v.sort_by_key(|(o, d, _)| (*o, *d));
        v
    }

    pub fn is_kicked(&self, owner: &EndpointId, guest: &EndpointId) -> bool {
        self.roster.read().unwrap().kicked.contains(&(*owner, *guest))
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
            } else if r.guests.iter().any(|(o, gs)| {
                r.owners.contains(o)
                    && ((gs.contains(&id) && !r.kicked.contains(&(*o, id)))
                        || (gs.contains(&dev) && !r.kicked.contains(&(*o, dev))))
            }) {
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
        if counts_in(&peers, &src, &r.owners) && counts_in(&peers, &dst, &r.owners) {
            return true;
        }
        let met = self.met.lock().unwrap();
        let now = unix_now();
        let (a, b) = (devices(&peers, &src), devices(&peers, &dst));
        let pair = |o: &EndpointId, g: &EndpointId| {
            r.owners.contains(o)
                && !r.kicked.contains(&(*o, *g))
                && (r.guests.get(o).is_some_and(|gs| gs.contains(g))
                    || met.get(&(*o, *g)).is_some_and(|(t, _)| *t > now))
        };
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
        let p = Policy::new([owner], BTreeMap::new(), []);
        assert!(!p.allow_forward(stranger, owner, 0));
        p.admit(owner, stranger, "friend", unix_now() + 600);
        assert!(p.allow_forward(stranger, owner, 0));
        assert_eq!(p.admitted_list(), vec![(owner, stranger, "friend".to_string())]);
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
    fn guests_reach_only_their_own_owner_and_kicked_stops_at_once() {
        let p = Policy::new([id(1), id(2)], BTreeMap::new(), []);
        assert!(p.allow_forward(id(1), id(2), 0), "owner ↔ owner");
        p.set_guests(id(1), vec![id(3)]);
        assert!(p.allow_forward(id(3), id(1), 0), "guest → its own owner");
        assert!(!p.allow_forward(id(3), id(2), 0), "guest cannot cross over to another owner");
        // Both owners listing the same guest lets it reach both.
        p.set_guests(id(2), vec![id(3)]);
        assert!(p.allow_forward(id(3), id(2), 0));
        // A guest's own list counts for nothing.
        p.set_guests(id(3), vec![id(4)]);
        assert!(!p.allow_forward(id(4), id(3), 0));
        // Kicking (id(1), id(3)) stops id(3) ↔ id(1) immediately, even while listed,
        // without touching id(2) unless id(2) also kicked it.
        p.set_roster([id(1), id(2)], BTreeMap::from([(id(1), vec![id(3)]), (id(2), vec![id(3)])]), [(id(1), id(3))]);
        assert!(!p.allow_forward(id(3), id(1), 0), "kicked under id(1)");
        assert!(p.allow_forward(id(3), id(2), 0), "still allowed under id(2)");
        // Unkicked: works again.
        p.set_roster([id(1), id(2)], BTreeMap::from([(id(1), vec![id(3)])]), []);
        assert!(p.allow_forward(id(3), id(1), 0));
        // An owner removed (its code deleted on the relay): its guests are out.
        p.set_roster([id(2)], BTreeMap::from([(id(1), vec![id(3)])]), []);
        assert!(!p.allow_forward(id(3), id(1), 0));
        assert!(!p.allow_forward(id(1), id(2), 0));
        assert_eq!(p.owners(), HashSet::from([id(2)]));
    }

    #[test]
    fn a_removed_owner_cannot_meet_with_its_codes() {
        let p = Policy::new([id(1)], BTreeMap::new(), []);
        p.admit(id(1), id(5), "tmp", unix_now() + 600);
        assert!(p.allow_forward(id(5), id(1), 0));
        p.set_roster([], BTreeMap::new(), []);
        assert!(!p.allow_forward(id(5), id(1), 0), "no owner any more, so no grant either");
    }
}
