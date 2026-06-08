//! End-to-end bridge: node A converts a torrent and bridges it; node B resolves
//! the same torrent straight from the NP2PTP network, without converting.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use np2ptp_bridge::{
    convert, publish, resolve_or_convert, BridgeError, TorrentDownload, TorrentFile, TorrentMeta,
    TorrentSource,
};
use np2ptp_net::{Multiaddr, Network};
use np2ptp_store::Store;
use sha1::{Digest, Sha1};

struct TmpDir(std::path::PathBuf);
impl TmpDir {
    fn new() -> TmpDir {
        static C: AtomicU64 = AtomicU64::new(0);
        let n = C.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("np2ptp-brnet-{}-{}", std::process::id(), n));
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
    (0..n).map(|_| { x ^= x >> 12; x ^= x << 25; x ^= x >> 27; (x.wrapping_mul(0x2545F4914F6CDD1D) >> 33) as u8 }).collect()
}

fn fake_meta(files: &[(String, Vec<u8>)], piece_length: usize, infohash: Vec<u8>) -> TorrentMeta {
    let mut data = Vec::new();
    for (_, b) in files {
        data.extend_from_slice(b);
    }
    let piece_hashes = data.chunks(piece_length).map(|c| Sha1::digest(c).into()).collect();
    TorrentMeta {
        infohash,
        name: "linux-iso".to_string(),
        files: files.iter().map(|(p, b)| TorrentFile { path: p.clone(), length: b.len() as u64 }).collect(),
        piece_length: piece_length as u32,
        piece_hashes,
    }
}

struct FakeSource {
    meta: TorrentMeta,
    files: Option<Vec<(String, Vec<u8>)>>,
}
impl TorrentSource for FakeSource {
    async fn infohash(&self, _: &str) -> Result<Vec<u8>, BridgeError> {
        Ok(self.meta.infohash.clone())
    }
    async fn metadata(&self, _: &str) -> Result<Option<TorrentMeta>, BridgeError> {
        Ok(Some(self.meta.clone()))
    }
    async fn fetch(&self, _: &str) -> Result<TorrentDownload, BridgeError> {
        match &self.files {
            Some(files) => Ok(TorrentDownload { meta: self.meta.clone(), files: files.clone() }),
            None => Err(BridgeError::Source("this node refuses to hit BitTorrent".into())),
        }
    }
}

async fn first_listen_addr(net: &Network) -> Multiaddr {
    for _ in 0..100 {
        if let Some(addr) = net.listeners().await.unwrap().into_iter().next() {
            return addr;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no listen address");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_node_resolves_torrent_from_np2ptp_without_converting() {
    let files = vec![
        ("disc/part1.bin".to_string(), sample(250_000, 1)),
        ("disc/part2.bin".to_string(), sample(180_000, 2)),
    ];
    let infohash = vec![0x11u8; 20];
    let meta = fake_meta(&files, 32_768, infohash.clone());

    // --- Node A: convert from "BitTorrent" and bridge onto NP2PTP ---
    let a_dir = TmpDir::new();
    let net_a = Network::spawn(Store::open(a_dir.path()).unwrap(), Some([90u8; 32])).unwrap();
    let store_a = Store::open(a_dir.path()).unwrap();
    net_a.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap()).await.unwrap();
    let a_addr = first_listen_addr(&net_a).await;
    let a_peer = net_a.local_peer_id();

    let src_a = FakeSource { meta: meta.clone(), files: Some(files.clone()) };
    let (m_a, _) = convert(&store_a, &src_a, "linux.torrent").await.unwrap();

    // --- Node B joins the DHT, both listening so the mapping can replicate ---
    let b_dir = TmpDir::new();
    let net_b = Network::spawn(Store::open(b_dir.path()).unwrap(), Some([91u8; 32])).unwrap();
    let store_b = Store::open(b_dir.path()).unwrap();
    net_b.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap()).await.unwrap();
    let b_addr = first_listen_addr(&net_b).await;
    let b_peer = net_b.local_peer_id();

    net_a.add_peer(b_peer, b_addr.clone()).await.unwrap();
    net_b.add_peer(a_peer, a_addr.clone()).await.unwrap();
    net_a.dial(b_addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // A publishes the bridge mapping + provides the content.
    publish(&net_a, &m_a, &infohash).await.unwrap();

    // --- Node B resolves the SAME torrent. Its source refuses BitTorrent, so a
    // success can only mean it came from the NP2PTP network. ---
    let src_b = FakeSource { meta: meta.clone(), files: None };
    let mut outcome = None;
    for _ in 0..100 {
        if let Ok(o) = resolve_or_convert(&net_b, &store_b, &src_b, "linux.torrent").await {
            outcome = Some(o);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let o = outcome.expect("B should resolve the torrent from NP2PTP");

    assert!(!o.converted, "B must serve from NP2PTP, not convert via BitTorrent");
    assert_eq!(o.manifest.root, m_a.root, "same torrent -> same nptp content id");

    // And the bytes B reconstructed match the originals.
    let rebuilt = store_b.export_tree(&o.manifest).unwrap();
    assert_eq!(rebuilt, files);
}
