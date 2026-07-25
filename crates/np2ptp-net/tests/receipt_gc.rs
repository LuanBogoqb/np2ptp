//! GC of the transient per-connection receipt-pull bookkeeping
//! (`rep_peers`/`receipts_pulled_from`) on disconnect: a peer that
//! disconnects and reconnects should get pulled from again, not skipped
//! forever because of stale state from the first connection.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use np2ptp_net::{Multiaddr, Network};
use np2ptp_store::Store;

struct TmpDir(std::path::PathBuf);
impl TmpDir {
    fn new() -> TmpDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("np2ptp-receipt-gc-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sample(n: usize, seed: u64) -> Vec<u8> {
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

async fn first_listen_addr(net: &Network) -> Multiaddr {
    for _ in 0..100 {
        if let Some(addr) = net.listeners().await.unwrap().into_iter().next() {
            return addr;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("node never reported a listen address");
}

/// `a` is the long-lived node whose GC we're testing (never respawned). `b`
/// disconnects and reconnects (same identity seed, so same peer id — a real
/// restart, not a stranger) with a *new* receipt in its bag that only
/// arrived while it was gone. If `a`'s `receipts_pulled_from` entry for `b`
/// wasn't cleared on disconnect, `a` never asks again and never credits `b`
/// for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disconnect_then_reconnect_pulls_receipts_again() {
    const B_SEED: [u8; 32] = [50u8; 32];
    let b_store_dir = TmpDir::new();

    // --- Round 1: a connects to a fresh, receipt-less b. ---
    let b1 = Network::spawn(Store::open(b_store_dir.path()).unwrap(), Some(B_SEED)).unwrap();
    b1.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap()).await.unwrap();
    let b_addr_1 = first_listen_addr(&b1).await;
    let b_peer = b1.local_peer_id();
    let voucher_data = sample(200_000, 51);
    let voucher_manifest = Store::open(b_store_dir.path()).unwrap().ingest(&voucher_data, None).unwrap();
    let voucher_root = voucher_manifest.root;
    b1.provide(&voucher_manifest).await.unwrap();

    let a_dir = TmpDir::new();
    let a = Network::spawn(Store::open(a_dir.path()).unwrap(), Some([51u8; 32])).unwrap();
    a.dial(b_addr_1.clone()).await.unwrap();

    // Give identify + the (empty-handed) GetReceipts round-trip time to happen.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(a.reputation(b_peer).await.unwrap(), 0, "b has no receipt yet, nothing to credit");

    // --- b earns a receipt while a is still connected to it. ---
    let e_dir = TmpDir::new();
    let e = Network::spawn(Store::open(e_dir.path()).unwrap(), Some([52u8; 32])).unwrap();
    let e_store = Store::open(e_dir.path()).unwrap();
    e.dial(b_addr_1.clone()).await.unwrap();
    let mut earned = false;
    for _ in 0..100 {
        if e.download(voucher_root, b_peer, &e_store).await.is_ok() {
            earned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(earned, "setup: e must download from b so b earns a receipt");
    tokio::time::sleep(Duration::from_millis(300)).await; // let b receive+persist the receipt

    // Since a already pulled b once (round 1, empty-handed) and a's ledger
    // for b is untouched (no bytes ever changed hands between a and b), a
    // will never pull again on its own here without a fresh connection —
    // this is exactly the state the GC needs to unstick.
    assert_eq!(
        a.reputation(b_peer).await.unwrap(),
        0,
        "a must not credit b just because b earned a receipt elsewhere, without a fresh connection"
    );

    // --- b disconnects (drop its Network handle) and reconnects with the
    // same identity, now actually carrying a receipt. ---
    drop(b1);
    tokio::time::sleep(Duration::from_millis(500)).await; // let a's EventLoop observe ConnectionClosed

    let b2 = Network::spawn(Store::open(b_store_dir.path()).unwrap(), Some(B_SEED)).unwrap();
    b2.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap()).await.unwrap();
    let b_addr_2 = first_listen_addr(&b2).await;
    assert_eq!(b2.local_peer_id(), b_peer, "same seed must mean same peer id across the reconnect");

    a.dial(b_addr_2).await.unwrap();

    let mut credited = false;
    for _ in 0..100 {
        if a.reputation(b_peer).await.unwrap() > 0 {
            credited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        credited,
        "a should pull b's receipts again after b disconnects and reconnects, crediting the receipt earned in between"
    );
}
