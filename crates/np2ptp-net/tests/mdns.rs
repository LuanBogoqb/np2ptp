//! mDNS: two nodes on the same "LAN" (loopback) find and connect to each
//! other with zero manual wiring — no `add_peer`/`dial`, no tracker, no DHT
//! bootstrap contact.
//!
//! `mdns_discovers_and_connects_without_dialing` is `#[ignore]`d: mDNS relies
//! on real UDP multicast, which this dev sandbox doesn't deliver between two
//! local processes (tried up to 65s, no discovery either direction — not a
//! timing issue, the initial mDNS probe fires at 500ms). Same category as
//! `np2ptp-net/tests/relay.rs`'s `download_through_a_relay`: needs a real
//! network (or at least an unsandboxed host) to validate by hand, not
//! something a loopback-only CI run can confirm.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use np2ptp_net::Network;
use np2ptp_store::Store;

struct TmpDir(std::path::PathBuf);
impl TmpDir {
    fn new() -> TmpDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("np2ptp-mdns-{}-{}", std::process::id(), n));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "mDNS multicast isn't delivered between two local processes in this sandbox; see module doc"]
async fn mdns_discovers_and_connects_without_dialing() {
    let a_dir = TmpDir::new();
    let a = Network::spawn(Store::open(a_dir.path()).unwrap(), Some([90u8; 32])).unwrap();
    a.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap()).await.unwrap();
    let a_peer = a.local_peer_id();

    let b_dir = TmpDir::new();
    let b = Network::spawn(Store::open(b_dir.path()).unwrap(), Some([91u8; 32])).unwrap();
    b.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap()).await.unwrap();
    let b_peer = b.local_peer_id();

    // No add_peer/dial on either side — mDNS alone should find and connect them.
    let mut a_sees_b = false;
    let mut b_sees_a = false;
    for _ in 0..600 {
        a_sees_b = a_sees_b || a.connected_peers().await.unwrap().contains(&b_peer);
        b_sees_a = b_sees_a || b.connected_peers().await.unwrap().contains(&a_peer);
        if a_sees_b && b_sees_a {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(a_sees_b && b_sees_a, "mDNS should discover and connect the two nodes without manual dialing");
}
