//! Certificates of self-hosted relays without a domain name. Such a relay
//! makes its own certificate (see `config`) and puts its fingerprint into
//! every address it hands out: `https://203.0.113.7:7843/?rc-cert=<Base58>`.
//! A client that has seen such an address trusts that certificate for that
//! host; every other certificate is checked the usual way (Mozilla's roots).
//! The address travels in relay codes, connection codes and between
//! devices, so whoever may use the relay learns the fingerprint along with
//! the address.

use std::{
    collections::BTreeSet,
    sync::{Arc, RwLock},
};

use iroh_base::RelayUrl;
use iroh_relay::tls::CaTlsConfig;
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, UnixTime},
};

/// The query key of the fingerprint in a relay URL.
pub const PIN_KEY: &str = "rc-cert";

/// (host, fingerprint): the certificate trusted for that host.
static PINS: RwLock<BTreeSet<(String, [u8; 32])>> = RwLock::new(BTreeSet::new());

/// The fingerprint of a certificate (DER).
pub fn fingerprint(cert_der: &[u8]) -> [u8; 32] {
    *blake3::hash(cert_der).as_bytes()
}

/// `url` with the fingerprint of `cert_der` in it.
pub fn pinned_url(url: &RelayUrl, cert_der: &[u8]) -> RelayUrl {
    let mut u: url::Url = (**url).clone();
    u.query_pairs_mut().clear().append_pair(PIN_KEY, &bs58::encode(fingerprint(cert_der)).into_string());
    RelayUrl::from(u)
}

/// The fingerprint in `url`, if it has one.
pub fn pin_of(url: &RelayUrl) -> Option<[u8; 32]> {
    let (_, v) = url.query_pairs().find(|(k, _)| k == PIN_KEY)?;
    bs58::decode(v.as_ref()).into_vec().ok()?.try_into().ok()
}

/// A host as TLS names it: an IP address without brackets, a name in lower case without the final dot.
fn host_key(url: &RelayUrl) -> Option<String> {
    Some(match url.host()? {
        url::Host::Ipv4(a) => a.to_string(),
        url::Host::Ipv6(a) => a.to_string(),
        url::Host::Domain(d) => d.trim_end_matches('.').to_ascii_lowercase(),
    })
}

fn name_key(name: &ServerName<'_>) -> String {
    match name {
        ServerName::IpAddress(ip) => std::net::IpAddr::from(*ip).to_string(),
        other => other.to_str().trim_end_matches('.').to_ascii_lowercase(),
    }
}

/// From now on, trust the certificate pinned in `url` (if any) for its host.
pub fn learn(url: &RelayUrl) {
    if let (Some(p), Some(h)) = (pin_of(url), host_key(url)) {
        let key = (h, p);
        if !PINS.read().unwrap().contains(&key) {
            PINS.write().unwrap().insert(key);
        }
    }
}

pub fn learn_all<'a>(urls: impl IntoIterator<Item = &'a RelayUrl>) {
    urls.into_iter().for_each(learn);
}

/// What every TLS connection of the app and of `rc-relay`'s client side
/// trusts: the pinned certificates, else Mozilla's roots.
pub fn ca() -> CaTlsConfig {
    CaTlsConfig::custom_server_cert_verifier(Arc::new(|provider: Arc<CryptoProvider>| {
        let web = CaTlsConfig::embedded().server_cert_verifier(provider.clone())?;
        Ok(Arc::new(Pinned { web, provider }) as Arc<dyn ServerCertVerifier>)
    }))
}

#[derive(Debug)]
struct Pinned {
    web: Arc<dyn ServerCertVerifier>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if PINS.read().unwrap().contains(&(name_key(server_name), fingerprint(end_entity))) {
            return Ok(ServerCertVerified::assertion());
        }
        self.web.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_ride_in_the_url() {
        let url: RelayUrl = "https://203.0.113.7:7843".parse().unwrap();
        assert_eq!(pin_of(&url), None);
        let p = pinned_url(&url, b"certificate");
        assert_eq!(pin_of(&p), Some(fingerprint(b"certificate")));
        assert_eq!(p.host_str(), Some("203.0.113.7"));
        assert_eq!(p.port(), Some(7843));
        assert!(p.as_str().starts_with("https://203.0.113.7:7843/?rc-cert="), "{p}");
        let back: RelayUrl = p.to_string().parse().unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn pins_are_per_host() {
        let key = |u: &str| host_key(&u.parse().unwrap()).unwrap();
        let name = |s: &str| name_key(&ServerName::try_from(s.to_string()).unwrap());
        assert_eq!(key("https://203.0.113.7:7843"), name("203.0.113.7"));
        assert_eq!(key("https://[2001:db8::7]:7843"), name("2001:db8::7"));
        assert_eq!(key("https://Relay.Example.com./"), name("relay.example.com"));
    }
}
