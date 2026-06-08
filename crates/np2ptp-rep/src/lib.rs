//! `np2ptp-rep` — reputation and incentives for NP2PTP.
//!
//! Three pieces that together replace BitTorrent's memoryless tit-for-tat:
//!
//! * [`identity`] — an Ed25519 keypair whose public key is a peer's durable id.
//! * [`receipt`] — signed, portable proof that one peer served bytes to another.
//! * [`ledger`] — persistent per-peer accounting that ranks peers for
//!   choke/unchoke, so past contribution keeps mattering and leeches are
//!   deprioritized.
//!
//! This crate is policy, not transport: `np2ptp-net` will feed it observed
//! transfers and consult it when deciding whom to serve.

pub mod identity;
pub mod ledger;
pub mod receipt;

pub use identity::{Identity, PeerId};
pub use ledger::{Counters, Ledger, LedgerError};
pub use receipt::Receipt;
