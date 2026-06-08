//! End-to-end: link a file into a "seed" store, then download it into a fresh
//! client store through the public API — the same path the CLI drives.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use np2ptp_core::Hash;
use np2ptp_node::{download, pack, read_dir_tree, write_tree, ChunkSource, NodeError, StoreSource};
use np2ptp_store::Store;

struct TmpDir(std::path::PathBuf);

impl TmpDir {
    fn new() -> TmpDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("np2ptp-it-{}-{}", std::process::id(), n));
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

#[test]
fn pack_then_download_reconstructs_and_dedups() {
    let seed_dir = TmpDir::new();
    let client_dir = TmpDir::new();
    let data = sample(2_000_000, 5);

    // Linker: pack into the seed store and get a manifest (the .nptp contents).
    let seed = Store::open(seed_dir.path()).unwrap();
    let manifest = pack(&data, Some("f.bin".into()), &seed).unwrap();

    // Client: download from the seed into an empty client store.
    let source = StoreSource::open(seed_dir.path()).unwrap();
    let client = Store::open(client_dir.path()).unwrap();
    let report = download(&manifest, &source, &client).unwrap();
    assert_eq!(report.fetched, manifest.chunks.len());
    assert_eq!(report.deduped, 0);

    // Reconstructed bytes match the original exactly.
    assert_eq!(client.export(&manifest).unwrap(), data);

    // Downloading the same content again fetches nothing — all chunks dedup.
    let again = download(&manifest, &source, &client).unwrap();
    assert_eq!(again.fetched, 0);
    assert_eq!(again.deduped, manifest.chunks.len());
}

/// A source that hands back the right *shape* of data but the wrong *bytes* —
/// i.e. a lying peer. The client must reject it via Merkle verification.
struct EvilSource;

impl ChunkSource for EvilSource {
    fn fetch(&self, _hash: &Hash) -> Result<Option<Vec<u8>>, NodeError> {
        Ok(Some(vec![0xEE; 4096]))
    }
}

#[test]
fn download_rejects_a_lying_source() {
    let seed_dir = TmpDir::new();
    let client_dir = TmpDir::new();
    let data = sample(300_000, 9);

    let seed = Store::open(seed_dir.path()).unwrap();
    let manifest = pack(&data, None, &seed).unwrap();

    let client = Store::open(client_dir.path()).unwrap();
    let result = download(&manifest, &EvilSource, &client);
    assert!(matches!(result, Err(NodeError::BadChunk { .. })));
}

#[test]
fn pack_then_download_a_directory_tree() {
    // Build a small folder tree on disk.
    let srcdir = TmpDir::new();
    let base = srcdir.path();
    std::fs::create_dir_all(base.join("sub")).unwrap();
    std::fs::write(base.join("a.bin"), sample(200_000, 1)).unwrap();
    std::fs::write(base.join("sub").join("b.bin"), sample(150_000, 2)).unwrap();
    std::fs::write(base.join("readme.txt"), b"hello tree").unwrap();

    let files = read_dir_tree(base).unwrap();
    assert_eq!(files.len(), 3);

    // Linker: pack the whole tree into a seed store.
    let seed_dir = TmpDir::new();
    let seed = Store::open(seed_dir.path()).unwrap();
    let manifest = seed.ingest_tree(&files, Some("tree".into())).unwrap();
    assert_eq!(manifest.files.len(), 3);
    assert!(!manifest.is_single_file());

    // Client: download every chunk, then materialize the tree to an output dir.
    let client_dir = TmpDir::new();
    let client = Store::open(client_dir.path()).unwrap();
    let source = StoreSource::open(seed_dir.path()).unwrap();
    let report = download(&manifest, &source, &client).unwrap();
    assert_eq!(report.fetched, manifest.chunks.len());

    let out_dir = TmpDir::new();
    let rebuilt = client.export_tree(&manifest).unwrap();
    write_tree(out_dir.path(), &rebuilt).unwrap();

    // Every original file is back at the right relative path with identical bytes.
    for (rel, bytes) in &files {
        let mut p = out_dir.path().to_path_buf();
        for c in rel.split('/') {
            p.push(c);
        }
        assert_eq!(&std::fs::read(&p).unwrap(), bytes, "mismatch for {rel}");
    }
}

/// A source missing the content entirely should fail cleanly, not hang or panic.
#[test]
fn download_reports_missing_chunks() {
    let empty_seed = TmpDir::new();
    let client_dir = TmpDir::new();
    let data = sample(200_000, 11);

    // Build a manifest but never store its chunks in the source.
    let scratch = TmpDir::new();
    let manifest = pack(&data, None, &Store::open(scratch.path()).unwrap()).unwrap();

    let source = StoreSource::open(empty_seed.path()).unwrap();
    let client = Store::open(client_dir.path()).unwrap();
    let result = download(&manifest, &source, &client);
    assert!(matches!(result, Err(NodeError::MissingChunk(_))));
}
