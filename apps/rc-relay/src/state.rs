//! The relay's state directory: its relay codes (`codes.txt`), the devices
//! that joined with them, its owners (`owners.txt`), the devices each owner
//! lets use it (`guests.txt`) and each owner's connection codes not used
//! yet (`invites.txt`). Small text files, each replaced atomically (written
//! next to the old one, then renamed over it), so the running relay and the
//! management screen can both change them. Everyone reads the files again
//! before changing them.
//!
//! A code is a group: every device that joined with it is an owner, until
//! it is removed or its code is deleted. A code works any number of times.
//! Codes are named `码 1`, `码 2`, … (the smallest free number) until renamed.

use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use iroh_base::EndpointId;
use rc_ident::proof::unix_now;

use crate::api::{Invite, random_key};

/// One of the relay's codes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Code {
    /// Stays with the code for good; its name may change.
    pub n: u32,
    pub key: [u8; 16],
    /// Empty: shown as `码 <n>`.
    pub name: String,
}

impl Code {
    pub fn shown_name(&self) -> String {
        if self.name.is_empty() { format!("码 {}", self.n) } else { self.name.clone() }
    }
}

/// A device that joined with a code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    pub id: EndpointId,
    /// The [`Code::n`] it joined with.
    pub code: u32,
    /// Unix seconds.
    pub added: u64,
    /// The name the device gave when it joined.
    pub name: String,
}

/// What the running relay tells the management screen (`status.txt`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// Unix seconds; 0: unknown.
    pub started: u64,
    /// When it was written (Unix seconds): an old one means the relay is not running.
    pub written: u64,
    /// Devices connected now.
    pub online: Vec<EndpointId>,
    /// When each owner was last connected (Unix seconds).
    pub seen: BTreeMap<EndpointId, u64>,
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

/// Names go in a tab-separated line.
pub fn clean(name: &str) -> String {
    name.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().trim().to_string()
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

    pub fn codes_path(&self) -> PathBuf {
        self.dir.join("codes.txt")
    }

    /// Lines: number, key (hex), name. A missing file is no codes.
    pub fn codes(&self) -> Result<Vec<Code>> {
        let path = self.codes_path();
        let mut v = Vec::new();
        for (i, line) in lines(&path)?.iter().enumerate() {
            let f: Vec<&str> = line.split('\t').collect();
            let bad = || anyhow!("{}:{}: bad line", path.display(), i + 1);
            let n = f[0].trim().parse().map_err(|_| bad())?;
            let key = f.get(1).and_then(|k| unhex::<16>(k.trim())).ok_or_else(bad)?;
            v.push(Code { n, key, name: f.get(2).map(|s| clean(s)).unwrap_or_default() });
        }
        Ok(v)
    }

    fn save_codes(&self, codes: &[Code]) -> Result<()> {
        let mut text = String::from("# rc-relay codes: number, key (hex), name (empty: 码 <number>)\n");
        for c in codes {
            text += &format!("{}\t{}\t{}\n", c.n, hex(&c.key), clean(&c.name));
        }
        self.write(&self.codes_path(), &text)
    }

    /// A new code with the smallest number not in use.
    pub fn new_code(&self, name: &str) -> Result<Code> {
        let mut codes = self.codes()?;
        let n = (1..).find(|n| !codes.iter().any(|c| c.n == *n)).expect("a free number");
        let c = Code { n, key: random_key(), name: clean(name) };
        codes.push(c.clone());
        codes.sort_by_key(|c| c.n);
        self.save_codes(&codes)?;
        Ok(c)
    }

    pub fn rename_code(&self, n: u32, name: &str) -> Result<()> {
        let mut codes = self.codes()?;
        let c = codes.iter_mut().find(|c| c.n == n).ok_or_else(|| anyhow!("no code {n}"))?;
        c.name = clean(name);
        self.save_codes(&codes)
    }

    /// Deletes code `n` and everyone who joined with it. Returns those devices.
    pub fn delete_code(&self, n: u32) -> Result<Vec<EndpointId>> {
        let mut codes = self.codes()?;
        let before = codes.len();
        codes.retain(|c| c.n != n);
        if codes.len() == before {
            bail!("no code {n}");
        }
        let gone: Vec<EndpointId> = self.owners()?.iter().filter(|o| o.code == n).map(|o| o.id).collect();
        for id in &gone {
            self.remove_owner(id)?;
        }
        self.save_codes(&codes)?;
        Ok(gone)
    }

    /// The code whose key is `key`.
    pub fn code_with(&self, key: &[u8; 16]) -> Result<Option<Code>> {
        Ok(self.codes()?.into_iter().find(|c| constant_eq(&c.key, key)))
    }

    /// The first code; one is made if there is none and no owner either
    /// (a new relay). `None` when every code was deleted on purpose.
    pub fn first_code(&self) -> Result<Option<Code>> {
        let codes = self.codes()?;
        if let Some(c) = codes.into_iter().next() {
            return Ok(Some(c));
        }
        if self.codes_path().exists() || !self.owners()?.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.new_code("")?))
    }

    /// Lines: device id, code number, added (Unix seconds), name. A missing
    /// file is no owners.
    pub fn owners(&self) -> Result<Vec<Owner>> {
        let path = self.owners_path();
        let mut v = Vec::new();
        for (i, line) in lines(&path)?.iter().enumerate() {
            let f: Vec<&str> = line.split('\t').collect();
            let bad = || anyhow!("{}:{}: bad line", path.display(), i + 1);
            let id = f[0].trim().parse().map_err(|_| bad())?;
            let code = f.get(1).ok_or_else(bad)?.trim().parse().map_err(|_| bad())?;
            let added = f.get(2).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            v.push(Owner { id, code, added, name: f.get(3).map(|s| clean(s)).unwrap_or_default() });
        }
        Ok(v)
    }

    fn save_owners(&self, owners: &[Owner]) -> Result<()> {
        let mut text = String::from("# rc-relay owners: device id, code number it joined with, added (Unix seconds), name\n");
        for o in owners {
            text += &format!("{}\t{}\t{}\t{}\n", o.id, o.code, o.added, clean(&o.name));
        }
        self.write(&self.owners_path(), &text)
    }

    /// `id` joined with code `code` as `name`: it is an owner in that code's
    /// group (moved there if it was in another; its name updated).
    pub fn join(&self, id: EndpointId, code: u32, name: &str) -> Result<()> {
        let mut owners = self.owners()?;
        let name = clean(name);
        match owners.iter_mut().find(|o| o.id == id) {
            Some(o) => {
                if o.code == code && (name.is_empty() || o.name == name) {
                    return Ok(());
                }
                o.code = code;
                if !name.is_empty() {
                    o.name = name;
                }
            }
            None => owners.push(Owner { id, code, added: unix_now(), name }),
        }
        self.save_owners(&owners)
    }

    /// Removes the owner `id`, with its guest list and connection codes.
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

    /// From an older relay: `owners.txt` without code numbers (device id,
    /// added, note) and one-time keys in `claims.txt`. The owners go into a
    /// code of their own (`旧的主人`), the keys are dropped (a key printed
    /// before was the first code, and nobody joins with it any more).
    pub fn upgrade(&self) -> Result<()> {
        if self.codes_path().exists() {
            return Ok(());
        }
        let claims = self.dir.join("claims.txt");
        let old: Vec<(EndpointId, u64)> = lines(&self.owners_path())?
            .iter()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                let numbered = f.get(1).is_some_and(|s| s.trim().parse::<u32>().is_ok() && f.len() >= 4);
                if numbered {
                    return None;
                }
                Some((f[0].trim().parse().ok()?, f.get(1).and_then(|s| s.trim().parse().ok()).unwrap_or(0)))
            })
            .collect();
        if old.is_empty() {
            let _ = std::fs::remove_file(&claims);
            return Ok(());
        }
        let c = self.new_code("旧的主人")?;
        let owners: Vec<Owner> = old.into_iter().map(|(id, added)| Owner { id, code: c.n, added, name: String::new() }).collect();
        self.save_owners(&owners)?;
        let _ = std::fs::remove_file(&claims);
        Ok(())
    }

    pub fn guests_path(&self) -> PathBuf {
        self.dir.join("guests.txt")
    }

    /// Lines: owner id, guest id (a third field from older versions is left
    /// out). A missing file is no guests.
    pub fn guests(&self) -> Result<BTreeMap<EndpointId, Vec<EndpointId>>> {
        let path = self.guests_path();
        let mut m: BTreeMap<EndpointId, Vec<EndpointId>> = BTreeMap::new();
        for (i, line) in lines(&path)?.iter().enumerate() {
            let f: Vec<&str> = line.split('\t').collect();
            let bad = || anyhow!("{}:{}: bad line", path.display(), i + 1);
            let owner: EndpointId = f[0].trim().parse().map_err(|_| bad())?;
            let id: EndpointId = f.get(1).ok_or_else(bad)?.trim().parse().map_err(|_| bad())?;
            m.entry(owner).or_default().push(id);
        }
        Ok(m)
    }

    /// Replaces `owner`'s guests.
    pub fn set_guests(&self, owner: &EndpointId, guests: &[EndpointId]) -> Result<()> {
        let mut m = self.guests()?;
        let old = m.remove(owner).unwrap_or_default();
        if old == guests {
            return Ok(());
        }
        if !guests.is_empty() {
            m.insert(*owner, guests.to_vec());
        }
        let mut text = String::from("# rc-relay guests: owner id, a device the owner lets use the relay\n");
        for (o, gs) in &m {
            for g in gs {
                text += &format!("{o}\t{g}\n");
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

    fn status_path(&self) -> PathBuf {
        self.dir.join("status.txt")
    }

    /// What the running relay last wrote; empty when it never ran here.
    pub fn status(&self) -> Result<Status> {
        let mut s = Status::default();
        for line in lines(&self.status_path())? {
            let f: Vec<&str> = line.split('\t').collect();
            match (f[0], f.get(1).map(|v| v.trim())) {
                ("started", Some(v)) => s.started = v.parse().unwrap_or(0),
                ("written", Some(v)) => s.written = v.parse().unwrap_or(0),
                ("online", Some(v)) => s.online.extend(v.parse::<EndpointId>().ok()),
                ("seen", Some(v)) => {
                    if let (Ok(id), Some(t)) = (v.parse(), f.get(2).and_then(|t| t.trim().parse().ok())) {
                        s.seen.insert(id, t);
                    }
                }
                _ => {}
            }
        }
        Ok(s)
    }

    pub fn set_status(&self, s: &Status) -> Result<()> {
        let mut text = format!("# rc-relay status, written by the running relay\nstarted\t{}\nwritten\t{}\n", s.started, s.written);
        for id in &s.online {
            text += &format!("online\t{id}\n");
        }
        let owners: Vec<EndpointId> = self.owners()?.iter().map(|o| o.id).collect();
        for (id, t) in s.seen.iter().filter(|(id, _)| owners.contains(id)) {
            text += &format!("seen\t{id}\t{t}\n");
        }
        self.write(&self.status_path(), &text)
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
    fn codes_take_the_smallest_free_number() {
        let s = temp("codes");
        let first = s.first_code().unwrap().unwrap();
        assert_eq!((first.n, first.shown_name()), (1, "码 1".to_string()));
        assert_eq!(s.first_code().unwrap().unwrap(), first, "kept across restarts");
        let two = s.new_code("").unwrap();
        let three = s.new_code("爸妈").unwrap();
        assert_eq!((two.n, three.n, three.shown_name().as_str()), (2, 3, "爸妈"));
        s.delete_code(2).unwrap();
        assert_eq!(s.new_code("").unwrap().n, 2, "a deleted number is used again");
        s.delete_code(1).unwrap();
        s.delete_code(2).unwrap();
        s.delete_code(3).unwrap();
        assert!(s.first_code().unwrap().is_none(), "all deleted on purpose: none made");
        assert_eq!(s.new_code("").unwrap().n, 1);
        s.rename_code(1, "公司\t三楼").unwrap();
        assert_eq!(s.codes().unwrap()[0].shown_name(), "公司 三楼");
        s.rename_code(1, "").unwrap();
        assert_eq!(s.codes().unwrap()[0].shown_name(), "码 1");
        assert!(s.code_with(&s.codes().unwrap()[0].key).unwrap().is_some());
        assert!(s.code_with(&[0; 16]).unwrap().is_none());
    }

    #[test]
    fn owners_belong_to_a_code() {
        let s = temp("owners");
        let (a, b) = (s.new_code("").unwrap(), s.new_code("").unwrap());
        s.join(id(1), a.n, "DESKTOP").unwrap();
        s.join(id(2), a.n, "laptop").unwrap();
        s.join(id(3), b.n, "").unwrap();
        s.join(id(1), a.n, "").unwrap();
        assert_eq!(s.owners().unwrap()[0].name, "DESKTOP", "joining again without a name keeps it");
        s.join(id(2), b.n, "LAPTOP").unwrap();
        let o = s.owners().unwrap();
        assert_eq!((o[1].code, o[1].name.as_str()), (b.n, "LAPTOP"), "joined with another code: moved");
        s.set_guests(&id(3), &[id(9)]).unwrap();
        let mut gone = s.delete_code(b.n).unwrap();
        gone.sort();
        let mut want = vec![id(2), id(3)];
        want.sort();
        assert_eq!(gone, want);
        assert_eq!(s.owners().unwrap().iter().map(|o| o.id).collect::<Vec<_>>(), [id(1)]);
        assert!(s.guests().unwrap().is_empty(), "a removed owner's guests go with it");
        assert!(s.remove_owner(&id(1)).unwrap());
        assert!(!s.remove_owner(&id(1)).unwrap());
        std::fs::write(s.owners_path(), "nonsense\n").unwrap();
        assert!(s.owners().is_err());
    }

    #[test]
    fn an_older_relays_owners_go_into_a_code_of_their_own() {
        let s = temp("upgrade");
        std::fs::create_dir_all(&s.dir).unwrap();
        std::fs::write(s.owners_path(), format!("# old\n{}\t1700000000\tclaimed\n{}\t1700000001\t\n", id(1), id(2))).unwrap();
        std::fs::write(s.dir.join("claims.txt"), "00112233445566778899aabbccddeeff\t0\n").unwrap();
        std::fs::write(s.guests_path(), format!("{}\t{}\tmine\n", id(1), id(5))).unwrap();
        s.upgrade().unwrap();
        let codes = s.codes().unwrap();
        assert_eq!(codes.len(), 1);
        assert_eq!(codes[0].shown_name(), "旧的主人");
        let o = s.owners().unwrap();
        assert_eq!(o.iter().map(|o| (o.id, o.code, o.added)).collect::<Vec<_>>(), [(id(1), 1, 1_700_000_000), (id(2), 1, 1_700_000_001)]);
        assert!(!s.dir.join("claims.txt").exists());
        assert_eq!(s.guests().unwrap()[&id(1)], [id(5)], "the old mark is left out");
        assert!(s.first_code().unwrap().is_some());
        s.upgrade().unwrap();
        assert_eq!(s.codes().unwrap().len(), 1, "once");
        // A relay that never had an owner: the old key is dropped, a code is made.
        let t = temp("upgrade-new");
        std::fs::create_dir_all(&t.dir).unwrap();
        std::fs::write(t.dir.join("claims.txt"), "00112233445566778899aabbccddeeff\t0\n").unwrap();
        t.upgrade().unwrap();
        assert!(!t.dir.join("claims.txt").exists());
        assert_eq!(t.first_code().unwrap().unwrap().n, 1);
    }

    #[test]
    fn guests_per_owner() {
        let s = temp("guests");
        assert!(s.guests().unwrap().is_empty());
        s.set_guests(&id(1), &[id(5), id(6)]).unwrap();
        s.set_guests(&id(2), &[id(6)]).unwrap();
        s.set_guests(&id(1), &[id(7)]).unwrap();
        let m = s.guests().unwrap();
        assert_eq!(m[&id(1)], [id(7)], "the new list replaces the old one");
        assert_eq!(m[&id(2)], [id(6)]);
        s.set_guests(&id(2), &[]).unwrap();
        assert!(!s.guests().unwrap().contains_key(&id(2)));
    }

    #[test]
    fn invites_are_used_once_and_expire() {
        let s = temp("invites");
        let now = unix_now();
        let (a, b) = (Invite { token: [1; 8], expires: now + 600 }, Invite { token: [2; 8], expires: now + 600 });
        let c = s.new_code("").unwrap();
        s.join(id(1), c.n, "").unwrap();
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
}
