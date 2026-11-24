//! The device list and the settings, kept as small text files next to the
//! device key (`devices.txt`, `settings.txt`). Temporary devices (met on the
//! LAN or with a connection code and not trusted) are never written here.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use iroh::{EndpointId, RelayUrl};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: EndpointId,
    /// What its card shows; the user may change it.
    pub name: String,
    /// It may control this computer without asking ("my computer"); off for
    /// a trusted device, which gets a confirmation each time.
    pub auto_accept: bool,
    /// The addresses of its home relay when last heard from (its own relay,
    /// one address per IP family, or a public one).
    pub relays: Vec<RelayUrl>,
    /// Where it was last reached directly.
    pub addrs: Vec<SocketAddr>,
    /// Deleted here, kept until the other side has been told.
    pub forgotten: bool,
    /// Added here, the other side not told yet.
    pub tell: bool,
}

impl Device {
    pub fn new(id: EndpointId) -> Device {
        Device { id, name: String::new(), auto_accept: false, relays: Vec::new(), addrs: Vec::new(), forgotten: false, tell: false }
    }
}

pub struct Store {
    path: PathBuf,
    pub devices: Vec<Device>,
}

pub fn devices_path(key: &Path) -> PathBuf {
    key.with_file_name("devices.txt")
}

pub fn settings_path(key: &Path) -> PathBuf {
    key.with_file_name("settings.txt")
}

const HEADER: &str = "# remote-control devices v2: id, flags (a = may control without asking, f = deleted, other side not told yet, t = added, other side not told yet), name, relay addresses, addresses";

/// Names go in a tab-separated line.
pub fn clean(name: &str) -> String {
    name.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().trim().chars().take(64).collect()
}

fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("data");
    let tmp = path.with_file_name(format!(".{name}.{}.{n}.tmp", std::process::id()));
    let res = (|| -> Result<()> {
        let mut f = std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(content).with_context(|| format!("write {}", tmp.display()))?;
        f.sync_all().with_context(|| format!("sync {}", tmp.display()))?;
        drop(f);
        std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

fn addrs(field: Option<&str>) -> Vec<SocketAddr> {
    field.map_or(Vec::new(), |a| a.split(',').filter_map(|s| s.trim().parse().ok()).collect())
}

/// Relay URLs separated by commas or spaces, bad ones left out. Their
/// certificate pins (see `rc_relay::tls`) are trusted from now on.
pub fn relay_urls(field: &str) -> Vec<RelayUrl> {
    let mut v: Vec<RelayUrl> = Vec::new();
    for u in field.split([',', ' ']).filter_map(|s| s.trim().parse::<RelayUrl>().ok()) {
        if !v.contains(&u) {
            v.push(u);
        }
    }
    rc_relay::tls::learn_all(&v);
    v
}

pub fn url_list(urls: &[RelayUrl]) -> String {
    urls.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",")
}

impl Store {
    /// A missing file is an empty list. A list from before v2 (phase 1
    /// pairing) is read as: paired to control this computer = my computer,
    /// other paired devices = trusted; the services' own ids are dropped
    /// (a service now uses its computer's device key).
    pub fn load(path: &Path) -> Result<Store> {
        let mut devices = Vec::new();
        if let Ok(text) = std::fs::read_to_string(path) {
            let v2 = text.starts_with("# remote-control devices v2");
            for line in text.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')) {
                let f: Vec<&str> = line.split('\t').collect();
                let id: EndpointId =
                    f[0].trim().parse().map_err(|e| anyhow!("{}: bad id {}: {e}", path.display(), f[0]))?;
                let flags = f.get(1).copied().unwrap_or("");
                let name = f.get(2).map_or(String::new(), |n| clean(n));
                let d = if v2 {
                    Device {
                        id,
                        name,
                        auto_accept: flags.contains('a'),
                        relays: relay_urls(f.get(3).copied().unwrap_or("")),
                        addrs: addrs(f.get(4).copied()),
                        forgotten: flags.contains('f'),
                        tell: flags.contains('t'),
                    }
                } else {
                    if flags.contains('s') {
                        continue;
                    }
                    Device { name, auto_accept: flags.contains('c'), addrs: addrs(f.get(3).copied()), ..Device::new(id) }
                };
                if !devices.iter().any(|x: &Device| x.id == id) {
                    devices.push(d);
                }
            }
        }
        Ok(Store { path: path.to_path_buf(), devices })
    }

    /// An empty list that saves to `path`.
    pub fn empty(path: &Path) -> Store {
        Store { path: path.to_path_buf(), devices: Vec::new() }
    }

    pub fn save(&self) -> Result<()> {
        let mut text = format!("{HEADER}\n");
        for d in &self.devices {
            let flags = format!(
                "{}{}{}",
                if d.auto_accept { "a" } else { "" },
                if d.forgotten { "f" } else { "" },
                if d.tell { "t" } else { "" }
            );
            let addrs: Vec<String> = d.addrs.iter().map(|a| a.to_string()).collect();
            let relay = url_list(&d.relays);
            text += &format!("{}\t{flags}\t{}\t{relay}\t{}\n", d.id, clean(&d.name), addrs.join(","));
        }
        write_atomic(&self.path, text.as_bytes())
    }

    /// A device in the list (deleted ones are not).
    pub fn get(&self, id: &EndpointId) -> Option<&Device> {
        self.devices.iter().find(|d| d.id == *id && !d.forgotten)
    }

    pub fn get_mut(&mut self, id: &EndpointId) -> Option<&mut Device> {
        self.devices.iter_mut().find(|d| d.id == *id && !d.forgotten)
    }

    /// Whether `id` was deleted here and has not been told yet.
    pub fn is_forgotten(&self, id: &EndpointId) -> bool {
        self.devices.iter().any(|d| d.id == *id && d.forgotten)
    }

    /// Puts `id` in the list (back, if it was deleted) and returns it.
    pub fn add(&mut self, id: EndpointId) -> &mut Device {
        let i = match self.devices.iter().position(|d| d.id == id) {
            Some(i) => {
                if self.devices[i].forgotten {
                    self.devices[i] = Device { name: self.devices[i].name.clone(), ..Device::new(id) };
                }
                i
            }
            None => {
                self.devices.push(Device::new(id));
                self.devices.len() - 1
            }
        };
        &mut self.devices[i]
    }

    /// Deletes `id`, keeping a mark to tell it later. Returns whether it was listed.
    pub fn forget(&mut self, id: &EndpointId) -> bool {
        match self.devices.iter_mut().find(|d| d.id == *id && !d.forgotten) {
            Some(d) => {
                d.forgotten = true;
                d.auto_accept = false;
                d.tell = false;
                true
            }
            None => false,
        }
    }

    /// Removes every trace of `id` (the other side knows, or deleted us first).
    pub fn remove(&mut self, id: &EndpointId) -> bool {
        let before = self.devices.len();
        self.devices.retain(|d| d.id != *id);
        self.devices.len() != before
    }

    pub fn listed(&self) -> impl Iterator<Item = &Device> {
        self.devices.iter().filter(|d| !d.forgotten)
    }
}

/// Settings. Unknown lines are left out; missing values take defaults.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Shown to other devices; empty = the computer name.
    pub name: String,
    /// This computer's own (self-hosted) relays, each with its addresses
    /// (an IPv4 and an IPv6 one, or a name).
    pub relays: Vec<Vec<RelayUrl>>,
    /// Use the free public relays to meet and to find a direct way.
    pub public_relays: bool,
    /// Answer LAN discovery for strangers and accept incoming LAN connections from them.
    pub lan_visible: bool,
    /// Automatically start service when Windows boots.
    pub boot_online: bool,
    /// Allow remote control while the computer is locked.
    pub lock_control: bool,
    /// Prevent computer from sleeping while on mains power.
    pub stay_awake: bool,
    /// UI language preference (auto follows Windows UI language).
    pub language: crate::i18n::LangPref,
    /// 只被控: the lobby leaves out every way of controlling other devices.
    pub host_only: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            name: String::new(),
            relays: Vec::new(),
            public_relays: true,
            lan_visible: true,
            boot_online: true,
            lock_control: true,
            stay_awake: true,
            language: crate::i18n::LangPref::Auto,
            host_only: false,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Settings {
        let mut s = Settings::default();
        // A relay another computer gave this one (older versions): not this
        // computer's own, so it goes; paste the relay's code to use it.
        let mut given: Vec<Vec<RelayUrl>> = Vec::new();
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                match line.split_once('=') {
                    Some(("name", v)) => s.name = clean(v),
                    Some(("relay", v)) => {
                        let urls = relay_urls(v);
                        if !urls.is_empty() && !s.relays.iter().flatten().any(|u| urls.contains(u)) {
                            s.relays.push(urls);
                        }
                    }
                    Some(("granted", v)) => given.push(relay_urls(v.split_once(' ').map_or(v, |(u, _)| u))),
                    Some(("public_relays", v)) => s.public_relays = v.trim() != "0",
                    Some(("lan_visible", v)) => s.lan_visible = v.trim() != "0",
                    Some(("boot_online", v)) => s.boot_online = v.trim() != "0",
                    Some(("lock_control", v)) => s.lock_control = v.trim() != "0",
                    Some(("stay_awake", v)) => s.stay_awake = v.trim() != "0",
                    Some(("language", v)) => s.language = crate::i18n::LangPref::parse(v),
                    Some(("host_only", v)) => s.host_only = v.trim() == "1",
                    _ => {}
                }
            }
        }
        s.relays.retain(|r| !given.contains(r));
        s
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut text = format!("name={}\n", clean(&self.name));
        for r in &self.relays {
            text += &format!("relay={}\n", url_list(r));
        }
        text += &format!(
            "public_relays={}\nlan_visible={}\nboot_online={}\nlock_control={}\nstay_awake={}\nlanguage={}\nhost_only={}\n",
            self.public_relays as u8,
            self.lan_visible as u8,
            self.boot_online as u8,
            self.lock_control as u8,
            self.stay_awake as u8,
            self.language.as_str(),
            self.host_only as u8,
        );
        write_atomic(path, text.as_bytes())
    }

    /// The name other devices see: the one set here, else the computer name.
    pub fn shown_name(&self) -> String {
        if !self.name.is_empty() {
            return self.name.clone();
        }
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> EndpointId {
        iroh::SecretKey::from_bytes(&[n; 32]).public()
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rc-store-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn devices_round_trip() {
        let dir = temp("rt");
        let p = devices_path(&dir.join("device.key"));
        let mut s = Store::load(&p).unwrap();
        assert!(s.devices.is_empty());
        {
            let d = s.add(id(1));
            d.name = "书房\t台式机\n".into();
            d.auto_accept = true;
        }
        {
            let d = s.add(id(2));
            d.addrs = vec!["192.168.0.5:51234".parse().unwrap(), "[2001:db8::1]:7".parse().unwrap()];
            d.relays = vec![
                "https://203.0.113.7:7843/?rc-cert=abc".parse().unwrap(),
                "https://[2001:db8::7]:7843/?rc-cert=abc".parse().unwrap(),
            ];
            d.tell = true;
        }
        s.add(id(3));
        assert!(s.forget(&id(3)));
        s.save().unwrap();
        let t = Store::load(&p).unwrap();
        assert_eq!(t.devices[1..], s.devices[1..]);
        assert_eq!(t.get(&id(1)).unwrap().name, "书房 台式机");
        assert!(t.get(&id(1)).unwrap().auto_accept);
        assert!(t.get(&id(3)).is_none() && t.is_forgotten(&id(3)));
        assert_eq!(t.listed().count(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn forget_add_remove() {
        let dir = temp("fr");
        let mut s = Store::load(&devices_path(&dir.join("device.key"))).unwrap();
        s.add(id(4)).auto_accept = true;
        s.get_mut(&id(4)).unwrap().name = "笔记本".into();
        assert!(s.forget(&id(4)));
        assert!(!s.forget(&id(4)));
        assert!(s.get(&id(4)).is_none());
        // Met again: back in the list, without its old rights.
        let d = s.add(id(4));
        assert!(!d.forgotten && !d.auto_accept);
        assert_eq!(d.name, "笔记本");
        assert!(s.remove(&id(4)));
        assert!(!s.remove(&id(4)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn phase_one_list_is_migrated() {
        let dir = temp("old");
        let p = dir.join("devices.txt");
        std::fs::write(
            &p,
            format!(
                "# id, flags (c = may control this computer, h = paired host, s = a service), name, addresses, twin id\n{}\tch\tlaptop\t192.168.1.87:5000\t{}\n{}\ths\tLAPTOP\t\t{}\n{}\th\t书房\t\t\n",
                id(5),
                id(6),
                id(6),
                id(5),
                id(7)
            ),
        )
        .unwrap();
        let s = Store::load(&p).unwrap();
        assert_eq!(s.devices.len(), 2);
        assert!(s.get(&id(5)).unwrap().auto_accept);
        assert_eq!(s.get(&id(5)).unwrap().addrs, vec!["192.168.1.87:5000".parse().unwrap()]);
        assert!(s.get(&id(6)).is_none());
        assert!(!s.get(&id(7)).unwrap().auto_accept);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bad_lines() {
        let dir = temp("bad");
        let p = dir.join("devices.txt");
        std::fs::write(&p, "# comment\n\nnot-an-id\tc\tx\t\n").unwrap();
        assert!(Store::load(&p).is_err());
        std::fs::write(&p, format!("{}\n", id(5))).unwrap();
        let s = Store::load(&p).unwrap();
        assert_eq!(s.devices.len(), 1);
        assert!(!s.devices[0].auto_accept);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_defaults_and_round_trip() {
        let dir = temp("set");
        let p = settings_path(&dir.join("device.key"));
        assert_eq!(Settings::load(&p), Settings::default());
        assert!(Settings::default().public_relays);
        assert!(Settings::default().lan_visible);
        assert!(Settings::default().boot_online);
        assert!(Settings::default().lock_control);
        assert!(Settings::default().stay_awake);
        assert!(!Settings::default().host_only);
        // A phase 1 file, and switches that are gone.
        std::fs::write(&p, "name=客厅\nallow_control=1\naccept_new=0\nclipboard=0\n").unwrap();
        assert_eq!(Settings::load(&p), Settings { name: "客厅".into(), ..Settings::default() });
        let s = Settings {
            name: "客厅".into(),
            relays: vec![
                vec!["https://203.0.113.7:7843".parse().unwrap(), "https://[2001:db8::7]:7843".parse().unwrap()],
                vec!["https://203.0.113.8:7843".parse().unwrap()],
            ],
            public_relays: false,
            lan_visible: false,
            boot_online: true,
            lock_control: false,
            stay_awake: true,
            language: crate::i18n::LangPref::En,
            host_only: true,
        };
        s.save(&p).unwrap();
        assert_eq!(Settings::load(&p), s);
        assert_eq!(s.shown_name(), "客厅");
        // A relay another computer gave this one (older versions) goes.
        std::fs::write(&p, format!("relay=https://203.0.113.7:7843\nrelay=https://203.0.113.9:7843\ngranted=https://203.0.113.9:7843 {}\n", id(1))).unwrap();
        assert_eq!(Settings::load(&p).relays, vec![vec!["https://203.0.113.7:7843".parse::<RelayUrl>().unwrap()]]);
        // An old file with a single relay line or an empty relay line.
        std::fs::write(&p, "relay=\npublic_relays=1\n").unwrap();
        assert_eq!(Settings::load(&p).relays, Vec::<Vec<RelayUrl>>::new());
        std::fs::write(&p, "relay=https://203.0.113.7:7843\n").unwrap();
        assert_eq!(Settings::load(&p).relays, vec![vec!["https://203.0.113.7:7843".parse().unwrap()]]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
