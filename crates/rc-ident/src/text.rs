//! `<prefix><Base58(payload ‖ check)>`, where `check` is the first 4 bytes of
//! BLAKE3 over prefix and payload: a code with the wrong prefix, a typo or a
//! missing piece fails the check instead of decoding to something else.

use crate::Error;

const CHECK_LEN: usize = 4;

fn check(prefix: &str, payload: &[u8]) -> [u8; CHECK_LEN] {
    let mut h = blake3::Hasher::new();
    h.update(prefix.as_bytes());
    h.update(payload);
    let mut out = [0; CHECK_LEN];
    out.copy_from_slice(&h.finalize().as_bytes()[..CHECK_LEN]);
    out
}

pub fn encode(prefix: &str, payload: &[u8]) -> String {
    let mut v = payload.to_vec();
    v.extend_from_slice(&check(prefix, payload));
    format!("{prefix}{}", bs58::encode(v).into_string())
}

/// The payload of `text` (surrounding whitespace ignored).
pub fn decode(prefix: &str, text: &str) -> Result<Vec<u8>, Error> {
    let body = text.trim().strip_prefix(prefix).ok_or(Error::WrongPrefix)?;
    let mut v = bs58::decode(body).into_vec().map_err(|_| Error::NotBase58)?;
    if v.len() < CHECK_LEN {
        return Err(Error::Checksum);
    }
    let sum = v.split_off(v.len() - CHECK_LEN);
    if sum != check(prefix, &v) {
        return Err(Error::Checksum);
    }
    Ok(v)
}

fn is_base58(c: char) -> bool {
    c.is_ascii_alphanumeric() && !matches!(c, '0' | 'O' | 'I' | 'l')
}

/// The first code with `prefix` inside `text` (a chat message, say) whose
/// checksum is right. A code starts where the text has no letter or digit
/// right before it and runs as long as Base58 characters follow.
pub fn find<'a>(prefix: &str, text: &'a str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(i) = text[from..].find(prefix).map(|i| i + from) {
        from = i + prefix.len();
        if text[..i].chars().next_back().is_some_and(|c| c.is_ascii_alphanumeric()) {
            continue;
        }
        let len = text[from..].find(|c: char| !is_base58(c)).unwrap_or(text.len() - from);
        let code = &text[i..from + len];
        if decode(prefix, code).is_ok() {
            return Some(code);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_checks() {
        let code = encode("rcx1", b"hello world");
        assert!(code.starts_with("rcx1"));
        assert!(code[4..].chars().all(is_base58));
        assert_eq!(decode("rcx1", &code).unwrap(), b"hello world");
        assert_eq!(decode("rcx1", &format!("  {code}\n")).unwrap(), b"hello world");
        assert_eq!(decode("rcy1", &code), Err(Error::WrongPrefix));
        // Same body under another prefix: the checksum covers the prefix.
        assert_eq!(decode("rcy1", &code.replacen("rcx1", "rcy1", 1)), Err(Error::Checksum));
        assert_eq!(decode("rcx1", &code[..code.len() - 1]), Err(Error::Checksum));
        assert_eq!(decode("rcx1", &format!("{code}0")), Err(Error::NotBase58));
        assert_eq!(decode("rcx1", "rcx1"), Err(Error::Checksum));
    }

    #[test]
    fn any_single_changed_character_is_caught() {
        let code = encode("rcx1", &[7u8; 40]);
        let alphabet: Vec<char> = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz".chars().collect();
        for i in 4..code.len() {
            for &c in &alphabet {
                let mut s: Vec<char> = code.chars().collect();
                if s[i] == c {
                    continue;
                }
                s[i] = c;
                let s: String = s.into_iter().collect();
                assert!(decode("rcx1", &s).is_err(), "{s} passed");
            }
        }
    }

    #[test]
    fn finds_a_code_in_a_message() {
        let code = encode("rcx1", b"payload");
        let msg = format!("连这个：{code}，谢谢");
        assert_eq!(find("rcx1", &msg), Some(code.as_str()));
        assert_eq!(find("rcx1", &format!("{code}")), Some(code.as_str()));
        // Glued to a word, or cut short: not a code.
        assert_eq!(find("rcx1", &format!("abc{code}")), None);
        assert_eq!(find("rcx1", &code[..code.len() - 2]), None);
        // A broken copy first, a good one later.
        let msg = format!("{} 不对，用这个 {code}", &code[..code.len() - 3]);
        assert_eq!(find("rcx1", &msg), Some(code.as_str()));
    }
}
