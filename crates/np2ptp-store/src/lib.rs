//! `np2ptp-store` — a content-addressed chunk store with dedup.
//!
//! Chunks are written to disk under their BLAKE3 hash, so the same bytes are
//! only ever stored once — no matter how many files or versions contain them.
//! This is where the content-defined chunking from `np2ptp-core` turns into
//! real savings: edit a big file and re-share it, and only the changed chunks
//! cost new storage.
//!
//! Layout: `<root>/objects/<aa>/<full-hex>` where `aa` is the first hash byte,
//! a 256-way fan-out that keeps directories small. Writes are atomic
//! (temp file + rename) so a crash mid-write can't leave a corrupt object.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use np2ptp_core::hash::Hash;
use np2ptp_core::manifest::{Manifest, ManifestError};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("chunk {0} missing from store")]
    Missing(Hash),
    #[error("stored object {0} failed its hash check (corruption)")]
    Corrupt(Hash),
}

/// A disk-backed content-addressed store.
pub struct Store {
    objects: PathBuf,
}

impl Store {
    /// Open (creating if needed) a store rooted at `dir`.
    pub fn open(dir: impl AsRef<Path>) -> Result<Store, StoreError> {
        let objects = dir.as_ref().join("objects");
        fs::create_dir_all(&objects)?;
        Ok(Store { objects })
    }

    fn path_for(&self, h: &Hash) -> PathBuf {
        let hex = h.to_hex();
        self.objects.join(&hex[..2]).join(&hex)
    }

    /// True if this exact content is already stored.
    pub fn has(&self, h: &Hash) -> bool {
        self.path_for(h).exists()
    }

    /// Store `bytes`, returning their hash. Returns `(hash, newly_written)` so
    /// callers can measure dedup. A no-op if the content is already present.
    pub fn put(&self, bytes: &[u8]) -> Result<(Hash, bool), StoreError> {
        let h = Hash::of(bytes);
        let dest = self.path_for(&h);
        if dest.exists() {
            return Ok((h, false)); // dedup hit
        }
        let dir = dest.parent().expect("object path always has a parent");
        fs::create_dir_all(dir)?;

        // Atomic write: unique temp name in the same dir, then rename.
        let tmp = dir.join(format!("{}.tmp.{}", h.to_hex(), std::process::id()));
        fs::write(&tmp, bytes)?;
        match fs::rename(&tmp, &dest) {
            Ok(()) => Ok((h, true)),
            Err(e) => {
                // Lost a race or hit an error; clean up. If the dest now exists,
                // a concurrent writer won — treat as a dedup hit.
                let _ = fs::remove_file(&tmp);
                if dest.exists() {
                    Ok((h, false))
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Fetch content by hash, verifying it still matches on the way out.
    pub fn get(&self, h: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self.path_for(h);
        match fs::read(&path) {
            Ok(bytes) => {
                if Hash::of(&bytes) != *h {
                    return Err(StoreError::Corrupt(*h));
                }
                Ok(Some(bytes))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Number of distinct objects currently stored (walks the fan-out dirs).
    pub fn object_count(&self) -> Result<usize, StoreError> {
        let mut count = 0;
        for shard in fs::read_dir(&self.objects)? {
            let shard = shard?;
            if shard.file_type()?.is_dir() {
                for entry in fs::read_dir(shard.path())? {
                    let entry = entry?;
                    let name = entry.file_name();
                    // Skip leftover temp files from interrupted writes.
                    if !name.to_string_lossy().contains(".tmp.") {
                        count += 1;
                    }
                }
            }
        }
        Ok(count)
    }

    /// Chunk `data`, store every chunk (deduping), and return its manifest.
    pub fn ingest(&self, data: &[u8], name: Option<String>) -> Result<Manifest, StoreError> {
        let manifest = Manifest::from_bytes(data, name);
        for cref in &manifest.chunks {
            let start = cref.offset as usize;
            let end = start + cref.length as usize;
            self.put(&data[start..end])?;
        }
        Ok(manifest)
    }

    /// Rebuild the full content described by `manifest` from stored chunks,
    /// verifying every chunk against the manifest root.
    pub fn export(&self, manifest: &Manifest) -> Result<Vec<u8>, StoreError> {
        // Pull chunks out of the store; verification happens in `reconstruct`.
        let mut store_err: Option<StoreError> = None;
        let result = manifest.reconstruct(|h| match self.get(h) {
            Ok(opt) => opt,
            Err(e) => {
                store_err = Some(e);
                None
            }
        });
        if let Some(e) = store_err {
            return Err(e);
        }
        Ok(result?)
    }

    /// Chunk and store an ordered set of `(relative_path, bytes)` files (a whole
    /// directory tree), returning its manifest. Identical chunks — within or
    /// across files — are stored once.
    pub fn ingest_tree(
        &self,
        files: &[(String, Vec<u8>)],
        name: Option<String>,
    ) -> Result<Manifest, StoreError> {
        let refs: Vec<(String, &[u8])> =
            files.iter().map(|(p, d)| (p.clone(), d.as_slice())).collect();
        let manifest = Manifest::from_files(refs, name);
        // manifest.files preserves input order, so zip to recover each file's bytes.
        for (entry, (_, data)) in manifest.files.iter().zip(files.iter()) {
            let mut local = 0usize;
            for ci in entry.chunk_start..entry.chunk_start + entry.chunk_count {
                let len = manifest.chunks[ci].length as usize;
                self.put(&data[local..local + len])?;
                local += len;
            }
        }
        Ok(manifest)
    }

    /// Rebuild every file in `manifest` from stored chunks as `(path, bytes)`
    /// pairs, each fully verified against the Merkle root.
    pub fn export_tree(&self, manifest: &Manifest) -> Result<Vec<(String, Vec<u8>)>, StoreError> {
        let mut store_err: Option<StoreError> = None;
        let result = manifest.reconstruct_files(|h| match self.get(h) {
            Ok(opt) => opt,
            Err(e) => {
                store_err = Some(e);
                None
            }
        });
        if let Some(e) = store_err {
            return Err(e);
        }
        Ok(result?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Minimal self-cleaning temp dir so tests need no external crates (keeps the
    /// dependency tree pure-Rust, which matters on toolchains without a C linker).
    struct TmpDir(std::path::PathBuf);

    impl TmpDir {
        fn new() -> TmpDir {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("np2ptp-test-{}-{}", std::process::id(), n));
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

    /// High-entropy deterministic bytes (xorshift64*). Entropy matters here:
    /// low-entropy input gives FastCDC no boundaries to find, so it cuts at the
    /// max size — degenerating into fixed-size chunking and defeating the test.
    fn sample(n: usize, seed: u32) -> Vec<u8> {
        let mut x = 0x9E3779B97F4A7C15u64 ^ (seed as u64).wrapping_mul(0xD1B54A32D192ED03);
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
    fn put_get_round_trip() {
        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();
        let (h, new) = store.put(b"hello world").unwrap();
        assert!(new);
        assert!(store.has(&h));
        assert_eq!(store.get(&h).unwrap().unwrap(), b"hello world");
    }

    #[test]
    fn putting_same_bytes_twice_dedups() {
        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();
        let (_, first) = store.put(b"same").unwrap();
        let (_, second) = store.put(b"same").unwrap();
        assert!(first);
        assert!(!second); // second write deduped
        assert_eq!(store.object_count().unwrap(), 1);
    }

    #[test]
    fn ingest_then_export_round_trips() {
        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();
        let data = sample(300_000, 1);
        let m = store.ingest(&data, Some("a.bin".into())).unwrap();
        assert_eq!(store.export(&m).unwrap(), data);
    }

    #[test]
    fn similar_files_share_chunks_on_disk() {
        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();

        let base = sample(2_000_000, 7); // ~30 content-defined chunks at avg 64 KiB
        let mut edited = base.clone();
        edited.splice(10..10, *b"INSERTED"); // small edit near the front

        let m1 = store.ingest(&base, None).unwrap();
        let after_first = store.object_count().unwrap();
        let m2 = store.ingest(&edited, None).unwrap();
        let after_second = store.object_count().unwrap();

        let new_chunks = after_second - after_first;
        // Different roots (content changed)...
        assert_ne!(m1.root, m2.root);
        // ...but the second file should add only a handful of new chunks, far
        // fewer than storing it from scratch would (proves cross-file dedup).
        assert!(
            new_chunks < m2.chunks.len() / 2,
            "expected heavy dedup: added {new_chunks} new of {} chunks",
            m2.chunks.len()
        );

        // Both files still reconstruct correctly from the shared store.
        assert_eq!(store.export(&m1).unwrap(), base);
        assert_eq!(store.export(&m2).unwrap(), edited);
    }

    #[test]
    fn missing_chunk_export_errors() {
        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();
        // Manifest built but chunks never stored.
        let m = Manifest::from_bytes(&sample(120_000, 3), None);
        assert!(store.export(&m).is_err());
    }

    #[test]
    fn tree_ingest_export_round_trips() {
        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();
        let files = vec![
            ("a/one.bin".to_string(), sample(200_000, 1)),
            ("a/b/two.bin".to_string(), sample(150_000, 2)),
            ("readme.txt".to_string(), b"hello tree".to_vec()),
        ];
        let m = store.ingest_tree(&files, Some("a".into())).unwrap();
        assert_eq!(m.files.len(), 3);
        assert_eq!(store.export_tree(&m).unwrap(), files);
    }

    #[test]
    fn tree_stores_duplicate_file_once() {
        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();
        let dup = sample(400_000, 5);
        // Identical content under two different paths in the same tree.
        let files = vec![
            ("x/dup.bin".to_string(), dup.clone()),
            ("y/dup.bin".to_string(), dup.clone()),
        ];
        let m = store.ingest_tree(&files, None).unwrap();
        let per_file = m.chunks.len() / 2;
        assert!(per_file > 1, "duplicate file should span several chunks");
        // Two files in the manifest, but the shared chunks are stored only once.
        assert_eq!(store.object_count().unwrap(), per_file);
        assert_eq!(store.export_tree(&m).unwrap(), files);
    }
}
