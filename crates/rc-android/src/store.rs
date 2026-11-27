//! Persistent device key, saved computers, and claimed self-hosted relays on Android.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use iroh::{EndpointId, RelayUrl, SecretKey};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SavedDevice {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub addrs: Vec<String>,
    #[serde(default)]
    pub home_relays: Vec<String>,
    #[serde(default)]
    pub remembered: bool,
    #[serde(default)]
    pub tell_remember: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StoreData {
    #[serde(default)]
    pub devices: Vec<SavedDevice>,
    /// Each entry is one self-hosted relay (1..=4 URLs: IPv4 / IPv6 / domain).
    #[serde(default)]
    pub own_relays: Vec<Vec<String>>,
    #[serde(default)]
    pub forgotten: Vec<String>,
}

pub struct Store {
    dir: PathBuf,
    pub data: StoreData,
}

impl Store {
    pub fn load(dir: &Path) -> Result<(SecretKey, Self)> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create files dir {}", dir.display()))?;
        let key_path = dir.join("device.key");
        let key = if let Ok(bytes) = std::fs::read(&key_path) {
            let arr: [u8; 32] = bytes
                .as_slice()
                .try_into()
                .context("device.key must be 32 bytes")?;
            SecretKey::from_bytes(&arr)
        } else {
            let k = SecretKey::generate();
            std::fs::write(&key_path, k.to_bytes())
                .with_context(|| format!("write {}", key_path.display()))?;
            k
        };

        let store_path = dir.join("store.json");
        let data = match std::fs::read_to_string(&store_path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => StoreData::default(),
        };

        Ok((
            key,
            Self {
                dir: dir.to_path_buf(),
                data,
            },
        ))
    }

    pub fn save(&self) {
        let store_path = self.dir.join("store.json");
        if let Ok(json) = serde_json::to_string_pretty(&self.data) {
            let _ = std::fs::write(store_path, json);
        }
    }

    pub fn own_relay_urls(&self) -> Vec<RelayUrl> {
        self.data
            .own_relays
            .iter()
            .flat_map(|g| g.iter())
            .filter_map(|s| s.parse::<RelayUrl>().ok())
            .collect()
    }

    pub fn add_own_relay(&mut self, urls: &[RelayUrl]) {
        if urls.is_empty() {
            return;
        }
        let strings: Vec<String> = urls.iter().map(ToString::to_string).collect();
        self.data
            .own_relays
            .retain(|g| !g.iter().any(|u| strings.contains(u)));
        self.data.own_relays.insert(0, strings);
        self.save();
    }

    pub fn upsert_device(
        &mut self,
        id: &EndpointId,
        name: Option<&str>,
        direct: Option<SocketAddr>,
        home: &[RelayUrl],
        remembered: Option<bool>,
    ) {
        let id_str = id.to_string();
        let entry = match self.data.devices.iter_mut().find(|d| d.id == id_str) {
            Some(d) => d,
            None => {
                self.data.devices.push(SavedDevice {
                    id: id_str,
                    name: String::new(),
                    addrs: Vec::new(),
                    home_relays: Vec::new(),
                    remembered: false,
                    tell_remember: false,
                });
                self.data.devices.last_mut().unwrap()
            }
        };
        if let Some(n) = name
            && !n.is_empty()
        {
            entry.name = n.to_string();
        }
        if let Some(a) = direct {
            let s = a.to_string();
            entry.addrs.retain(|x| x != &s);
            entry.addrs.insert(0, s);
            entry.addrs.truncate(4);
        }
        if !home.is_empty() {
            entry.home_relays = home.iter().map(ToString::to_string).collect();
        }
        if let Some(r) = remembered {
            entry.remembered = r;
            if !r {
                entry.tell_remember = false;
            }
        }
        let id_check = id.to_string();
        self.data.forgotten.retain(|x| x != &id_check);
        self.save();
    }

    pub fn is_remembered(&self, id: &EndpointId) -> bool {
        let id_str = id.to_string();
        self.data
            .devices
            .iter()
            .any(|d| d.id == id_str && d.remembered)
    }

    pub fn set_tell_remember(&mut self, id: &EndpointId, tell: bool) {
        let id_str = id.to_string();
        if let Some(d) = self.data.devices.iter_mut().find(|d| d.id == id_str)
            && d.tell_remember != tell
        {
            d.tell_remember = tell;
            self.save();
        }
    }

    pub fn is_forgotten(&self, id: &EndpointId) -> bool {
        let id_str = id.to_string();
        self.data.forgotten.iter().any(|x| x == &id_str)
    }

    pub fn mark_forgotten(&mut self, id: &EndpointId) {
        let id_str = id.to_string();
        self.data.devices.retain(|d| d.id != id_str);
        if !self.data.forgotten.iter().any(|x| x == &id_str) {
            self.data.forgotten.push(id_str);
        }
        self.save();
    }

    pub fn clear_forgotten(&mut self, id: &EndpointId) {
        let id_str = id.to_string();
        let before = self.data.forgotten.len();
        self.data.forgotten.retain(|x| x != &id_str);
        if self.data.forgotten.len() != before {
            self.save();
        }
    }

    pub fn remove_device(&mut self, id: &EndpointId) {
        let id_str = id.to_string();
        self.data.devices.retain(|d| d.id != id_str);
        self.data.forgotten.retain(|x| x != &id_str);
        self.save();
    }
}
