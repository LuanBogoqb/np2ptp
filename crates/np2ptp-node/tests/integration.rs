//! End-to-end: link a file into a "seed" store, then download it into a fresh
//! client store through the public API — the same path the CLI drives.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use np2ptp_core::Hash;
use np2ptp_node::{download, download_with_progress, pack, read_dir_tree, write_tree, ChunkSource, NodeError, StoreSource};
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

#[test]
fn download_with_progress_reports_every_chunk_once() {
    let seed_dir = TmpDir::new();
    let seed_store = Store::open(seed_dir.path()).unwrap();
    let data = sample(300_000, 77);
    let manifest = seed_store.ingest(&data, None).unwrap();

    let client_dir = TmpDir::new();
    let source = StoreSource::open(seed_dir.path()).unwrap();
    let local = Store::open(client_dir.path()).unwrap();

    let mut calls: Vec<(usize, usize)> = Vec::new();
    let report = download_with_progress(&manifest, &source, &local, |done, total| {
        calls.push((done, total));
    })
    .unwrap();

    let total = manifest.chunks.len();
    assert!(total > 1, "want a multi-chunk transfer");
    assert_eq!(calls.len(), total);
    assert_eq!(calls.last().unwrap(), &(total, total));
    assert_eq!(report.fetched, total);
    assert_eq!(report.deduped, 0);
}

#[test]
fn pack_json_emits_valid_ndjson_and_a_final_result_event() {
    let dir = TmpDir::new();
    let input = dir.path().join("f.bin");
    std::fs::write(&input, sample(300_000, 50)).unwrap();
    let store_dir = dir.path().join("store");
    let out = dir.path().join("f.nptp");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_np2ptp"))
        .arg("pack")
        .arg(&input)
        .arg("--store")
        .arg(&store_dir)
        .arg("--out")
        .arg(&out)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert!(!lines.is_empty(), "expected at least one NDJSON line");

    let mut saw_result = false;
    for line in &lines {
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line not valid JSON: {line:?}: {e}"));
        assert_eq!(v["op"], "pack");
        if v["event"] == "result" {
            saw_result = true;
            assert!(v["root"].as_str().unwrap().starts_with("np2ptp:"));
            assert_eq!(v["bytes_total"], 300_000);
        }
    }
    assert!(saw_result, "expected a final result event, got: {stdout}");

    let last: serde_json::Value = serde_json::from_str(lines.last().unwrap())
        .unwrap_or_else(|e| panic!("last line not valid JSON: {:?}: {e}", lines.last()));
    assert_eq!(last["event"], "result", "expected the LAST line to be the result event, got: {stdout}");
}

#[test]
fn get_json_emits_valid_ndjson_and_a_final_result_event() {
    let dir = TmpDir::new();
    let input = dir.path().join("f.bin");
    std::fs::write(&input, sample(300_000, 51)).unwrap();
    let store_dir = dir.path().join("store");
    let out = dir.path().join("f.nptp");

    let pack_output = std::process::Command::new(env!("CARGO_BIN_EXE_np2ptp"))
        .arg("pack")
        .arg(&input)
        .arg("--store")
        .arg(&store_dir)
        .arg("--out")
        .arg(&out)
        .output()
        .unwrap();
    assert!(pack_output.status.success());

    let client_store = dir.path().join("client-store");
    let restored = dir.path().join("restored");
    let get_output = std::process::Command::new(env!("CARGO_BIN_EXE_np2ptp"))
        .arg("get")
        .arg(&out)
        .arg("--source")
        .arg(&store_dir)
        .arg("--store")
        .arg(&client_store)
        .arg("--out")
        .arg(&restored)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        get_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&get_output.stderr)
    );

    let stdout = String::from_utf8(get_output.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert!(!lines.is_empty());
    let mut saw_result = false;
    for line in &lines {
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line not valid JSON: {line:?}: {e}"));
        assert_eq!(v["op"], "get");
        if v["event"] == "result" {
            saw_result = true;
            assert_eq!(v["chunks_deduped"], 0);
            assert!(
                v["chunks_fetched"].as_u64().unwrap() > 0,
                "expected chunks_fetched > 0, got: {v}"
            );
        }
    }
    assert!(saw_result, "expected a final result event, got: {stdout}");

    let last: serde_json::Value = serde_json::from_str(lines.last().unwrap())
        .unwrap_or_else(|e| panic!("last line not valid JSON: {:?}: {e}", lines.last()));
    assert_eq!(last["event"], "result", "expected the LAST line to be the result event, got: {stdout}");
}
