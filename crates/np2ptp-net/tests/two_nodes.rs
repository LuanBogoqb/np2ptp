//! Two real libp2p nodes over QUIC: end-to-end content download + DHT discovery.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use np2ptp_core::Hash;
use np2ptp_net::{Multiaddr, Network};
use np2ptp_store::Store;

struct TmpDir(std::path::PathBuf);

impl TmpDir {
    fn new() -> TmpDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("np2ptp-net-{}-{}", std::process::id(), n));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_to_end_download_over_quic() {
    // --- Seed: pack a multi-chunk file into its store and start serving it ---
    let seed_dir = TmpDir::new();
    let seed_store = Store::open(seed_dir.path()).unwrap();
    let data = sample(300_000, 1); // several content-defined chunks
    let manifest = seed_store.ingest(&data, Some("movie.bin".into())).unwrap();
    let root = manifest.root;
    assert!(manifest.chunks.len() > 1, "want a multi-chunk transfer");

    let seed = Network::spawn(seed_store, Some([1u8; 32])).unwrap();
    seed.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let seed_addr = first_listen_addr(&seed).await;
    let seed_peer = seed.local_peer_id();
    seed.provide(&manifest).await.unwrap();

    // --- Client: connect and download the whole thing by content id ---
    let client_dir = TmpDir::new();
    let client = Network::spawn(Store::open(client_dir.path()).unwrap(), Some([2u8; 32])).unwrap();
    let client_store = Store::open(client_dir.path()).unwrap();
    client.add_peer(seed_peer, seed_addr.clone()).await.unwrap();
    client.dial(seed_addr).await.unwrap();

    // Retry while the QUIC connection establishes.
    let mut downloaded = None;
    for _ in 0..100 {
        if let Ok(m) = client.download(root, seed_peer, &client_store).await {
            downloaded = Some(m);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let got = downloaded.expect("download should complete");
    assert_eq!(got.root, root);

    // Reconstruct from the client's own store and confirm byte-for-byte identity.
    let rebuilt = client_store.export(&got).unwrap();
    assert_eq!(rebuilt, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reputation_recorded_after_transfer() {
    let seed_dir = TmpDir::new();
    let seed_store = Store::open(seed_dir.path()).unwrap();
    let data = sample(300_000, 4);
    let manifest = seed_store.ingest(&data, None).unwrap();
    let root = manifest.root;

    let seed = Network::spawn(seed_store, Some([30u8; 32])).unwrap();
    seed.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let seed_addr = first_listen_addr(&seed).await;
    let seed_peer = seed.local_peer_id();
    seed.provide(&manifest).await.unwrap();

    let client_dir = TmpDir::new();
    let client = Network::spawn(Store::open(client_dir.path()).unwrap(), Some([31u8; 32])).unwrap();
    let client_store = Store::open(client_dir.path()).unwrap();
    let client_peer = client.local_peer_id();
    client.dial(seed_addr).await.unwrap();

    for _ in 0..100 {
        if client.download(root, seed_peer, &client_store).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The client received bytes from the seed and gave nothing back.
    assert!(client.reputation(seed_peer).await.unwrap() > 0, "client should credit the seed");
    assert!(seed.reputation(client_peer).await.unwrap() < 0, "seed should see the client as a taker");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn choke_blocks_a_non_reciprocating_peer() {
    let seed_dir = TmpDir::new();
    let seed_store = Store::open(seed_dir.path()).unwrap();
    let data = sample(300_000, 5); // multi-chunk
    let manifest = seed_store.ingest(&data, None).unwrap();
    let root = manifest.root;

    let seed = Network::spawn(seed_store, Some([40u8; 32])).unwrap();
    seed.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let seed_addr = first_listen_addr(&seed).await;
    let seed_peer = seed.local_peer_id();
    seed.provide(&manifest).await.unwrap();
    // Choke anyone the moment they go net-negative: a pure leech gets one freebie
    // chunk, then is refused the rest.
    seed.set_choke_threshold(0).await.unwrap();

    let client_dir = TmpDir::new();
    let client = Network::spawn(Store::open(client_dir.path()).unwrap(), Some([41u8; 32])).unwrap();
    let client_store = Store::open(client_dir.path()).unwrap();
    client.dial(seed_addr).await.unwrap();

    // The download must never complete: after the first served chunk the leech's
    // reputation is negative and further chunks are choked.
    let mut completed = false;
    for _ in 0..40 {
        if client.download(root, seed_peer, &client_store).await.is_ok() {
            completed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!completed, "a choked leech should not be able to finish the download");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fec_download_reconstructs_over_quic() {
    // Seed a multi-chunk file and serve RaptorQ symbols for it.
    let seed_dir = TmpDir::new();
    let seed_store = Store::open(seed_dir.path()).unwrap();
    let data = sample(250_000, 8);
    let manifest = seed_store.ingest(&data, Some("vid.bin".into())).unwrap();
    let root = manifest.root;

    let seed = Network::spawn(seed_store, Some([50u8; 32])).unwrap();
    seed.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let seed_addr = first_listen_addr(&seed).await;
    let seed_peer = seed.local_peer_id();
    seed.provide(&manifest).await.unwrap();

    // Client reconstructs purely from fountain-coded symbols.
    let client_dir = TmpDir::new();
    let client = Network::spawn(Store::open(client_dir.path()).unwrap(), Some([51u8; 32])).unwrap();
    let client_store = Store::open(client_dir.path()).unwrap();
    client.dial(seed_addr).await.unwrap();

    let mut downloaded = None;
    for _ in 0..100 {
        if let Ok(m) = client.download_fec(root, seed_peer, &client_store).await {
            downloaded = Some(m);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let got = downloaded.expect("FEC download should complete");
    assert_eq!(client_store.export(&got).unwrap(), data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dht_mapping_infohash_to_root_round_trips() {
    // Node A publishes a torrent-infohash -> nptp-root mapping.
    let a_dir = TmpDir::new();
    let a = Network::spawn(Store::open(a_dir.path()).unwrap(), Some([80u8; 32])).unwrap();
    a.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let a_addr = first_listen_addr(&a).await;
    let a_peer = a.local_peer_id();

    let infohash = [0xABu8; 20]; // stand-in for a BitTorrent v1 infohash
    let root = Hash::of(b"nptp content bridged from that torrent");

    // Node B also listens, so the record can be replicated to it (put_record with
    // Quorum::One needs at least one reachable remote peer).
    let b_dir = TmpDir::new();
    let b = Network::spawn(Store::open(b_dir.path()).unwrap(), Some([81u8; 32])).unwrap();
    b.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let b_addr = first_listen_addr(&b).await;
    let b_peer = b.local_peer_id();

    a.add_peer(b_peer, b_addr.clone()).await.unwrap();
    b.add_peer(a_peer, a_addr.clone()).await.unwrap();
    a.dial(b_addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(a.put_mapping(&infohash, root).await.unwrap(), "put_mapping should succeed");

    // B resolves the infohash to the nptp root via the DHT.
    let mut got = None;
    for _ in 0..100 {
        if let Some(r) = b.get_mapping(&infohash).await.unwrap() {
            got = Some(r);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(got, Some(root), "B should resolve the bridged mapping");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovers_provider_via_dht() {
    let seed_dir = TmpDir::new();
    let seed_store = Store::open(seed_dir.path()).unwrap();
    let manifest = seed_store.ingest(&sample(80_000, 3), None).unwrap();
    let root = manifest.root;

    let seed = Network::spawn(seed_store, Some([10u8; 32])).unwrap();
    seed.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let seed_addr = first_listen_addr(&seed).await;
    let seed_peer = seed.local_peer_id();
    seed.provide(&manifest).await.unwrap();

    let client_dir = TmpDir::new();
    let client = Network::spawn(Store::open(client_dir.path()).unwrap(), Some([11u8; 32])).unwrap();
    client.add_peer(seed_peer, seed_addr.clone()).await.unwrap();
    client.dial(seed_addr).await.unwrap();

    // The client should discover the seed as a provider of `root` via the DHT.
    let mut found = Vec::new();
    for _ in 0..100 {
        found = client.find_providers(root).await.unwrap();
        if found.contains(&seed_peer) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(found.contains(&seed_peer), "DHT should reveal the seed as a provider");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connected_peers_and_ledger_totals_reflect_a_transfer() {
    let seed_dir = TmpDir::new();
    let seed_store = Store::open(seed_dir.path()).unwrap();
    let data = sample(300_000, 6);
    let manifest = seed_store.ingest(&data, None).unwrap();
    let root = manifest.root;

    let seed = Network::spawn(seed_store, Some([90u8; 32])).unwrap();
    seed.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let seed_addr = first_listen_addr(&seed).await;
    let seed_peer = seed.local_peer_id();
    seed.provide(&manifest).await.unwrap();

    let client_dir = TmpDir::new();
    let client = Network::spawn(Store::open(client_dir.path()).unwrap(), Some([91u8; 32])).unwrap();
    let client_store = Store::open(client_dir.path()).unwrap();
    let client_peer = client.local_peer_id();
    client.dial(seed_addr).await.unwrap();

    for _ in 0..100 {
        if client.download(root, seed_peer, &client_store).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Both sides should see the other in their connected-peer list.
    let mut seed_sees_client = false;
    for _ in 0..50 {
        if seed.connected_peers().await.unwrap().contains(&client_peer) {
            seed_sees_client = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(seed_sees_client, "seed should list the client as connected");

    // The seed served the whole file to someone; ledger totals must reflect it.
    let totals = seed.ledger_totals().await.unwrap();
    assert_eq!(totals.we_served, data.len() as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn download_with_progress_reaches_total() {
    let seed_dir = TmpDir::new();
    let seed_store = Store::open(seed_dir.path()).unwrap();
    let data = sample(300_000, 9);
    let manifest = seed_store.ingest(&data, None).unwrap();
    let root = manifest.root;
    let total_chunks = manifest.chunks.len();
    assert!(total_chunks > 1, "want a multi-chunk transfer");

    let seed = Network::spawn(seed_store, Some([95u8; 32])).unwrap();
    seed.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let seed_addr = first_listen_addr(&seed).await;
    let seed_peer = seed.local_peer_id();
    seed.provide(&manifest).await.unwrap();

    let client_dir = TmpDir::new();
    let client = Network::spawn(Store::open(client_dir.path()).unwrap(), Some([96u8; 32])).unwrap();
    let client_store = Store::open(client_dir.path()).unwrap();
    client.dial(seed_addr).await.unwrap();

    let last_done = std::sync::Arc::new(std::sync::Mutex::new(0usize));
    let mut ok = false;
    for _ in 0..100 {
        let done_cell = last_done.clone();
        let result = client
            .download_with_progress(root, seed_peer, &client_store, move |done, _total| {
                *done_cell.lock().unwrap() = done;
            })
            .await;
        if result.is_ok() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ok, "download should complete");
    assert_eq!(*last_done.lock().unwrap(), total_chunks);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fec_download_with_progress_reaches_need() {
    let seed_dir = TmpDir::new();
    let seed_store = Store::open(seed_dir.path()).unwrap();
    let data = sample(250_000, 11);
    let manifest = seed_store.ingest(&data, Some("vid.bin".into())).unwrap();
    let root = manifest.root;

    let seed = Network::spawn(seed_store, Some([97u8; 32])).unwrap();
    seed.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let seed_addr = first_listen_addr(&seed).await;
    let seed_peer = seed.local_peer_id();
    seed.provide(&manifest).await.unwrap();

    let client_dir = TmpDir::new();
    let client = Network::spawn(Store::open(client_dir.path()).unwrap(), Some([98u8; 32])).unwrap();
    let client_store = Store::open(client_dir.path()).unwrap();
    client.dial(seed_addr).await.unwrap();

    let last_call = std::sync::Arc::new(std::sync::Mutex::new((0usize, 0usize)));
    let mut downloaded = None;
    for _ in 0..100 {
        let cell = last_call.clone();
        let result = client
            .download_fec_with_progress(root, seed_peer, &client_store, move |done, need| {
                *cell.lock().unwrap() = (done, need);
            })
            .await;
        if let Ok(m) = result {
            downloaded = Some(m);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let got = downloaded.expect("FEC download should complete");
    assert_eq!(client_store.export(&got).unwrap(), data);
    let (done, need) = *last_call.lock().unwrap();
    assert!(done >= need && need > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_content_is_reported_not_hung() {
    // A seed that serves nothing: manifest request returns NoManifest.
    let seed_dir = TmpDir::new();
    let seed = Network::spawn(Store::open(seed_dir.path()).unwrap(), Some([20u8; 32])).unwrap();
    seed.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let seed_addr = first_listen_addr(&seed).await;
    let seed_peer = seed.local_peer_id();

    let client_dir = TmpDir::new();
    let client = Network::spawn(Store::open(client_dir.path()).unwrap(), Some([21u8; 32])).unwrap();
    client.dial(seed_addr).await.unwrap();

    let phantom = Hash::of(b"content nobody seeds");
    // Eventually (once connected) we get a definite "no manifest", never a hang.
    let mut saw_answer = false;
    for _ in 0..100 {
        match client.get_manifest(seed_peer, phantom).await {
            Err(np2ptp_net::NetError::NoManifest) => {
                saw_answer = true;
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    assert!(saw_answer);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receipt_from_a_download_lets_a_third_party_credit_the_server() {
    // A serves some content; B downloads it, which should automatically send
    // A a signed receipt crediting A for the bytes served.
    let a_dir = TmpDir::new();
    let a_store = Store::open(a_dir.path()).unwrap();
    let data = sample(300_000, 70);
    let manifest = a_store.ingest(&data, None).unwrap();
    let root = manifest.root;

    let a = Network::spawn(a_store, Some([70u8; 32])).unwrap();
    a.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let a_addr = first_listen_addr(&a).await;
    let a_peer = a.local_peer_id();
    a.provide(&manifest).await.unwrap();

    let b_dir = TmpDir::new();
    let b = Network::spawn(Store::open(b_dir.path()).unwrap(), Some([71u8; 32])).unwrap();
    let b_store = Store::open(b_dir.path()).unwrap();
    b.dial(a_addr.clone()).await.unwrap();

    for _ in 0..100 {
        if b.download(root, a_peer, &b_store).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // C has no prior history with A. On connecting, C should pull A's receipt
    // bag (which now holds B's receipt crediting A), verify it, and credit A
    // — even though C never transacted with A directly.
    let c_dir = TmpDir::new();
    let c = Network::spawn(Store::open(c_dir.path()).unwrap(), Some([72u8; 32])).unwrap();
    c.dial(a_addr).await.unwrap();

    let mut credited = false;
    for _ in 0..100 {
        if c.reputation(a_peer).await.unwrap() > 0 {
            credited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(credited, "C should credit A via A's receipt from B, despite no direct history");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_peer_with_a_receipt_is_not_choked() {
    // B earns a receipt by serving a file to an earlier downloader. The choke
    // threshold below is 0 (zero tolerance for negative reputation, see
    // `choke_blocks_a_non_reciprocating_peer`), so the credited receipt has to
    // cover the *entire* subsequent download for B to never dip negative —
    // hence the voucher is sized comfortably larger than the real content.
    let b_dir = TmpDir::new();
    let b_store = Store::open(b_dir.path()).unwrap();
    let voucher_data = sample(400_000, 60);
    let voucher_manifest = b_store.ingest(&voucher_data, None).unwrap();
    let voucher_root = voucher_manifest.root;

    let b = Network::spawn(b_store, Some([60u8; 32])).unwrap();
    b.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let b_addr = first_listen_addr(&b).await;
    let b_peer = b.local_peer_id();
    b.provide(&voucher_manifest).await.unwrap();

    let earlier_dir = TmpDir::new();
    let earlier = Network::spawn(Store::open(earlier_dir.path()).unwrap(), Some([61u8; 32])).unwrap();
    let earlier_store = Store::open(earlier_dir.path()).unwrap();
    earlier.dial(b_addr.clone()).await.unwrap();
    let mut earned = false;
    for _ in 0..100 {
        if earlier.download(voucher_root, b_peer, &earlier_store).await.is_ok() {
            earned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(earned, "setup: B must earn a receipt from the earlier download");
    // Give B's EventLoop a moment to receive and store the auto-submitted receipt.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // A strict seed serving the real content: any net-negative peer is
    // choked after its first freebie chunk.
    let data = sample(300_000, 61);
    let seed_dir = TmpDir::new();
    let seed_store = Store::open(seed_dir.path()).unwrap();
    let manifest = seed_store.ingest(&data, None).unwrap();
    let root = manifest.root;
    let seed = Network::spawn(seed_store, Some([62u8; 32])).unwrap();
    seed.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let seed_addr = first_listen_addr(&seed).await;
    let seed_peer = seed.local_peer_id();
    seed.provide(&manifest).await.unwrap();
    seed.set_choke_threshold(0).await.unwrap();

    // B, cold to the seed but carrying a receipt, should not be choked.
    let b_download_dir = TmpDir::new();
    let b_download_store = Store::open(b_download_dir.path()).unwrap();
    b.dial(seed_addr.clone()).await.unwrap();
    let mut b_completed = false;
    for _ in 0..60 {
        if b.download(root, seed_peer, &b_download_store).await.is_ok() {
            b_completed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(b_completed, "a peer vouched for by a receipt should not be choked");

    // A, equally cold but with no receipt, should be choked.
    let a_dir = TmpDir::new();
    let a = Network::spawn(Store::open(a_dir.path()).unwrap(), Some([63u8; 32])).unwrap();
    let a_store = Store::open(a_dir.path()).unwrap();
    a.dial(seed_addr).await.unwrap();
    let mut a_completed = false;
    for _ in 0..60 {
        if a.download(root, seed_peer, &a_store).await.is_ok() {
            a_completed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!a_completed, "a cold peer with no receipt should still be choked");
}
