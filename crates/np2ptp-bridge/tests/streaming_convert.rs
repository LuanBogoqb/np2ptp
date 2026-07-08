//! Streaming local-conversion path: verifies piece hashes and ingests torrent
//! content read from real files on disk (never the whole torrent in memory),
//! and checks it agrees with the existing in-memory `convert()` path.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use np2ptp_bridge::{convert, verify_pieces_streaming, BridgeError, TorrentFile, TorrentMeta};
use np2ptp_store::Store;
use sha1::{Digest, Sha1};

struct TmpDir(std::path::PathBuf);
impl TmpDir {
    fn new() -> TmpDir {
        static C: AtomicU64 = AtomicU64::new(0);
        let n = C.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("np2ptp-stream-{}-{}", std::process::id(), n));
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

fn piece_hashes_for(files: &[(String, Vec<u8>)], piece_length: usize) -> Vec<[u8; 20]> {
    let mut data = Vec::new();
    for (_, b) in files {
        data.extend_from_slice(b);
    }
    data.chunks(piece_length).map(|c| Sha1::digest(c).into()).collect()
}

#[test]
fn streaming_verifier_agrees_with_in_memory_verify_pieces() {
    // Two files, piece length chosen so a piece spans the file boundary, plus
    // a final undersized piece.
    let files = vec![
        ("a.bin".to_string(), sample(50_000, 1)),
        ("b.bin".to_string(), sample(37_777, 2)),
    ];
    let piece_length = 16_384;
    let hashes = piece_hashes_for(&files, piece_length);

    let dir = TmpDir::new();
    let mut disk_files = Vec::new();
    for (name, bytes) in &files {
        let p = dir.path().join(name);
        std::fs::write(&p, bytes).unwrap();
        disk_files.push((name.clone(), p));
    }

    assert!(verify_pieces_streaming(&disk_files, piece_length, &hashes).is_ok());

    // Corrupt one byte on disk -> must be rejected.
    let mut bad = std::fs::read(&disk_files[1].1).unwrap();
    bad[0] ^= 0xFF;
    std::fs::write(&disk_files[1].1, &bad).unwrap();
    assert!(matches!(
        verify_pieces_streaming(&disk_files, piece_length, &hashes),
        Err(BridgeError::PieceVerificationFailed)
    ));
}

#[test]
fn streaming_verifier_rejects_piece_count_mismatch() {
    let files = [("a.bin".to_string(), sample(1000, 3))];
    let dir = TmpDir::new();
    let p = dir.path().join("a.bin");
    std::fs::write(&p, &files[0].1).unwrap();
    let disk_files = vec![("a.bin".to_string(), p)];

    // Hashes for a completely different (empty) piece list.
    let wrong_hashes: Vec<[u8; 20]> = vec![];
    assert!(matches!(
        verify_pieces_streaming(&disk_files, 500, &wrong_hashes),
        Err(BridgeError::PieceVerificationFailed)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn convert_local_matches_in_memory_convert_root() {
    use np2ptp_bridge::{convert_local, TorrentSource, TorrentDownload};

    let files = vec![
        ("dir/a.bin".to_string(), sample(200_000, 4)),
        ("dir/b.bin".to_string(), sample(150_000, 5)),
    ];
    let piece_length = 32_768;
    let piece_hashes = piece_hashes_for(&files, piece_length);
    let meta = TorrentMeta {
        infohash: vec![5u8; 20],
        name: "pack".to_string(),
        files: files.iter().map(|(p, b)| TorrentFile { path: p.clone(), length: b.len() as u64 }).collect(),
        piece_length: piece_length as u32,
        piece_hashes,
    };

    // Reference: existing in-memory convert().
    struct FakeSource {
        meta: TorrentMeta,
        files: Vec<(String, Vec<u8>)>,
    }
    impl TorrentSource for FakeSource {
        async fn infohash(&self, _: &str) -> Result<Vec<u8>, BridgeError> {
            Ok(self.meta.infohash.clone())
        }
        async fn metadata(&self, _: &str) -> Result<Option<TorrentMeta>, BridgeError> {
            Ok(Some(self.meta.clone()))
        }
        async fn fetch(&self, _: &str) -> Result<TorrentDownload, BridgeError> {
            Ok(TorrentDownload { meta: self.meta.clone(), files: self.files.clone() })
        }
    }
    let ref_dir = TmpDir::new();
    let ref_store = Store::open(ref_dir.path()).unwrap();
    let src = FakeSource { meta: meta.clone(), files: files.clone() };
    let (ref_manifest, _) = convert(&ref_store, &src, "x.torrent").await.unwrap();

    // Streaming: files written to disk, then convert_local reads them back.
    let data_dir = TmpDir::new();
    for (rel, bytes) in &files {
        let p = data_dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
    }
    let store_dir = TmpDir::new();
    let store = Store::open(store_dir.path()).unwrap();
    let manifest = convert_local(&store, &meta, data_dir.path(), false).unwrap();

    assert_eq!(manifest.root, ref_manifest.root, "streaming and in-memory converters must agree on the content id");

    // And it's actually retrievable/correct.
    let rebuilt = store.export_tree(&manifest).unwrap();
    let mut rebuilt_sorted = rebuilt.clone();
    rebuilt_sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut expected_sorted = files.clone();
    expected_sorted.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(rebuilt_sorted, expected_sorted);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn convert_local_rejects_corrupted_file_on_disk() {
    use np2ptp_bridge::convert_local;

    let files = vec![("a.bin".to_string(), sample(60_000, 6))];
    let piece_length = 16_384;
    let piece_hashes = piece_hashes_for(&files, piece_length);
    let meta = TorrentMeta {
        infohash: vec![6u8; 20],
        name: "pack".to_string(),
        files: files.iter().map(|(p, b)| TorrentFile { path: p.clone(), length: b.len() as u64 }).collect(),
        piece_length: piece_length as u32,
        piece_hashes,
    };

    let data_dir = TmpDir::new();
    let mut bad = files[0].1.clone();
    bad[0] ^= 0xFF;
    std::fs::write(data_dir.path().join("a.bin"), &bad).unwrap();

    let store_dir = TmpDir::new();
    let store = Store::open(store_dir.path()).unwrap();
    assert!(matches!(
        convert_local(&store, &meta, data_dir.path(), false),
        Err(BridgeError::PieceVerificationFailed)
    ));
}
