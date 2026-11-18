//! One device key per computer, online in one process only: the service's
//! helper when the service is installed, else the lobby. A session window
//! connects with a key of its own and shows a proof, signed with the device
//! key, that it speaks for the device; the host then treats it as the device
//! (its confirmation, "my computer" rights). See `rc_ident::proof`.

use anyhow::{Result, anyhow};
use iroh::{EndpointId, SecretKey};
use rc_ident::proof::{DeviceProof, unix_now};

/// A session may start this long after its window asked for the proof.
const PROOF_LIFETIME_S: u64 = 12 * 3600;

pub fn issue(device: &SecretKey, session: EndpointId) -> String {
    DeviceProof::issue(device, session, unix_now() + PROOF_LIFETIME_S).encode()
}

/// The device behind a connection made with key `remote`: the one its proof
/// names (the proof must be for `remote`), or `remote` itself without one.
pub fn identify(remote: EndpointId, proof: &str) -> Result<EndpointId> {
    if proof.is_empty() {
        return Ok(remote);
    }
    let p = DeviceProof::check(proof, &remote, unix_now()).map_err(|e| anyhow!("device proof: {e}"))?;
    Ok(p.device)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proofs_name_the_device() {
        let device = SecretKey::from_bytes(&[1; 32]);
        let session = SecretKey::from_bytes(&[2; 32]).public();
        let p = issue(&device, session);
        assert_eq!(identify(session, &p).unwrap(), device.public());
        assert_eq!(identify(session, "").unwrap(), session);
        // Someone else showing the proof.
        assert!(identify(SecretKey::from_bytes(&[3; 32]).public(), &p).is_err());
        assert!(identify(session, "rcd1nonsense").is_err());
    }
}
