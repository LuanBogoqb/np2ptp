//! The contribution ledger: per-peer accounting that drives choke/unchoke.
//!
//! BitTorrent's tit-for-tat favors peers who are currently uploading to you, but
//! forgets them as soon as the connection ends — so seeding earns nothing
//! lasting and the system leans on altruism. Here every peer's give-and-take is
//! remembered and (optionally) persisted, so a peer that has served you in the
//! past keeps priority later, and a pure leech is deprioritized.
//!
//! [`Ledger`] is generic over the peer-key type `K`: the `np2ptp-rep` tests use
//! the Ed25519 [`PeerId`], while `np2ptp-net` keys it by the libp2p `PeerId`.

use std::collections::HashMap;
use std::fs;
use std::hash::Hash;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::identity::PeerId;
use crate::receipt::Receipt;

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization: {0}")]
    Codec(#[from] bincode::Error),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counters {
    /// Bytes this peer has served *to us*.
    pub served_to_us: u64,
    /// Bytes *we* have served to this peer.
    pub we_served: u64,
    /// Bytes credited to this peer by valid third-party receipts we've seen.
    pub credited_by_receipts: u64,
}

/// Per-peer contribution accounting, optionally persisted to disk.
pub struct Ledger<K> {
    peers: HashMap<K, Counters>,
    path: Option<PathBuf>,
}

impl<K> Default for Ledger<K> {
    fn default() -> Self {
        Ledger { peers: HashMap::new(), path: None }
    }
}

impl<K> Ledger<K>
where
    K: Eq + Hash + Clone + Ord,
{
    pub fn new() -> Ledger<K> {
        Ledger::default()
    }

    /// Record that `from` served us `bytes` (call when a download chunk arrives).
    pub fn record_received(&mut self, from: K, bytes: u64) {
        self.peers.entry(from).or_default().served_to_us += bytes;
    }

    /// Record that we served `to` `bytes` (call when we upload a chunk).
    pub fn record_served(&mut self, to: K, bytes: u64) {
        self.peers.entry(to).or_default().we_served += bytes;
    }

    /// Credit `peer` with `bytes` on the strength of a receipt whose
    /// cryptographic validity the caller has already checked — this method
    /// itself does no verification, so callers must call it only after
    /// confirming the receipt is genuinely about `peer`.
    pub fn credit_receipt(&mut self, peer: K, bytes: u64) {
        self.peers.entry(peer).or_default().credited_by_receipts += bytes;
    }

    pub fn counters(&self, peer: &K) -> Counters {
        self.peers.get(peer).copied().unwrap_or_default()
    }

    /// Sum of every peer's counters — one aggregate figure (e.g. "total bytes
    /// served across all peers") instead of per-peer detail.
    pub fn totals(&self) -> Counters {
        let mut total = Counters::default();
        for c in self.peers.values() {
            total.served_to_us += c.served_to_us;
            total.we_served += c.we_served;
            total.credited_by_receipts += c.credited_by_receipts;
        }
        total
    }

    /// Reciprocity score: how much a peer has given us (directly, or vouched
    /// for by a valid third-party receipt) beyond what we've given them.
    /// Positive = net giver (favor it), negative = net taker (choke it).
    pub fn reputation(&self, peer: &K) -> i64 {
        let c = self.counters(peer);
        c.served_to_us as i64 + c.credited_by_receipts as i64 - c.we_served as i64
    }

    /// Pick which peers to unchoke: the `slots` candidates with the highest
    /// reputation. Ties broken by key for determinism.
    pub fn rank_for_unchoke(&self, candidates: &[K], slots: usize) -> Vec<K> {
        let mut ranked = candidates.to_vec();
        ranked.sort_by(|a, b| {
            self.reputation(b)
                .cmp(&self.reputation(a))
                .then_with(|| a.cmp(b))
        });
        ranked.truncate(slots);
        ranked
    }
}

// Receipt absorption is only meaningful when the ledger is keyed by the Ed25519
// identity, since a receipt names Ed25519 peers.
impl Ledger<PeerId> {
    /// Verify and absorb a receipt, crediting its `server`. Ignores invalid ones.
    /// Returns whether the receipt was valid and applied.
    pub fn apply_receipt(&mut self, receipt: &Receipt) -> bool {
        if !receipt.verify() {
            return false;
        }
        self.peers.entry(receipt.server).or_default().credited_by_receipts += receipt.bytes;
        true
    }
}

// Persistence is available whenever the key type can be (de)serialized.
impl<K> Ledger<K>
where
    K: Eq + Hash + Clone + Ord + Serialize + DeserializeOwned,
{
    /// Open a ledger persisted at `path`, or start a fresh one bound to it.
    pub fn open(path: impl AsRef<Path>) -> Result<Ledger<K>, LedgerError> {
        let path = path.as_ref().to_path_buf();
        let peers = match fs::read(&path) {
            Ok(bytes) => bincode::deserialize(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Ledger { peers, path: Some(path) })
    }

    /// Persist to the bound path (no-op if created without one).
    pub fn save(&self) -> Result<(), LedgerError> {
        if let Some(path) = &self.path {
            let tmp = path.with_extension("tmp");
            fs::write(&tmp, bincode::serialize(&self.peers)?)?;
            fs::rename(&tmp, path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn pid(seed: u8) -> PeerId {
        Identity::from_seed([seed; 32]).peer_id()
    }

    fn tmp_path() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("np2ptp-ledger-{}-{}.bin", std::process::id(), n))
    }

    #[test]
    fn reputation_reflects_net_contribution() {
        let mut l = Ledger::new();
        let giver = pid(1);
        let leech = pid(2);
        l.record_received(giver, 1000); // gave us a lot
        l.record_served(giver, 200); // we gave a little back
        l.record_served(leech, 1000); // we fed the leech
        assert_eq!(l.reputation(&giver), 800);
        assert_eq!(l.reputation(&leech), -1000);
    }

    #[test]
    fn unchoke_favors_reciprocators_over_leeches() {
        let mut l = Ledger::new();
        let good = pid(1);
        let ok = pid(2);
        let leech = pid(3);
        l.record_received(good, 5000);
        l.record_received(ok, 1000);
        l.record_served(leech, 5000);
        let order = l.rank_for_unchoke(&[leech, ok, good], 2);
        assert_eq!(order, vec![good, ok]); // leech excluded from the 2 slots
    }

    #[test]
    fn totals_sums_across_all_peers() {
        let mut l = Ledger::new();
        let a = pid(1);
        let b = pid(2);
        l.record_received(a, 1000);
        l.record_served(a, 100);
        l.record_received(b, 500);
        l.record_served(b, 50);
        let t = l.totals();
        assert_eq!(t.served_to_us, 1500);
        assert_eq!(t.we_served, 150);
    }

    #[test]
    fn apply_receipt_credits_only_when_valid() {
        let mut l = Ledger::new();
        let client = Identity::from_seed([1u8; 32]);
        let server = pid(2);
        let good = Receipt::issue(&client, server, 4096, 1);
        assert!(l.apply_receipt(&good));
        assert_eq!(l.counters(&server).credited_by_receipts, 4096);

        let mut bad = Receipt::issue(&client, server, 4096, 2);
        bad.bytes = 9_999; // tamper
        assert!(!l.apply_receipt(&bad));
        assert_eq!(l.counters(&server).credited_by_receipts, 4096); // unchanged
    }

    #[test]
    fn save_and_reopen_round_trips() {
        let path = tmp_path();
        let peer = pid(5);
        {
            let mut l: Ledger<PeerId> = Ledger::open(&path).unwrap();
            l.record_received(peer, 1234);
            l.record_served(peer, 34);
            l.save().unwrap();
        }
        let reopened: Ledger<PeerId> = Ledger::open(&path).unwrap();
        assert_eq!(reopened.reputation(&peer), 1200);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn credited_receipts_add_to_reputation() {
        let mut l = Ledger::new();
        let client = Identity::from_seed([9u8; 32]);
        let peer = pid(6);
        let r = Receipt::issue(&client, peer, 5000, 1);
        assert!(l.apply_receipt(&r));
        assert_eq!(l.reputation(&peer), 5000);
    }

    #[test]
    fn credit_receipt_adds_without_needing_a_full_receipt_object() {
        let mut l: Ledger<PeerId> = Ledger::new();
        let peer = pid(7);
        l.credit_receipt(peer, 2500);
        assert_eq!(l.counters(&peer).credited_by_receipts, 2500);
        assert_eq!(l.reputation(&peer), 2500);
    }
}
