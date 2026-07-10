//! NAT traversal via circuit relay.
//!
//! Status on a single dev machine:
//! * ✅ A "behind-NAT" listener obtains a **relay reservation** and gets a usable
//!   `/…/p2p-circuit/p2p/<peer>` address (`listener_gets_relay_reservation`).
//! * ✅ A full **data transfer over the relayed connection** works on loopback
//!   (`download_through_a_relay`, `relay_moves_content_past_the_old_default_byte_cap`).
//!   This used to be `#[ignore]`d as "flaky, needs real NATs" — that diagnosis
//!   was wrong. The actual cause: `relay::Config::default()` caps a circuit at
//!   128 KiB, and `download_through_a_relay`'s 120,000-byte payload sat just
//!   under that cap, right at the edge of failing. Once `relay_config()`
//!   raised the cap to 512 MiB (see `lib.rs`), re-running the un-ignored test
//!   passed 5/5 — no real NAT involved, loopback the whole time.
//!
//! DCUtR hole-punching itself (as opposed to the relay circuit) still has
//! nothing to punch through on loopback and is only validated against a real
//! NAT by hand (see `docs/RELAY.md`).
//!
//! Unrelated background noise you may see in `--nocapture` output: a
//! "failed to persist receipts" I/O error and an occasional UPnP background-task
//! panic ("sender shouldn't have been dropped"), both from an *earlier* test's
//! `Network` being dropped (its temp dir removed) while its own background
//! tasks were still winding down. Neither affects this file's assertions —
//! all three tests pass either way — but it's real noise, not imagined.

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
        let p = std::env::temp_dir().join(format!("np2ptp-relay-{}-{}", std::process::id(), n));
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

async fn has_circuit_listener(net: &Network) -> bool {
    for _ in 0..100 {
        if net
            .listeners()
            .await
            .unwrap()
            .iter()
            .any(|a| a.to_string().contains("p2p-circuit"))
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// A node behind NAT can obtain a relay reservation and a dialable circuit address.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listener_gets_relay_reservation() {
    let relay_dir = TmpDir::new();
    let relay = Network::spawn(Store::open(relay_dir.path()).unwrap(), Some([60u8; 32])).unwrap();
    relay
        .listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let relay_addr = first_listen_addr(&relay).await;
    let relay_peer = relay.local_peer_id();
    // Without a known external address the relay grants address-less reservations
    // that clients reject; advertising its listen address fixes that.
    relay.add_external_address(relay_addr.clone()).await.unwrap();
    let relay_base: Multiaddr = format!("{relay_addr}/p2p/{relay_peer}").parse().unwrap();

    let l_dir = TmpDir::new();
    let listener = Network::spawn(Store::open(l_dir.path()).unwrap(), Some([61u8; 32])).unwrap();
    listener.dial(relay_base.clone()).await.unwrap();
    // The reservation needs an established connection to the relay first.
    tokio::time::sleep(Duration::from_millis(800)).await;
    listener
        .listen(format!("{relay_base}/p2p-circuit").parse().unwrap())
        .await
        .unwrap();

    assert!(
        has_circuit_listener(&listener).await,
        "listener should obtain a relay reservation (circuit address)"
    );
}

/// Full content download through a relay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_through_a_relay() {
    let relay_dir = TmpDir::new();
    let relay = Network::spawn(Store::open(relay_dir.path()).unwrap(), Some([70u8; 32])).unwrap();
    relay
        .listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let relay_addr = first_listen_addr(&relay).await;
    let relay_peer = relay.local_peer_id();
    relay.add_external_address(relay_addr.clone()).await.unwrap();
    let relay_base: Multiaddr = format!("{relay_addr}/p2p/{relay_peer}").parse().unwrap();

    let l_dir = TmpDir::new();
    let l_store = Store::open(l_dir.path()).unwrap();
    let data = sample(120_000, 6);
    let manifest = l_store.ingest(&data, None).unwrap();
    let root = manifest.root;
    let listener = Network::spawn(l_store, Some([71u8; 32])).unwrap();
    let listener_peer = listener.local_peer_id();
    listener.dial(relay_base.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    listener
        .listen(format!("{relay_base}/p2p-circuit").parse().unwrap())
        .await
        .unwrap();
    listener.provide(&manifest).await.unwrap();
    assert!(has_circuit_listener(&listener).await);

    let d_dir = TmpDir::new();
    let dialer = Network::spawn(Store::open(d_dir.path()).unwrap(), Some([72u8; 32])).unwrap();
    let d_store = Store::open(d_dir.path()).unwrap();
    dialer.dial(relay_base.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    dialer
        .dial(format!("{relay_base}/p2p-circuit/p2p/{listener_peer}").parse().unwrap())
        .await
        .unwrap();

    let mut ok = false;
    for _ in 0..200 {
        if dialer.download(root, listener_peer, &d_store).await.is_ok() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ok, "dialer should download from the listener via the relay");
    assert_eq!(d_store.export(&manifest).unwrap(), data);
}

/// Regression test for a real-world failure: a CGNAT seed serving a
/// multi-thousand-chunk `.nptp` file through the relay failed every time —
/// not flaky, every time — because `relay::Config::default()` caps a
/// circuit at `max_circuit_bytes` = 128 KiB (`1 << 17`), and the manifest
/// alone (a few thousand chunk hashes) was already ~12x over that. This was
/// confirmed to be why `download_through_a_relay` above used to be flaky too:
/// its 120_000-byte payload sat just under that same 128 KiB cap. The fix
/// (`relay_config()` in `lib.rs`) raises the cap to 512 MiB / 10 min, so
/// this payload — comfortably past the *old* cap — transfers cleanly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_moves_content_past_the_old_default_byte_cap() {
    let relay_dir = TmpDir::new();
    let relay = Network::spawn(Store::open(relay_dir.path()).unwrap(), Some([80u8; 32])).unwrap();
    relay
        .listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let relay_addr = first_listen_addr(&relay).await;
    let relay_peer = relay.local_peer_id();
    relay.add_external_address(relay_addr.clone()).await.unwrap();
    let relay_base: Multiaddr = format!("{relay_addr}/p2p/{relay_peer}").parse().unwrap();

    let l_dir = TmpDir::new();
    let l_store = Store::open(l_dir.path()).unwrap();
    // Comfortably past the *old* 131072-byte default cap (still far under
    // the new 512 MiB one).
    let data = sample(500_000, 8);
    let manifest = l_store.ingest(&data, None).unwrap();
    let root = manifest.root;
    let listener = Network::spawn(l_store, Some([81u8; 32])).unwrap();
    let listener_peer = listener.local_peer_id();
    listener.dial(relay_base.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    listener
        .listen(format!("{relay_base}/p2p-circuit").parse().unwrap())
        .await
        .unwrap();
    listener.provide(&manifest).await.unwrap();
    assert!(has_circuit_listener(&listener).await);

    let d_dir = TmpDir::new();
    let dialer = Network::spawn(Store::open(d_dir.path()).unwrap(), Some([82u8; 32])).unwrap();
    let d_store = Store::open(d_dir.path()).unwrap();
    dialer.dial(relay_base.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    dialer
        .dial(format!("{relay_base}/p2p-circuit/p2p/{listener_peer}").parse().unwrap())
        .await
        .unwrap();

    let mut ok = false;
    for _ in 0..200 {
        if dialer.download(root, listener_peer, &d_store).await.is_ok() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ok, "dialer should download 500 KB through the relay under the raised circuit cap");
    assert_eq!(d_store.export(&manifest).unwrap(), data);
}
