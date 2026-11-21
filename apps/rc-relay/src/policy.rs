//! Who the relay forwards for:
//! - Members (the devices in `owners.txt`, which joined with one of the
//!   relay's codes) may reach the members that joined with the same code,
//!   and their own guests. Members of different codes never reach each other.
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
    sync::RwLock,
};

use iroh_base::EndpointId;
use iroh_relay::server::{Access, AccessControl, ClientRequest, ConnectionId};
use rc_ident::proof::{DeviceProof, unix_now};
use tracing::info;

#[derive(Debug, Default)]
struct Roster {
    /// Each owner and the code it joined with.
    owners: HashMap<EndpointId, u32>,
    /// Each owner's guests, as the owner sent them.
    guests: BTreeMap<EndpointId, HashSet<EndpointId>>,
    /// Guests kicked on the relay (`kicked.txt`), waiting for the owner's
    /// client to sync the drop notice.
    kicked: HashSet<(EndpointId, EndpointId)>,
    /// (owner, device that brought its invite) and (until Unix seconds, name).
    met: HashMap<(EndpointId, EndpointId), (u64, String)>,
}

#[derive(Debug, Default)]
struct Peer {
    conns: Vec<(ConnectionId, Option<EndpointId>)>,
}

#[derive(Debug)]
pub struct Policy {
    roster: RwLock<Roster>,
    peers: RwLock<HashMap<EndpointId, Peer>>,
}

/// Checks `f(x, y)` across `src` / `dst` and any devices their proofs speak for,
/// without heap allocation.
fn any_device_pair(
    src: &EndpointId,
    src_conns: &[(ConnectionId, Option<EndpointId>)],
    dst: &EndpointId,
    dst_conns: &[(ConnectionId, Option<EndpointId>)],
    mut f: impl FnMut(&EndpointId, &EndpointId) -> bool,
) -> bool {
    let mut check_x = |y: &EndpointId| {
        f(src, y) || src_conns.iter().filter_map(|(_, s)| s.as_ref()).any(|x| f(x, y))
    };
    check_x(dst) || dst_conns.iter().filter_map(|(_, s)| s.as_ref()).any(check_x)
}

impl Policy {
    pub fn new(
        owners: impl IntoIterator<Item = (EndpointId, u32)>,
        guests: BTreeMap<EndpointId, Vec<EndpointId>>,
        kicked: impl IntoIterator<Item = (EndpointId, EndpointId)>,
    ) -> Policy {
        let r = Roster {
            owners: owners.into_iter().collect(),
            guests: guests.into_iter().map(|(o, gs)| (o, gs.into_iter().collect())).collect(),
            kicked: kicked.into_iter().collect(),
            met: HashMap::new(),
        };
        Policy { roster: RwLock::new(r), peers: RwLock::default() }
    }

    pub fn set_roster(
        &self,
        owners: impl IntoIterator<Item = (EndpointId, u32)>,
        guests: BTreeMap<EndpointId, Vec<EndpointId>>,
        kicked: impl IntoIterator<Item = (EndpointId, EndpointId)>,
    ) {
        let kicked_set: HashSet<(EndpointId, EndpointId)> = kicked.into_iter().collect();
        let mut r = self.roster.write().unwrap();
        r.met.retain(|k, _| !kicked_set.contains(k));
        r.owners = owners.into_iter().collect();
        r.guests = guests.into_iter().map(|(o, gs)| (o, gs.into_iter().collect())).collect();
        r.kicked = kicked_set;
    }

    /// A device the owner lists no longer needs the grant from its invite
    /// (`admit`): from now on the list decides, so taking it off the list
    /// (the owner's lobby closed) stops the forwarding.
    pub fn set_guests(&self, owner: EndpointId, guests: Vec<EndpointId>) {
        let set: HashSet<EndpointId> = guests.into_iter().collect();
        let mut r = self.roster.write().unwrap();
        r.met.retain(|(o, d), _| *o != owner || !set.contains(d));
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
        let mut r = self.roster.write().unwrap();
        r.met.retain(|_, (t, _)| *t > now);
        r.met.insert((owner, device), (until, name.to_string()));
    }

    /// Whether `device` was admitted to `owner` and still is.
    pub fn admitted(&self, owner: &EndpointId, device: &EndpointId) -> bool {
        self.roster.read().unwrap().met.get(&(*owner, *device)).is_some_and(|(t, _)| *t > unix_now())
    }

    /// Active temporary guests (`(owner, guest, name)`) for `status.txt`.
    pub fn admitted_list(&self) -> Vec<(EndpointId, EndpointId, String)> {
        let now = unix_now();
        let mut r = self.roster.write().unwrap();
        r.met.retain(|_, (t, _)| *t > now);
        let mut v: Vec<_> = r
            .met
            .iter()
            .filter(|((o, d), _)| r.owners.contains_key(o) && !r.kicked.contains(&(*o, *d)))
            .map(|((o, d), (_, n))| (*o, *d, n.clone()))
            .collect();
        v.sort_by_key(|(o, d, _)| (*o, *d));
        v
    }

    pub fn is_kicked(&self, owner: &EndpointId, guest: &EndpointId) -> bool {
        self.roster.read().unwrap().kicked.contains(&(*owner, *guest))
    }

    pub fn owners(&self) -> HashSet<EndpointId> {
        self.roster.read().unwrap().owners.keys().copied().collect()
    }

    /// The devices connected now (by key, and by what their proofs speak for).
    pub fn online(&self) -> HashSet<EndpointId> {
        let peers = self.peers.read().unwrap();
        peers
            .iter()
            .flat_map(|(id, p)| std::iter::once(*id).chain(p.conns.iter().filter_map(|(_, s)| *s)))
            .collect()
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
            let kind = if r.owners.contains_key(&id) || r.owners.contains_key(&dev) {
                "an owner"
            } else if r.guests.iter().any(|(o, gs)| {
                r.owners.contains_key(o)
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
        self.peers.write().unwrap().entry(id).or_default().conns.push((req.connection_id(), speaks));
        Access::Allow
    }

    fn on_disconnect(&self, id: EndpointId, conn: ConnectionId) {
        let mut peers = self.peers.write().unwrap();
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
        let peers = self.peers.read().unwrap();
        let src_conns = peers.get(&src).map(|p| p.conns.as_slice()).unwrap_or(&[]);
        let dst_conns = peers.get(&dst).map(|p| p.conns.as_slice()).unwrap_or(&[]);
        if any_device_pair(&src, src_conns, &dst, dst_conns, |x, y| {
            r.owners.get(x).is_some_and(|c| r.owners.get(y) == Some(c))
        }) {
            return true;
        }
        let guest_pair = |o: &EndpointId, g: &EndpointId| {
            r.owners.contains_key(o)
                && !r.kicked.contains(&(*o, *g))
                && r.guests.get(o).is_some_and(|gs| gs.contains(g))
        };
        if any_device_pair(&src, src_conns, &dst, dst_conns, |x, y| guest_pair(x, y) || guest_pair(y, x)) {
            return true;
        }
        if r.met.is_empty() {
            return false;
        }
        let now = unix_now();
        let met_pair = |o: &EndpointId, g: &EndpointId| {
            r.owners.contains_key(o)
                && !r.kicked.contains(&(*o, *g))
                && r.met.get(&(*o, *g)).is_some_and(|(t, _)| *t > now)
        };
        any_device_pair(&src, src_conns, &dst, dst_conns, |x, y| met_pair(x, y) || met_pair(y, x))
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
        let p = Policy::new([(owner, 1)], BTreeMap::new(), []);
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
        let p = Policy::new([(id(1), 1), (id(2), 1)], BTreeMap::new(), []);
        assert!(p.allow_forward(id(1), id(2), 0), "owner ↔ owner of the same code");
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
        p.set_roster([(id(1), 1), (id(2), 1)], BTreeMap::from([(id(1), vec![id(3)]), (id(2), vec![id(3)])]), [(id(1), id(3))]);
        assert!(!p.allow_forward(id(3), id(1), 0), "kicked under id(1)");
        assert!(p.allow_forward(id(3), id(2), 0), "still allowed under id(2)");
        // Unkicked: works again.
        p.set_roster([(id(1), 1), (id(2), 1)], BTreeMap::from([(id(1), vec![id(3)])]), []);
        assert!(p.allow_forward(id(3), id(1), 0));
        // An owner removed (its code deleted on the relay): its guests are out.
        p.set_roster([(id(2), 1)], BTreeMap::from([(id(1), vec![id(3)])]), []);
        assert!(!p.allow_forward(id(3), id(1), 0));
        assert!(!p.allow_forward(id(1), id(2), 0));
        assert_eq!(p.owners(), HashSet::from([id(2)]));
    }

    #[test]
    fn owners_of_different_codes_do_not_reach_each_other() {
        let p = Policy::new([(id(1), 1), (id(2), 2), (id(3), 1)], BTreeMap::new(), []);
        assert!(p.allow_forward(id(1), id(3), 0), "same code");
        assert!(!p.allow_forward(id(1), id(2), 0), "code 1 and code 2");
        assert!(!p.allow_forward(id(2), id(3), 0));
        // Listing the other owner as a guest (they met with a connection code) lets them through.
        p.set_guests(id(1), vec![id(2)]);
        assert!(p.allow_forward(id(2), id(1), 0));
        assert!(!p.allow_forward(id(2), id(3), 0), "only with the owner that listed it");
    }

    #[test]
    fn a_removed_owner_cannot_meet_with_its_codes() {
        let p = Policy::new([(id(1), 1)], BTreeMap::new(), []);
        p.admit(id(1), id(5), "tmp", unix_now() + 600);
        assert!(p.allow_forward(id(5), id(1), 0));
        p.set_roster([], BTreeMap::new(), []);
        assert!(!p.allow_forward(id(5), id(1), 0), "no owner any more, so no grant either");
    }
}
