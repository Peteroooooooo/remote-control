//! The relay's state directory: who owns it (`owners.txt`), the devices
//! each owner lets use it (`guests.txt`), each owner's connection codes not
//! used yet (`invites.txt`) and the claim keys not used yet (`claims.txt`). Small text files, each replaced atomically (written next
//! to the old one, then renamed over it), so the running relay,
//! `rc-relay owners …` and `rc-relay reset-claim` can all change them.
//! Everyone reads the files again before changing them.

use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use iroh_base::EndpointId;
use rc_ident::proof::unix_now;

use crate::api::{Guest, Invite, random_key};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    pub id: EndpointId,
    /// Unix seconds.
    pub added: u64,
    pub note: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    pub key: [u8; 16],
    /// Unix seconds; `None` for the key printed when nobody owns the relay.
    pub expires: Option<u64>,
}

impl Claim {
    pub fn initial() -> Claim {
        Claim { key: random_key(), expires: None }
    }

    pub fn valid_for(secs: u64) -> Claim {
        Claim { key: random_key(), expires: Some(unix_now() + secs) }
    }

    fn live(&self, now: u64) -> bool {
        self.expires.is_none_or(|t| now < t)
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != 2 * N {
        return None;
    }
    let mut out = [0; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Notes go in a tab-separated line.
fn clean(note: &str) -> String {
    note.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().trim().to_string()
}

#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Store {
        Store { dir: dir.into() }
    }

    pub fn owners_path(&self) -> PathBuf {
        self.dir.join("owners.txt")
    }

    fn claims_path(&self) -> PathBuf {
        self.dir.join("claims.txt")
    }

    /// Lines: device id, added (Unix seconds), note. A missing file is no owners.
    pub fn owners(&self) -> Result<Vec<Owner>> {
        let path = self.owners_path();
        let mut v = Vec::new();
        for (n, line) in lines(&path)?.iter().enumerate() {
            let f: Vec<&str> = line.split('\t').collect();
            let id = f[0].trim().parse().map_err(|e| anyhow!("{}:{}: bad device id: {e}", path.display(), n + 1))?;
            let added = f.get(1).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            v.push(Owner { id, added, note: f.get(2).map(|s| clean(s)).unwrap_or_default() });
        }
        Ok(v)
    }

    pub fn save_owners(&self, owners: &[Owner]) -> Result<()> {
        let mut text = String::from("# rc-relay owners: device id, added (Unix seconds), note\n");
        for o in owners {
            text += &format!("{}\t{}\t{}\n", o.id, o.added, clean(&o.note));
        }
        self.write(&self.owners_path(), &text)
    }

    /// Adds `id` (no-op if it owns the relay already). True if it was new.
    pub fn add_owner(&self, id: EndpointId, note: &str) -> Result<bool> {
        let mut owners = self.owners()?;
        if owners.iter().any(|o| o.id == id) {
            return Ok(false);
        }
        owners.push(Owner { id, added: unix_now(), note: clean(note) });
        self.save_owners(&owners)?;
        Ok(true)
    }

    pub fn remove_owner(&self, id: &EndpointId) -> Result<bool> {
        let mut owners = self.owners()?;
        let before = owners.len();
        owners.retain(|o| o.id != *id);
        if owners.len() == before {
            return Ok(false);
        }
        self.save_owners(&owners)?;
        self.set_guests(id, &[])?;
        self.set_invites(id, &[])?;
        Ok(true)
    }

    pub fn guests_path(&self) -> PathBuf {
        self.dir.join("guests.txt")
    }

    /// Lines: owner id, guest id, and `mine` for the owner's own computers.
    /// A missing file is no guests.
    pub fn guests(&self) -> Result<BTreeMap<EndpointId, Vec<Guest>>> {
        let path = self.guests_path();
        let mut m: BTreeMap<EndpointId, Vec<Guest>> = BTreeMap::new();
        for (n, line) in lines(&path)?.iter().enumerate() {
            let f: Vec<&str> = line.split('\t').collect();
            let bad = || anyhow!("{}:{}: bad line", path.display(), n + 1);
            let owner: EndpointId = f[0].trim().parse().map_err(|_| bad())?;
            let id: EndpointId = f.get(1).ok_or_else(bad)?.trim().parse().map_err(|_| bad())?;
            let mine = f.get(2).is_some_and(|s| s.trim() == "mine");
            m.entry(owner).or_default().push(Guest { id, mine });
        }
        Ok(m)
    }

    /// Replaces `owner`'s guests.
    pub fn set_guests(&self, owner: &EndpointId, guests: &[Guest]) -> Result<()> {
        let mut m = self.guests()?;
        let old = m.remove(owner).unwrap_or_default();
        if old == guests {
            return Ok(());
        }
        if !guests.is_empty() {
            m.insert(*owner, guests.to_vec());
        }
        let mut text = String::from("# rc-relay guests: owner id, a device the owner lets use the relay, mine = one of the owner's computers (owns the relay too)\n");
        for (o, gs) in &m {
            for g in gs {
                text += &format!("{o}\t{}{}\n", g.id, if g.mine { "\tmine" } else { "" });
            }
        }
        self.write(&self.guests_path(), &text)
    }

    fn invites_path(&self) -> PathBuf {
        self.dir.join("invites.txt")
    }

    /// Lines: owner id, token (hex), expires (Unix seconds). A missing file
    /// is no invites; expired ones are left out.
    pub fn invites(&self) -> Result<BTreeMap<EndpointId, Vec<Invite>>> {
        let path = self.invites_path();
        let now = unix_now();
        let mut m: BTreeMap<EndpointId, Vec<Invite>> = BTreeMap::new();
        for (n, line) in lines(&path)?.iter().enumerate() {
            let f: Vec<&str> = line.split('\t').collect();
            let bad = || anyhow!("{}:{}: bad line", path.display(), n + 1);
            let owner: EndpointId = f[0].trim().parse().map_err(|_| bad())?;
            let token = f.get(1).and_then(|t| unhex::<8>(t.trim())).ok_or_else(bad)?;
            let expires: u64 = f.get(2).and_then(|t| t.trim().parse().ok()).ok_or_else(bad)?;
            if expires > now {
                m.entry(owner).or_default().push(Invite { token, expires });
            }
        }
        Ok(m)
    }

    /// Replaces `owner`'s invites (the expired ones left out).
    pub fn set_invites(&self, owner: &EndpointId, invites: &[Invite]) -> Result<()> {
        let now = unix_now();
        let mut m = self.invites()?;
        let old = m.remove(owner).unwrap_or_default();
        let new: Vec<Invite> = invites.iter().filter(|i| i.expires > now).copied().collect();
        if old == new {
            return Ok(());
        }
        if !new.is_empty() {
            m.insert(*owner, new);
        }
        self.save_invites(&m)
    }

    fn save_invites(&self, m: &BTreeMap<EndpointId, Vec<Invite>>) -> Result<()> {
        let mut text = String::from("# rc-relay invites: owner id, token of an unused connection code (hex), expires (Unix seconds)\n");
        for (o, is) in m {
            for i in is {
                text += &format!("{o}\t{}\t{}\n", hex(&i.token), i.expires);
            }
        }
        self.write(&self.invites_path(), &text)
    }

    /// Uses up `owner`'s invite with `token`: true if it was there and not expired.
    pub fn take_invite(&self, owner: &EndpointId, token: &[u8; 8]) -> Result<bool> {
        let mut m = self.invites()?;
        let Some(is) = m.get_mut(owner) else { return Ok(false) };
        let Some(i) = is.iter().position(|i| constant_eq(&i.token, token)) else {
            return Ok(false);
        };
        is.remove(i);
        if is.is_empty() {
            m.remove(owner);
        }
        self.save_invites(&m)?;
        Ok(true)
    }

    /// Lines: key (hex), expires (Unix seconds, 0 = never). Expired keys are left out.
    pub fn claims(&self) -> Result<Vec<Claim>> {
        let path = self.claims_path();
        let now = unix_now();
        let mut v = Vec::new();
        for (n, line) in lines(&path)?.iter().enumerate() {
            let f: Vec<&str> = line.split('\t').collect();
            let key = unhex::<16>(f[0].trim()).ok_or_else(|| anyhow!("{}:{}: bad claim key", path.display(), n + 1))?;
            let expires = match f.get(1).and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0) {
                0 => None,
                t => Some(t),
            };
            let c = Claim { key, expires };
            if c.live(now) {
                v.push(c);
            }
        }
        Ok(v)
    }

    pub fn save_claims(&self, claims: &[Claim]) -> Result<()> {
        let mut text =
            String::from("# rc-relay claim keys not used yet: key (hex), expires (Unix seconds, 0 = never)\n");
        for c in claims {
            text += &format!("{}\t{}\n", hex(&c.key), c.expires.unwrap_or(0));
        }
        self.write(&self.claims_path(), &text)
    }

    pub fn add_claim(&self, c: Claim) -> Result<()> {
        let mut claims = self.claims()?;
        claims.push(c);
        self.save_claims(&claims)
    }

    /// The key printed while nobody owns the relay: kept across restarts,
    /// made if there is none.
    pub fn initial_claim(&self) -> Result<Claim> {
        let claims = self.claims()?;
        if let Some(c) = claims.iter().find(|c| c.expires.is_none()) {
            return Ok(c.clone());
        }
        let c = Claim::initial();
        self.add_claim(c.clone())?;
        Ok(c)
    }

    /// Replaces the initial key with a new one (`rc-relay reset-claim`).
    pub fn reset_initial_claim(&self) -> Result<Claim> {
        let mut claims = self.claims()?;
        claims.retain(|c| c.expires.is_some());
        let c = Claim::initial();
        claims.push(c.clone());
        self.save_claims(&claims)?;
        Ok(c)
    }

    /// Uses up `key`: true if it was valid.
    pub fn take_claim(&self, key: &[u8; 16]) -> Result<bool> {
        let mut claims = self.claims()?;
        let Some(i) = claims.iter().position(|c| constant_eq(&c.key, key)) else {
            return Ok(false);
        };
        claims.remove(i);
        self.save_claims(&claims)?;
        Ok(true)
    }

    fn write(&self, path: &Path, text: &str) -> Result<()> {
        std::fs::create_dir_all(&self.dir).with_context(|| format!("create {}", self.dir.display()))?;
        let tmp = path.with_extension("tmp");
        let mut f = std::fs::File::create(&tmp).with_context(|| format!("write {}", tmp.display()))?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
    }
}

fn constant_eq<const N: usize>(a: &[u8; N], b: &[u8; N]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn lines(path: &Path) -> Result<Vec<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')).map(str::to_string).collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(anyhow!("read {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> EndpointId {
        iroh_base::SecretKey::from_bytes(&[n; 32]).public()
    }

    fn temp(name: &str) -> Store {
        let d = std::env::temp_dir().join(format!("rc-relay-state-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        Store::new(d)
    }

    #[test]
    fn owners_round_trip() {
        let s = temp("owners");
        assert!(s.owners().unwrap().is_empty());
        assert!(s.add_owner(id(1), "台式机\t主力").unwrap());
        assert!(!s.add_owner(id(1), "again").unwrap());
        assert!(s.add_owner(id(2), "").unwrap());
        let o = s.owners().unwrap();
        assert_eq!(o.iter().map(|o| o.id).collect::<Vec<_>>(), [id(1), id(2)]);
        assert_eq!(o[0].note, "台式机 主力");
        assert!(s.remove_owner(&id(1)).unwrap());
        assert!(!s.remove_owner(&id(1)).unwrap());
        assert_eq!(s.owners().unwrap().len(), 1);
        std::fs::write(s.owners_path(), "nonsense\n").unwrap();
        assert!(s.owners().is_err());
    }

    #[test]
    fn guests_per_owner() {
        let s = temp("guests");
        let g = |n: u8| Guest::from(id(n));
        assert!(s.guests().unwrap().is_empty());
        s.add_owner(id(1), "").unwrap();
        s.add_owner(id(2), "").unwrap();
        s.set_guests(&id(1), &[g(5), g(6)]).unwrap();
        s.set_guests(&id(2), &[g(6)]).unwrap();
        s.set_guests(&id(1), &[g(7), Guest { id: id(8), mine: true }]).unwrap();
        let m = s.guests().unwrap();
        assert_eq!(m[&id(1)], [g(7), Guest { id: id(8), mine: true }], "the new list replaces the old one");
        assert_eq!(m[&id(2)], [g(6)]);
        s.set_guests(&id(2), &[]).unwrap();
        assert!(!s.guests().unwrap().contains_key(&id(2)));
        // A file from before `mine`.
        std::fs::write(s.guests_path(), format!("{}\t{}\n", id(2), id(9))).unwrap();
        assert_eq!(s.guests().unwrap()[&id(2)], [g(9)]);
        s.set_guests(&id(1), &[g(7)]).unwrap();
        // A removed owner's guests go with it.
        s.remove_owner(&id(1)).unwrap();
        s.remove_owner(&id(2)).unwrap();
        assert!(s.guests().unwrap().is_empty());
    }

    #[test]
    fn invites_are_used_once_and_expire() {
        let s = temp("invites");
        let now = unix_now();
        let (a, b) = (Invite { token: [1; 8], expires: now + 600 }, Invite { token: [2; 8], expires: now + 600 });
        s.add_owner(id(1), "").unwrap();
        s.set_invites(&id(1), &[a, b, Invite { token: [3; 8], expires: now - 1 }]).unwrap();
        s.set_invites(&id(2), &[a]).unwrap();
        assert_eq!(s.invites().unwrap()[&id(1)], [a, b], "the expired one is left out");
        assert!(!s.take_invite(&id(1), &[3; 8]).unwrap());
        assert!(s.take_invite(&id(1), &[1; 8]).unwrap());
        assert!(!s.take_invite(&id(1), &[1; 8]).unwrap(), "once");
        assert!(s.take_invite(&id(2), &[1; 8]).unwrap(), "another owner's code with the same token");
        assert_eq!(s.invites().unwrap()[&id(1)], [b]);
        s.set_invites(&id(1), &[a]).unwrap();
        assert_eq!(s.invites().unwrap()[&id(1)], [a], "the new list replaces the old one");
        s.remove_owner(&id(1)).unwrap();
        assert!(s.invites().unwrap().is_empty());
    }

    #[test]
    fn claims_are_used_once_and_expire() {
        let s = temp("claims");
        let first = s.initial_claim().unwrap();
        assert_eq!(s.initial_claim().unwrap(), first, "kept across restarts");
        let later = Claim::valid_for(3600);
        s.add_claim(later.clone()).unwrap();
        s.add_claim(Claim { key: random_key(), expires: Some(unix_now() - 1) }).unwrap();
        assert_eq!(s.claims().unwrap().len(), 2, "the expired one is gone");
        assert!(s.take_claim(&later.key).unwrap());
        assert!(!s.take_claim(&later.key).unwrap());
        let reset = s.reset_initial_claim().unwrap();
        assert_ne!(reset, first);
        assert!(!s.take_claim(&first.key).unwrap());
        assert!(s.take_claim(&reset.key).unwrap());
        assert!(s.claims().unwrap().is_empty());
    }
}
