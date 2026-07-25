//! `download_multi`: fetch from several providers, falling back to whichever
//! is still around when one drops mid-session.

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
        let p = std::env::temp_dir().join(format!("np2ptp-multi-source-{}-{}", std::process::id(), n));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_multi_completes_when_one_provider_disappears() {
    let data = sample(2_000_000, 40);

    // Two independent seeders holding the exact same content (same bytes ->
    // same deterministic manifest/root).
    let a_dir = TmpDir::new();
    let a_store = Store::open(a_dir.path()).unwrap();
    let manifest = a_store.ingest(&data, None).unwrap();
    let root = manifest.root;
    assert!(manifest.chunks.len() > 4, "want enough chunks to spread across providers");

    let b_dir = TmpDir::new();
    let b_store = Store::open(b_dir.path()).unwrap();
    let b_manifest = b_store.ingest(&data, None).unwrap();
    assert_eq!(b_manifest.root, root, "both seeders must agree on the content id");

    let a = Network::spawn(a_store, Some([100u8; 32])).unwrap();
    a.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap()).await.unwrap();
    let a_addr = first_listen_addr(&a).await;
    let a_peer = a.local_peer_id();
    a.provide(&manifest).await.unwrap();

    let b = Network::spawn(b_store, Some([101u8; 32])).unwrap();
    b.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap()).await.unwrap();
    let b_addr = first_listen_addr(&b).await;
    let b_peer = b.local_peer_id();
    b.provide(&manifest).await.unwrap();

    let d_dir = TmpDir::new();
    let d = Network::spawn(Store::open(d_dir.path()).unwrap(), Some([102u8; 32])).unwrap();
    let d_store = Store::open(d_dir.path()).unwrap();
    d.dial(a_addr).await.unwrap();
    d.dial(b_addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await; // let both connections settle

    // A disappears mid-session — d still has a and b in its provider list,
    // but a is no longer reachable.
    drop(a);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let providers = [a_peer, b_peer];
    let mut ok = false;
    for _ in 0..100 {
        if d.download_multi(root, &providers, &d_store).await.is_ok() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ok, "download_multi should complete via b once a is gone");
    assert_eq!(d_store.export(&manifest).unwrap(), data, "reconstructed content must match exactly");

    // b must have been credited for serving (a never could have).
    let mut credited = false;
    for _ in 0..40 {
        if d.reputation(b_peer).await.unwrap() > 0 {
            credited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(credited, "b should be credited a receipt for the bytes it actually served");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn download_multi_rejects_empty_provider_list() {
    let dir = TmpDir::new();
    let store = Store::open(dir.path()).unwrap();
    let d = Network::spawn(Store::open(dir.path()).unwrap(), Some([103u8; 32])).unwrap();
    let root = np2ptp_core::Hash::of(b"whatever");
    assert!(d.download_multi(root, &[], &store).await.is_err());
}
