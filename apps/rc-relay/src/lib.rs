//! Self-hosted relay for remote-control.
//!
//! It is iroh-relay with an access policy: it only forwards between devices
//! on its roster (its owners and the guests each owner lists), and only if
//! one end is an owner ([`policy`]); a device that brings one of an owner's
//! unused connection codes may meet that owner too. Owners are added by
//! claiming the relay with a relay code (`rcr1…`, [`api`]); the first code
//! is printed when the relay starts with no owners.
//!
//! Without the default `server` feature this crate is only what the app
//! needs: [`client`] (claim, ask for a code for another computer, send the
//! guest list and connection codes, bring a code) and the request formats.

pub mod api;
pub mod client;
pub mod tls;

#[cfg(feature = "server")]
pub mod cli;
#[cfg(feature = "server")]
pub mod config;
#[cfg(feature = "server")]
pub mod policy;
#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "server")]
pub mod state;
#[cfg(feature = "server")]
pub mod tui;
