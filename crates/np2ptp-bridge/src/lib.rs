//! `np2ptp-bridge` — a BitTorrent <-> NP2PTP gateway.
//!
//! The idea: the first node to fetch a torrent re-publishes it on the NP2PTP
//! network, and everyone after pulls it from NP2PTP instead (faster, and it
//! survives the torrent's seeders disappearing). Concretely, given a `.torrent`
//! or magnet:
//!
//! 1. Look up its **infohash** in the NP2PTP DHT (`infohash -> nptp root`).
//! 2. **Hit** → download the content from the NP2PTP network (already-bridged).
//! 3. **Miss** → *convert*: download via BitTorrent, verify the bytes against the
//!    torrent's own piece hashes, `pack` them into NP2PTP, publish the mapping,
//!    and start providing. The next lookup hits the fast path.
//!
//! Two things make this correct (see module tests):
//!
//! * **Determinism** — content is chunked in the torrent's declared file order,
//!   so any two converters of the same torrent produce the *same* nptp root and
//!   therefore share/dedup on the network.
//! * **Verification** — converted content is checked against the torrent's piece
//!   hashes before publishing, so a converter can't bridge corrupt data. (For a
//!   `.torrent` resolved via the fast path the pieces are re-checked too;
//!   magnets resolved purely from the network rely on NP2PTP's own Merkle
//!   integrity, since their piece hashes aren't known without a BitTorrent fetch.)
//!
//! The actual BitTorrent download sits behind the [`TorrentSource`] trait, so the
//! whole gateway is testable without a BitTorrent client; a `librqbit`-backed
//! implementation plugs in as the real source.
#![allow(async_fn_in_trait)]

use np2ptp_core::Manifest;
use np2ptp_net::Network;
use np2ptp_store::Store;
use sha1::{Digest, Sha1};

mod bencode;
pub use bencode::parse_torrent_file;

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error(transparent)]
    Net(#[from] np2ptp_net::NetError),
    #[error(transparent)]
    Store(#[from] np2ptp_store::StoreError),
    #[error("downloaded content failed verification against the torrent piece hashes")]
    PieceVerificationFailed,
    #[error("torrent source error: {0}")]
    Source(String),
}

/// One file declared by a torrent (path is '/'-separated, in torrent order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentFile {
    pub path: String,
    pub length: u64,
}

/// Enough torrent metadata to identify, locate, and verify content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentMeta {
    /// BitTorrent infohash (20 bytes for v1) — the DHT lookup key.
    pub infohash: Vec<u8>,
    /// Top-level name (becomes the nptp manifest name).
    pub name: String,
    pub files: Vec<TorrentFile>,
    pub piece_length: u32,
    /// SHA-1 hash per piece (v1), over the concatenated file stream.
    pub piece_hashes: Vec<[u8; 20]>,
}

/// A fully-downloaded torrent: metadata plus each file's bytes in torrent order.
pub struct TorrentDownload {
    pub meta: TorrentMeta,
    pub files: Vec<(String, Vec<u8>)>,
}

/// Something that can identify and download a torrent (`.torrent` or magnet).
/// Implemented for real by a `librqbit`-backed source.
pub trait TorrentSource {
    /// Cheaply determine the infohash without downloading content.
    async fn infohash(&self, input: &str) -> Result<Vec<u8>, BridgeError>;
    /// Cheaply read full metadata if possible (`.torrent`), else `None` (magnet
    /// metadata needs a BitTorrent fetch).
    async fn metadata(&self, input: &str) -> Result<Option<TorrentMeta>, BridgeError>;
    /// Download the torrent's content (fetching metadata first for magnets).
    async fn fetch(&self, input: &str) -> Result<TorrentDownload, BridgeError>;
}

/// Result of [`resolve_or_convert`].
pub struct Outcome {
    pub manifest: Manifest,
    pub infohash: Vec<u8>,
    /// True if we had to convert from BitTorrent; false if served from NP2PTP.
    pub converted: bool,
}

/// Verify reconstructed files against a torrent's v1 piece hashes.
pub fn verify_pieces(files: &[(String, Vec<u8>)], piece_length: usize, piece_hashes: &[[u8; 20]]) -> bool {
    if piece_length == 0 {
        return false;
    }
    // Concatenate the files in order, then hash each fixed-size piece.
    let mut data = Vec::new();
    for (_, bytes) in files {
        data.extend_from_slice(bytes);
    }
    let expected_pieces = data.len().div_ceil(piece_length);
    if expected_pieces != piece_hashes.len() {
        return false;
    }
    for (chunk, expected) in data.chunks(piece_length).zip(piece_hashes) {
        let got: [u8; 20] = Sha1::digest(chunk).into();
        if &got != expected {
            return false;
        }
    }
    true
}

/// Convert a torrent into NP2PTP content: download via `source`, verify against
/// the torrent piece hashes, and store it (chunked in torrent file order so the
/// content id is deterministic across converters).
pub async fn convert<S: TorrentSource>(
    store: &Store,
    source: &S,
    input: &str,
) -> Result<(Manifest, TorrentMeta), BridgeError> {
    let dl = source.fetch(input).await?;
    if !verify_pieces(&dl.files, dl.meta.piece_length as usize, &dl.meta.piece_hashes) {
        return Err(BridgeError::PieceVerificationFailed);
    }
    let manifest = store.ingest_tree(&dl.files, Some(dl.meta.name.clone()))?;
    Ok((manifest, dl.meta))
}

/// Announce bridged content: provide it and publish the infohash -> root mapping.
pub async fn publish(net: &Network, manifest: &Manifest, infohash: &[u8]) -> Result<(), BridgeError> {
    net.provide(manifest).await?;
    net.put_mapping(infohash, manifest.root).await?;
    Ok(())
}

/// Try to fetch already-bridged content from the NP2PTP network by infohash.
/// If `verify` metadata is supplied, the downloaded content is re-checked against
/// the torrent piece hashes (rejecting a poisoned mapping).
pub async fn resolve(
    net: &Network,
    store: &Store,
    infohash: &[u8],
    verify: Option<&TorrentMeta>,
) -> Result<Option<Manifest>, BridgeError> {
    let Some(root) = net.get_mapping(infohash).await? else {
        return Ok(None);
    };
    for provider in net.find_providers(root).await? {
        let Ok(manifest) = net.download(root, provider, store).await else {
            continue;
        };
        if let Some(meta) = verify {
            let files = store.export_tree(&manifest)?;
            if !verify_pieces(&files, meta.piece_length as usize, &meta.piece_hashes) {
                continue; // mapping points at content that isn't this torrent
            }
        }
        return Ok(Some(manifest));
    }
    Ok(None)
}

/// The headline entry point: serve a torrent from the NP2PTP network if it's
/// already bridged, otherwise convert it from BitTorrent and bridge it.
pub async fn resolve_or_convert<S: TorrentSource>(
    net: &Network,
    store: &Store,
    source: &S,
    input: &str,
) -> Result<Outcome, BridgeError> {
    let meta = source.metadata(input).await?;
    let infohash = match &meta {
        Some(m) => m.infohash.clone(),
        None => source.infohash(input).await?,
    };

    if let Some(manifest) = resolve(net, store, &infohash, meta.as_ref()).await? {
        return Ok(Outcome { manifest, infohash, converted: false });
    }

    let (manifest, found_meta) = convert(store, source, input).await?;
    publish(net, &manifest, &found_meta.infohash).await?;
    Ok(Outcome { manifest, infohash: found_meta.infohash, converted: true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TmpDir(std::path::PathBuf);
    impl TmpDir {
        fn new() -> TmpDir {
            static C: AtomicU64 = AtomicU64::new(0);
            let n = C.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("np2ptp-bridge-{}-{}", std::process::id(), n));
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

    /// Build a self-consistent fake torrent meta from real file bytes.
    fn fake_meta(files: &[(String, Vec<u8>)], piece_length: usize, infohash: Vec<u8>) -> TorrentMeta {
        let mut data = Vec::new();
        for (_, b) in files {
            data.extend_from_slice(b);
        }
        let piece_hashes = data.chunks(piece_length).map(|c| Sha1::digest(c).into()).collect();
        TorrentMeta {
            infohash,
            name: "torrent".to_string(),
            files: files.iter().map(|(p, b)| TorrentFile { path: p.clone(), length: b.len() as u64 }).collect(),
            piece_length: piece_length as u32,
            piece_hashes,
        }
    }

    struct FakeSource {
        meta: TorrentMeta,
        files: Option<Vec<(String, Vec<u8>)>>, // None => fetch fails (refuses BitTorrent)
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
                None => Err(BridgeError::Source("refused BitTorrent fetch".into())),
            }
        }
    }

    #[test]
    fn verify_pieces_detects_corruption() {
        let files = vec![("a.bin".to_string(), sample(100_000, 1))];
        let meta = fake_meta(&files, 16384, vec![1u8; 20]);
        assert!(verify_pieces(&files, meta.piece_length as usize, &meta.piece_hashes));

        let mut bad = files.clone();
        bad[0].1[0] ^= 0xFF; // flip a byte
        assert!(!verify_pieces(&bad, meta.piece_length as usize, &meta.piece_hashes));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn convert_verifies_and_is_deterministic() {
        let files = vec![
            ("dir/a.bin".to_string(), sample(200_000, 2)),
            ("dir/b.bin".to_string(), sample(150_000, 3)),
        ];
        let meta = fake_meta(&files, 32768, vec![2u8; 20]);

        let d1 = TmpDir::new();
        let s1 = Store::open(d1.path()).unwrap();
        let src = FakeSource { meta: meta.clone(), files: Some(files.clone()) };
        let (m1, _) = convert(&s1, &src, "x.torrent").await.unwrap();

        // A second, independent converter of the same torrent gets the same root.
        let d2 = TmpDir::new();
        let s2 = Store::open(d2.path()).unwrap();
        let (m2, _) = convert(&s2, &src, "x.torrent").await.unwrap();
        assert_eq!(m1.root, m2.root, "converters must agree on the content id");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn convert_rejects_content_that_fails_piece_hashes() {
        let files = vec![("a.bin".to_string(), sample(100_000, 4))];
        // Meta whose piece hashes belong to *different* data.
        let other = vec![("a.bin".to_string(), sample(100_000, 999))];
        let meta = fake_meta(&other, 16384, vec![3u8; 20]);

        let d = TmpDir::new();
        let store = Store::open(d.path()).unwrap();
        let src = FakeSource { meta, files: Some(files) };
        assert!(matches!(convert(&store, &src, "x").await, Err(BridgeError::PieceVerificationFailed)));
    }
}
