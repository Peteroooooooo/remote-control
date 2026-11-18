//! Short text codes that people copy around or that travel in headers:
//! `rc1…` connection codes ([`conn_code`]), `rcr1…` relay codes
//! ([`relay_code`]) and `rcd1…` device proofs ([`proof`]). Each is a prefix
//! plus Base58 (Bitcoin alphabet: no 0, O, I, l, so a double click selects
//! the whole code) of a versioned payload and a 4-byte checksum ([`text`]).

pub mod conn_code;
pub mod proof;
pub mod relay_code;
pub mod text;
pub mod wire;

pub use iroh_base::{PublicKey, RelayUrl, SecretKey, Signature};

/// Why a code was not accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Not the kind of code expected (another prefix).
    WrongPrefix,
    /// A character outside the Base58 alphabet.
    NotBase58,
    /// Mistyped or cut short: the checksum does not match.
    Checksum,
    /// Made by a newer (or unknown) version.
    Version(u8),
    /// The payload ends early or has bytes left over.
    Length,
    /// A field holds an impossible value.
    Field(&'static str),
    /// A device proof whose signature does not match.
    Signature,
    /// A device proof past its expiry.
    Expired,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::WrongPrefix => write!(f, "not this kind of code"),
            Error::NotBase58 => write!(f, "not a Base58 code"),
            Error::Checksum => write!(f, "checksum mismatch (mistyped or incomplete)"),
            Error::Version(v) => write!(f, "unknown code version {v}"),
            Error::Length => write!(f, "wrong length"),
            Error::Field(name) => write!(f, "invalid {name}"),
            Error::Signature => write!(f, "bad signature"),
            Error::Expired => write!(f, "expired"),
        }
    }
}

impl std::error::Error for Error {}
