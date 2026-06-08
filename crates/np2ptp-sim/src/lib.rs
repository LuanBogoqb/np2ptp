//! `np2ptp-sim` — the research harness.
//!
//! A protocol that claims to "improve torrent" has to *show* it. This crate spins
//! up real NP2PTP nodes (the same `np2ptp-net` stack used in production) and runs
//! A/B experiments that quantify the design's claims:
//!
//! * **Dedup** — how much storage/bandwidth content-defined chunking saves on
//!   re-shared, slightly-edited content.
//! * **Permanence** — does content survive the original seeder leaving, once a
//!   peer has re-shared it?
//! * **Incentives** — does the reputation choke actually stop a free-rider?
//! * **FEC cost** — what does erasure-coded download cost vs plain chunk download?
//!
//! Each scenario returns a metrics struct; `main.rs` prints a report and the unit
//! tests assert the headline claim of each.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use np2ptp_core::Hash;
use np2ptp_net::{Multiaddr, Network, PeerId};
use np2ptp_store::Store;

// ---- small helpers -------------------------------------------------------

/// Self-cleaning temp directory (no external crates).
pub struct TmpDir(PathBuf);

impl TmpDir {
    pub fn new() -> TmpDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("np2ptp-sim-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Deterministic high-entropy bytes (xorshift64*), so chunking finds real cut points.
pub fn sample(n: usize, seed: u64) -> Vec<u8> {
    let mut x = 0x9E3779B97F4A7C15u64 ^ seed.wrapping_mul(0xD1B54A32D192ED03);
    (0..n)
        .map(|_| {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            (x.wrapping_mul(0x2545F4914F6CDD1D) >> 33) as u8
        })
        .collect()
}

const QUIC_LISTEN: &str = "/ip4/127.0.0.1/udp/0/quic-v1";

async fn first_listen_addr(net: &Network) -> Multiaddr {
    for _ in 0..100 {
        if let Some(addr) = net.listeners().await.unwrap().into_iter().next() {
            return addr;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("node never reported a listen address");
}

/// Retry a chunk download until it completes or `tries` run out.
async fn download_until(net: &Network, root: Hash, peer: PeerId, into: &Store, tries: usize) -> bool {
    for _ in 0..tries {
        if net.download(root, peer, into).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

// ---- scenario 1: dedup ---------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct DedupResult {
    pub total_chunks: usize,
    pub unique_chunks_stored: usize,
    pub dedup_pct: f64,
}

/// Store a file, then a lightly-edited "v2", and measure how many chunks the
/// second version adds vs storing it from scratch.
pub fn dedup() -> DedupResult {
    let dir = TmpDir::new();
    let store = Store::open(dir.path()).unwrap();

    let base = sample(4_000_000, 1);
    let mut v2 = base.clone();
    v2.splice(2_000..2_000, sample(20_000, 99)); // insert near the front

    let m1 = store.ingest(&base, None).unwrap();
    let m2 = store.ingest(&v2, None).unwrap();

    let total_chunks = m1.chunks.len() + m2.chunks.len();
    let unique = store.object_count().unwrap();
    DedupResult {
        total_chunks,
        unique_chunks_stored: unique,
        dedup_pct: 100.0 * (1.0 - unique as f64 / total_chunks as f64),
    }
}

// ---- scenario 2: permanence ---------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct PermanenceResult {
    pub reshared: bool,
    pub completed_after_seed_left: bool,
}

/// A seeder shares content with one peer, which optionally re-shares it. Then the
/// seeder leaves and a fresh peer tries to download. With re-sharing the content
/// survives; without it, it dies with the seeder.
pub async fn permanence(reshare: bool) -> PermanenceResult {
    let data = sample(300_000, 2);

    // Seeder.
    let seed_dir = TmpDir::new();
    let seed_store = Store::open(seed_dir.path()).unwrap();
    let manifest = seed_store.ingest(&data, None).unwrap();
    let root = manifest.root;
    let seed = Network::spawn(seed_store, None).unwrap();
    seed.listen(QUIC_LISTEN.parse().unwrap()).await.unwrap();
    let seed_addr = first_listen_addr(&seed).await;
    let seed_peer = seed.local_peer_id();
    seed.provide(&manifest).await.unwrap();

    // First-generation peer downloads the whole thing.
    let p1_dir = TmpDir::new();
    let p1 = Network::spawn(Store::open(p1_dir.path()).unwrap(), None).unwrap();
    let p1_store = Store::open(p1_dir.path()).unwrap();
    p1.dial(seed_addr.clone()).await.unwrap();
    if !download_until(&p1, root, seed_peer, &p1_store, 200).await {
        return PermanenceResult { reshared: reshare, completed_after_seed_left: false };
    }

    // Optionally re-share it.
    let mut p1_addr = None;
    if reshare {
        p1.listen(QUIC_LISTEN.parse().unwrap()).await.unwrap();
        p1_addr = Some(first_listen_addr(&p1).await);
        p1.provide(&manifest).await.unwrap();
    }
    let p1_peer = p1.local_peer_id();

    // Seeder leaves the swarm.
    drop(seed);
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Fresh peer tries to get the content now that the seeder is gone.
    let p2_dir = TmpDir::new();
    let p2 = Network::spawn(Store::open(p2_dir.path()).unwrap(), None).unwrap();
    let p2_store = Store::open(p2_dir.path()).unwrap();

    let (provider_addr, provider_peer) = match (reshare, p1_addr) {
        (true, Some(addr)) => (addr, p1_peer),
        // No re-share: the only address we know is the dead seeder's.
        _ => (seed_addr, seed_peer),
    };
    p2.dial(provider_addr).await.unwrap();
    let completed = download_until(&p2, root, provider_peer, &p2_store, 60).await;

    PermanenceResult { reshared: reshare, completed_after_seed_left: completed }
}

// ---- scenario 3: free-riding --------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct FreerideResult {
    pub choke_enabled: bool,
    pub leech_completed: bool,
}

/// A leech (downloads, never reciprocates) tries to grab content. With the choke
/// enabled it should be cut off after its first freebie chunk; without it, it
/// finishes freely.
pub async fn freeride(choke: bool) -> FreerideResult {
    let data = sample(300_000, 3); // multi-chunk

    let seed_dir = TmpDir::new();
    let seed_store = Store::open(seed_dir.path()).unwrap();
    let manifest = seed_store.ingest(&data, None).unwrap();
    let root = manifest.root;
    let seed = Network::spawn(seed_store, None).unwrap();
    seed.listen(QUIC_LISTEN.parse().unwrap()).await.unwrap();
    let seed_addr = first_listen_addr(&seed).await;
    let seed_peer = seed.local_peer_id();
    seed.provide(&manifest).await.unwrap();
    if choke {
        seed.set_choke_threshold(0).await.unwrap();
    }

    let leech_dir = TmpDir::new();
    let leech = Network::spawn(Store::open(leech_dir.path()).unwrap(), None).unwrap();
    let leech_store = Store::open(leech_dir.path()).unwrap();
    leech.dial(seed_addr).await.unwrap();
    let completed = download_until(&leech, root, seed_peer, &leech_store, 60).await;

    FreerideResult { choke_enabled: choke, leech_completed: completed }
}

// ---- scenario 4: FEC cost -----------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct FecCostResult {
    pub size: usize,
    pub chunk_ms: u128,
    pub fec_ms: u128,
}

/// Download the same content two ways from the same seeder and time each: plain
/// chunk download vs erasure-coded (RaptorQ symbol) download.
pub async fn fec_cost() -> FecCostResult {
    let data = sample(1_000_000, 4);

    let seed_dir = TmpDir::new();
    let seed_store = Store::open(seed_dir.path()).unwrap();
    let manifest = seed_store.ingest(&data, None).unwrap();
    let root = manifest.root;
    let seed = Network::spawn(seed_store, None).unwrap();
    seed.listen(QUIC_LISTEN.parse().unwrap()).await.unwrap();
    let seed_addr = first_listen_addr(&seed).await;
    let seed_peer = seed.local_peer_id();
    seed.provide(&manifest).await.unwrap();

    // Plain chunk download.
    let a_dir = TmpDir::new();
    let a = Network::spawn(Store::open(a_dir.path()).unwrap(), None).unwrap();
    let a_store = Store::open(a_dir.path()).unwrap();
    a.dial(seed_addr.clone()).await.unwrap();
    let t = Instant::now();
    let _ = download_until(&a, root, seed_peer, &a_store, 600).await;
    let chunk_ms = t.elapsed().as_millis();

    // Erasure-coded download.
    let b_dir = TmpDir::new();
    let b = Network::spawn(Store::open(b_dir.path()).unwrap(), None).unwrap();
    let b_store = Store::open(b_dir.path()).unwrap();
    b.dial(seed_addr).await.unwrap();
    let t = Instant::now();
    for _ in 0..600 {
        if b.download_fec(root, seed_peer, &b_store).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let fec_ms = t.elapsed().as_millis();

    FecCostResult { size: data.len(), chunk_ms, fec_ms }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_saves_most_of_a_reshared_edit() {
        let r = dedup();
        // Re-sharing a lightly-edited copy should dedup the vast majority of it.
        assert!(r.dedup_pct > 40.0, "weak dedup: {r:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn content_survives_seeder_departure_only_with_resharing() {
        assert!(permanence(true).await.completed_after_seed_left, "reshared content should survive");
        assert!(!permanence(false).await.completed_after_seed_left, "non-reshared content should die with the seed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn choke_stops_the_free_rider() {
        assert!(freeride(false).await.leech_completed, "without choke the leech finishes");
        assert!(!freeride(true).await.leech_completed, "with choke the leech is cut off");
    }
}
