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

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use fastcdc::v2020::StreamCDC;
use np2ptp_core::chunk::{AVG_CHUNK, MAX_CHUNK, MIN_CHUNK};
use np2ptp_core::hash::Hash;
use np2ptp_core::manifest::{ChunkRef, FileEntry, Manifest, ManifestError};
use np2ptp_core::merkle_root;

/// Where an externally-referenced chunk's bytes actually live: a byte range
/// inside a file we don't own a copy of.
type RefLoc = (PathBuf, u64, u32);

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
    #[error("refusing unsafe path in manifest: {0:?}")]
    UnsafePath(String),
}

/// A disk-backed content-addressed store.
pub struct Store {
    objects: PathBuf,
    /// Sidecar index for `pack --no-copy`: chunks whose bytes were never
    /// copied in, only referenced by (path, offset, length) in a source file
    /// the caller keeps in place. Appended to as `<dir>/refs.tsv`.
    refs_path: PathBuf,
    refs: RwLock<HashMap<Hash, RefLoc>>,
}

impl Store {
    /// Open (creating if needed) a store rooted at `dir`.
    pub fn open(dir: impl AsRef<Path>) -> Result<Store, StoreError> {
        let objects = dir.as_ref().join("objects");
        fs::create_dir_all(&objects)?;
        let refs_path = dir.as_ref().join("refs.tsv");
        let refs = load_refs(&refs_path)?;
        Ok(Store { objects, refs_path, refs: RwLock::new(refs) })
    }

    /// The directory this store was opened with (what was passed to
    /// [`Store::open`]) — used by callers that need to keep sidecar files
    /// (e.g. a persisted network identity or ledger) next to the store.
    pub fn root(&self) -> PathBuf {
        self.objects.parent().expect("objects is always <dir>/objects").to_path_buf()
    }

    fn path_for(&self, h: &Hash) -> PathBuf {
        let hex = h.to_hex();
        self.objects.join(&hex[..2]).join(&hex)
    }

    /// True if this exact content is already stored (copied in or referenced).
    pub fn has(&self, h: &Hash) -> bool {
        self.path_for(h).exists() || self.refs.read().unwrap().contains_key(h)
    }

    /// Record that `h`'s bytes live at `data[offset..offset+length]` in
    /// `source` rather than copying them into `objects/`. `source` should
    /// already be an absolute path (canonicalized once by the caller) since
    /// this reference must still resolve after the process (and its cwd)
    /// changes — e.g. `pack` now, `serve` later.
    fn add_reference(&self, h: Hash, source: &Path, offset: u64, length: u32) -> io::Result<()> {
        let mut line = String::new();
        line.push_str(&h.to_hex());
        line.push('\t');
        line.push_str(&offset.to_string());
        line.push('\t');
        line.push_str(&length.to_string());
        line.push('\t');
        line.push_str(&source.to_string_lossy());
        line.push('\n');
        let mut f = fs::OpenOptions::new().create(true).append(true).open(&self.refs_path)?;
        f.write_all(line.as_bytes())?;
        self.refs.write().unwrap().insert(h, (source.to_path_buf(), offset, length));
        Ok(())
    }

    /// Read a referenced chunk's bytes straight from its source file,
    /// verifying them against `h` on the way out (a moved/edited source file
    /// is caught here, same as on-disk corruption).
    fn get_reference(&self, h: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let Some((path, offset, length)) = self.refs.read().unwrap().get(h).cloned() else {
            return Ok(None);
        };
        let mut file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0u8; length as usize];
        file.read_exact(&mut bytes)?;
        if Hash::of(&bytes) != *h {
            return Err(StoreError::Corrupt(*h));
        }
        Ok(Some(bytes))
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
    /// Checks copied-in objects first, then the `--no-copy` reference index.
    pub fn get(&self, h: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self.path_for(h);
        match fs::read(&path) {
            Ok(bytes) => {
                if Hash::of(&bytes) != *h {
                    return Err(StoreError::Corrupt(*h));
                }
                Ok(Some(bytes))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => self.get_reference(h),
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

    // --- streaming variants (for content too large to hold in memory) --------

    /// Chunk a single file straight from disk (one chunk in memory at a time),
    /// storing each chunk. Returns the file's chunk refs (offsets relative to the
    /// file start) and its size.
    pub fn ingest_file_streaming(&self, path: &Path) -> Result<(Vec<ChunkRef>, u64), StoreError> {
        self.ingest_file_streaming_impl(path, false, |_, _, _| {})
    }

    /// Like [`Store::ingest_file_streaming`], but instead of copying each
    /// chunk's bytes into `objects/`, records where they live in `path` and
    /// reads them from there on demand. Halves disk usage for a file you're
    /// only seeding (not receiving), at the cost of `path` needing to stay
    /// put and unchanged — see [`Store::get`]'s reference fallback.
    pub fn ingest_file_streaming_no_copy(&self, path: &Path) -> Result<(Vec<ChunkRef>, u64), StoreError> {
        self.ingest_file_streaming_impl(path, true, |_, _, _| {})
    }

    fn ingest_file_streaming_impl(
        &self,
        path: &Path,
        no_copy: bool,
        mut on_progress: impl FnMut(u64, u64, bool),
    ) -> Result<(Vec<ChunkRef>, u64), StoreError> {
        // Resolved once up front: the reference must still be valid from a
        // different working directory in a later `serve` process.
        let source = if no_copy { Some(fs::canonicalize(path)?) } else { None };
        let total = fs::metadata(path)?.len();
        let reader = BufReader::new(File::open(path)?);
        let mut refs = Vec::new();
        let mut offset = 0u64;
        for chunk in StreamCDC::new(reader, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK) {
            let chunk = chunk.map_err(|e| io::Error::other(e.to_string()))?;
            let length = chunk.data.len() as u32;
            let (hash, is_new) = match &source {
                Some(src) => {
                    let hash = Hash::of(&chunk.data);
                    let is_new = !self.has(&hash);
                    self.add_reference(hash, src, offset, length)?;
                    (hash, is_new)
                }
                None => self.put(&chunk.data)?,
            };
            refs.push(ChunkRef { hash, offset, length });
            offset += length as u64;
            on_progress(offset, total, is_new);
        }
        Ok((refs, offset))
    }

    /// Chunk and store a tree of `(relative_path, disk_path)` files by streaming
    /// each from disk, building the manifest. Never holds a whole file in memory.
    pub fn ingest_tree_files(
        &self,
        files: &[(String, PathBuf)],
        name: Option<String>,
    ) -> Result<Manifest, StoreError> {
        self.ingest_tree_files_impl(files, name, false, |_, _, _| {})
    }

    /// Like [`Store::ingest_tree_files`], but every file is referenced
    /// in place instead of copied — see [`Store::ingest_file_streaming_no_copy`].
    pub fn ingest_tree_files_no_copy(
        &self,
        files: &[(String, PathBuf)],
        name: Option<String>,
    ) -> Result<Manifest, StoreError> {
        self.ingest_tree_files_impl(files, name, true, |_, _, _| {})
    }

    /// Like [`Store::ingest_tree_files`], but calls `on_progress(bytes_done,
    /// bytes_total, chunk_was_new)` as each chunk is processed — `bytes_total`
    /// is the sum of every file's size, known upfront; `chunk_was_new` is
    /// false for a chunk that was already in the store (a dedup hit).
    pub fn ingest_tree_files_with_progress(
        &self,
        files: &[(String, PathBuf)],
        name: Option<String>,
        on_progress: impl FnMut(u64, u64, bool),
    ) -> Result<Manifest, StoreError> {
        self.ingest_tree_files_impl(files, name, false, on_progress)
    }

    /// Like [`Store::ingest_tree_files_no_copy`], with the same progress
    /// callback as [`Store::ingest_tree_files_with_progress`].
    pub fn ingest_tree_files_no_copy_with_progress(
        &self,
        files: &[(String, PathBuf)],
        name: Option<String>,
        on_progress: impl FnMut(u64, u64, bool),
    ) -> Result<Manifest, StoreError> {
        self.ingest_tree_files_impl(files, name, true, on_progress)
    }

    fn ingest_tree_files_impl(
        &self,
        files: &[(String, PathBuf)],
        name: Option<String>,
        no_copy: bool,
        mut on_progress: impl FnMut(u64, u64, bool),
    ) -> Result<Manifest, StoreError> {
        let total: u64 = files
            .iter()
            .map(|(_, p)| fs::metadata(p).map(|m| m.len()).unwrap_or(0))
            .sum();
        let mut chunks: Vec<ChunkRef> = Vec::new();
        let mut entries: Vec<FileEntry> = Vec::new();
        let mut global: u64 = 0;
        for (rel, disk) in files {
            let base = global;
            let (refs, size) = self.ingest_file_streaming_impl(disk, no_copy, |done, _file_total, is_new| {
                on_progress(base + done, total, is_new);
            })?;
            let chunk_start = chunks.len();
            for r in &refs {
                chunks.push(ChunkRef { hash: r.hash, offset: global + r.offset, length: r.length });
            }
            entries.push(FileEntry { path: rel.clone(), size, chunk_start, chunk_count: refs.len() });
            global += size;
        }
        let hashes: Vec<Hash> = chunks.iter().map(|c| c.hash).collect();
        Ok(Manifest { root: merkle_root(&hashes), total_size: global, chunks, files: entries, name })
    }

    /// Stream the whole content (all files concatenated) to a writer, verifying
    /// each chunk's content hash. For single-file content this is the file.
    pub fn export_to<W: Write>(&self, manifest: &Manifest, writer: W) -> Result<(), StoreError> {
        self.export_to_with_progress(manifest, writer, |_, _| {})
    }

    /// Like [`Store::export_to`], but calls `on_progress(chunks_done,
    /// chunks_total)` once per chunk as it's read back, verified, and
    /// written — this phase (re-reading every chunk from disk and hashing
    /// it again) has no relation to network download progress and can take
    /// a while on its own for large content.
    pub fn export_to_with_progress<W: Write>(
        &self,
        manifest: &Manifest,
        writer: W,
        mut on_progress: impl FnMut(usize, usize),
    ) -> Result<(), StoreError> {
        if !manifest.root_is_consistent() {
            return Err(StoreError::Corrupt(manifest.root));
        }
        let total = manifest.chunks.len();
        let mut w = BufWriter::new(writer);
        for (i, cref) in manifest.chunks.iter().enumerate() {
            let bytes = self.get(&cref.hash)?.ok_or(StoreError::Missing(cref.hash))?;
            if !manifest.chunk_hash_ok(i, &bytes) {
                return Err(StoreError::Corrupt(cref.hash));
            }
            w.write_all(&bytes)?;
            on_progress(i + 1, total);
        }
        w.flush()?;
        Ok(())
    }

    /// Write every file of `manifest` into `out_dir`, streaming each file
    /// chunk-by-chunk. Verifies chunk hashes and rejects unsafe paths.
    pub fn export_tree_to_dir(&self, manifest: &Manifest, out_dir: &Path) -> Result<(), StoreError> {
        self.export_tree_to_dir_with_progress(manifest, out_dir, |_, _| {})
    }

    /// Like [`Store::export_tree_to_dir`], but calls `on_progress(chunks_done,
    /// chunks_total)` once per chunk, cumulative across every file in the tree
    /// (not reset per file) — see [`Store::export_to_with_progress`].
    pub fn export_tree_to_dir_with_progress(
        &self,
        manifest: &Manifest,
        out_dir: &Path,
        mut on_progress: impl FnMut(usize, usize),
    ) -> Result<(), StoreError> {
        if !manifest.root_is_consistent() {
            return Err(StoreError::Corrupt(manifest.root));
        }
        let total = manifest.chunks.len();
        let mut done = 0;
        for entry in &manifest.files {
            let mut dest = out_dir.to_path_buf();
            for comp in entry.path.split('/') {
                if comp.is_empty() || comp == "." || comp == ".." {
                    return Err(StoreError::UnsafePath(entry.path.clone()));
                }
                dest.push(comp);
            }
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut w = BufWriter::new(File::create(&dest)?);
            for ci in entry.chunk_start..entry.chunk_start + entry.chunk_count {
                let cref = &manifest.chunks[ci];
                let bytes = self.get(&cref.hash)?.ok_or(StoreError::Missing(cref.hash))?;
                if !manifest.chunk_hash_ok(ci, &bytes) {
                    return Err(StoreError::Corrupt(cref.hash));
                }
                w.write_all(&bytes)?;
                done += 1;
                on_progress(done, total);
            }
            w.flush()?;
        }
        Ok(())
    }
}

/// Load `refs.tsv` (`<hash-hex>\t<offset>\t<length>\t<path>` per line), if it
/// exists. A truncated last line (e.g. process killed mid-append) is skipped
/// rather than failing the whole store open — every other line still lists
/// a valid, independently-verified-on-read reference.
fn load_refs(path: &Path) -> Result<HashMap<Hash, RefLoc>, StoreError> {
    let mut refs = HashMap::new();
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(refs),
        Err(e) => return Err(e.into()),
    };
    for line in text.lines() {
        let mut fields = line.splitn(4, '\t');
        let (Some(hash), Some(offset), Some(length), Some(path)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(hash), Ok(offset), Ok(length)) =
            (Hash::from_hex(hash), offset.parse::<u64>(), length.parse::<u32>())
        else {
            continue;
        };
        refs.insert(hash, (PathBuf::from(path), offset, length));
    }
    Ok(refs)
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

    #[test]
    fn streaming_ingest_matches_in_memory_root() {
        let data = sample(2_000_000, 42);

        let d1 = TmpDir::new();
        let in_mem = Store::open(d1.path()).unwrap().ingest(&data, Some("f.bin".into())).unwrap();

        let fdir = TmpDir::new();
        let fpath = fdir.path().join("f.bin");
        std::fs::write(&fpath, &data).unwrap();
        let d2 = TmpDir::new();
        let streamed = Store::open(d2.path())
            .unwrap()
            .ingest_tree_files(&[("f.bin".to_string(), fpath)], Some("f.bin".into()))
            .unwrap();

        // Critical: both chunking paths must produce the same content id.
        assert_eq!(in_mem.root, streamed.root);
        assert_eq!(in_mem.chunks.len(), streamed.chunks.len());
    }

    #[test]
    fn no_copy_pack_does_not_duplicate_bytes_on_disk() {
        let data = sample(2_000_000, 11);
        let fdir = TmpDir::new();
        let fpath = fdir.path().join("f.bin");
        std::fs::write(&fpath, &data).unwrap();

        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();
        let m = store
            .ingest_tree_files_no_copy(&[("f.bin".to_string(), fpath)], Some("f.bin".into()))
            .unwrap();

        // No chunk bytes were copied into objects/ ...
        assert_eq!(store.object_count().unwrap(), 0);
        // ...yet it still round-trips, reading straight from the source file.
        assert_eq!(store.export(&m).unwrap(), data);

        // Same bytes, same content id as a normal copying pack (determinism
        // doesn't depend on --no-copy).
        let copy_dir = TmpDir::new();
        let copied = Store::open(copy_dir.path()).unwrap().ingest(&data, None).unwrap();
        assert_eq!(copied.root, m.root);
    }

    #[test]
    fn no_copy_pack_survives_a_reopened_store() {
        let data = sample(500_000, 12);
        let fdir = TmpDir::new();
        let fpath = fdir.path().join("f.bin");
        std::fs::write(&fpath, &data).unwrap();

        let dir = TmpDir::new();
        let m = Store::open(dir.path())
            .unwrap()
            .ingest_tree_files_no_copy(&[("f.bin".to_string(), fpath)], None)
            .unwrap();

        // A brand new `Store` handle (as `serve` would open in a later
        // process) must still resolve the reference from refs.tsv.
        let reopened = Store::open(dir.path()).unwrap();
        assert_eq!(reopened.export(&m).unwrap(), data);
    }

    #[test]
    fn no_copy_pack_detects_a_changed_source_file() {
        let data = sample(500_000, 13);
        let fdir = TmpDir::new();
        let fpath = fdir.path().join("f.bin");
        std::fs::write(&fpath, &data).unwrap();

        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();
        let m = store
            .ingest_tree_files_no_copy(&[("f.bin".to_string(), fpath.clone())], None)
            .unwrap();

        // The referenced file changes after packing (e.g. the user moved a
        // different file to the same path) — export must fail, not silently
        // hand back the wrong bytes.
        std::fs::write(&fpath, sample(500_000, 99)).unwrap();
        assert!(store.export(&m).is_err());
    }

    #[test]
    fn pack_with_progress_reports_bytes_and_dedup_flag() {
        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();

        let fdir = TmpDir::new();
        let fpath = fdir.path().join("f.bin");
        let data = sample(500_000, 20);
        std::fs::write(&fpath, &data).unwrap();

        let mut calls: Vec<(u64, u64, bool)> = Vec::new();
        let manifest = store
            .ingest_tree_files_with_progress(
                &[("f.bin".to_string(), fpath.clone())],
                None,
                |done, total, is_new| calls.push((done, total, is_new)),
            )
            .unwrap();

        assert!(!calls.is_empty());
        // Every call reports the same (correct) total; done is monotonic and
        // reaches it on the last call.
        let total = calls[0].1;
        assert_eq!(total, data.len() as u64);
        assert!(calls.windows(2).all(|w| w[0].0 <= w[1].0));
        assert_eq!(calls.last().unwrap().0, total);
        // First pack of brand-new content: every chunk is new.
        assert!(calls.iter().all(|(_, _, is_new)| *is_new));

        // Re-packing the identical file into the SAME store must report every
        // chunk as a dedup hit (not new).
        let mut calls2: Vec<(u64, u64, bool)> = Vec::new();
        store
            .ingest_tree_files_with_progress(
                &[("f.bin".to_string(), fpath)],
                None,
                |done, total, is_new| calls2.push((done, total, is_new)),
            )
            .unwrap();
        assert!(calls2.iter().all(|(_, _, is_new)| !is_new));
        assert_eq!(manifest.total_size, data.len() as u64);
    }

    #[test]
    fn pack_with_progress_accumulates_bytes_across_multiple_files() {
        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();

        let fdir = TmpDir::new();
        let a_path = fdir.path().join("a.bin");
        let b_path = fdir.path().join("b.bin");
        let a_data = sample(200_000, 30);
        let b_data = sample(150_000, 31);
        std::fs::write(&a_path, &a_data).unwrap();
        std::fs::write(&b_path, &b_data).unwrap();

        let files = vec![
            ("a.bin".to_string(), a_path),
            ("b.bin".to_string(), b_path),
        ];
        let total_size = (a_data.len() + b_data.len()) as u64;

        let mut calls: Vec<(u64, u64, bool)> = Vec::new();
        store
            .ingest_tree_files_with_progress(&files, None, |done, total, is_new| {
                calls.push((done, total, is_new));
            })
            .unwrap();

        assert!(!calls.is_empty());
        // Every call reports the whole-tree total, not a per-file total.
        assert!(calls.iter().all(|(_, total, _)| *total == total_size));
        // done is monotonic and reaches the full cross-file total on the last call
        // (proves the second file's progress continues from the first file's,
        // rather than resetting to 0).
        assert!(calls.windows(2).all(|w| w[0].0 <= w[1].0));
        assert_eq!(calls.last().unwrap().0, total_size);
        // At least one call must report done > a_data.len(), proving progress
        // continued into the second file rather than stopping at the first.
        assert!(calls.iter().any(|(done, _, _)| *done > a_data.len() as u64));
    }

    #[test]
    fn root_returns_the_directory_passed_to_open() {
        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.root(), dir.path());
    }

    #[test]
    fn no_copy_pack_with_progress_reports_dedup_correctly() {
        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();

        let fdir = TmpDir::new();
        let fpath = fdir.path().join("f.bin");
        let data = sample(400_000, 32);
        std::fs::write(&fpath, &data).unwrap();

        let mut calls: Vec<(u64, u64, bool)> = Vec::new();
        store
            .ingest_tree_files_no_copy_with_progress(
                &[("f.bin".to_string(), fpath.clone())],
                None,
                |done, total, is_new| calls.push((done, total, is_new)),
            )
            .unwrap();
        assert!(!calls.is_empty());
        assert!(calls.iter().all(|(_, _, is_new)| *is_new), "first no-copy pack: every chunk is new");

        // Re-pack the identical file (same store, same path) — every chunk must
        // now report as a dedup hit (chunk_was_new = false), proving the no-copy
        // path's `is_new = !self.has(&hash)` check works, not just the copy path's.
        let mut calls2: Vec<(u64, u64, bool)> = Vec::new();
        store
            .ingest_tree_files_no_copy_with_progress(
                &[("f.bin".to_string(), fpath)],
                None,
                |done, total, is_new| calls2.push((done, total, is_new)),
            )
            .unwrap();
        assert!(calls2.iter().all(|(_, _, is_new)| !is_new), "re-pack via no-copy: every chunk is a dedup hit");
    }

    #[test]
    fn streaming_export_round_trips_a_tree() {
        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();

        let fdir = TmpDir::new();
        std::fs::create_dir_all(fdir.path().join("sub")).unwrap();
        let a = fdir.path().join("a.bin");
        let b = fdir.path().join("sub").join("b.bin");
        std::fs::write(&a, sample(300_000, 1)).unwrap();
        std::fs::write(&b, sample(200_000, 2)).unwrap();

        let files = vec![("a.bin".to_string(), a.clone()), ("sub/b.bin".to_string(), b.clone())];
        let m = store.ingest_tree_files(&files, Some("tree".into())).unwrap();

        let out = TmpDir::new();
        store.export_tree_to_dir(&m, out.path()).unwrap();
        assert_eq!(std::fs::read(out.path().join("a.bin")).unwrap(), std::fs::read(&a).unwrap());
        assert_eq!(
            std::fs::read(out.path().join("sub").join("b.bin")).unwrap(),
            std::fs::read(&b).unwrap()
        );
    }

    #[test]
    fn export_with_progress_reports_every_chunk_once() {
        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();
        let data = sample(400_000, 40);
        let m = store.ingest(&data, None).unwrap();
        let total = m.chunks.len();
        assert!(total > 1, "want a multi-chunk transfer");

        let mut calls: Vec<(usize, usize)> = Vec::new();
        let mut out = Vec::new();
        store
            .export_to_with_progress(&m, &mut out, |done, total| calls.push((done, total)))
            .unwrap();

        assert_eq!(out, data);
        assert_eq!(calls.len(), total);
        assert!(calls.windows(2).all(|w| w[0].0 < w[1].0), "done must be strictly increasing");
        assert_eq!(calls.last().unwrap(), &(total, total));
    }

    #[test]
    fn export_tree_with_progress_accumulates_across_files() {
        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();

        let fdir = TmpDir::new();
        std::fs::create_dir_all(fdir.path().join("sub")).unwrap();
        let a = fdir.path().join("a.bin");
        let b = fdir.path().join("sub").join("b.bin");
        std::fs::write(&a, sample(300_000, 41)).unwrap();
        std::fs::write(&b, sample(200_000, 42)).unwrap();

        let files = vec![("a.bin".to_string(), a.clone()), ("sub/b.bin".to_string(), b.clone())];
        let m = store.ingest_tree_files(&files, Some("tree".into())).unwrap();
        let total = m.chunks.len();

        let out = TmpDir::new();
        let mut calls: Vec<(usize, usize)> = Vec::new();
        store
            .export_tree_to_dir_with_progress(&m, out.path(), |done, total| calls.push((done, total)))
            .unwrap();

        assert_eq!(std::fs::read(out.path().join("a.bin")).unwrap(), std::fs::read(&a).unwrap());
        assert_eq!(
            std::fs::read(out.path().join("sub").join("b.bin")).unwrap(),
            std::fs::read(&b).unwrap()
        );
        // Every call reports the whole-tree total, and progress reaches it —
        // proves the second file's count continues from the first's rather
        // than resetting (the same cross-file bug class as the pack side).
        assert!(calls.iter().all(|(_, t)| *t == total));
        assert_eq!(calls.last().unwrap().0, total);
        assert!(calls.windows(2).all(|w| w[0].0 < w[1].0), "done must be strictly increasing");
    }
}
