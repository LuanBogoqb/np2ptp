//! `np2ptp-store` — a content-addressed chunk store with dedup.
//!
//! Chunks are content-addressed by their BLAKE3 hash, so the same bytes are
//! only ever stored once — no matter how many files or versions contain them.
//! This is where the content-defined chunking from `np2ptp-core` turns into
//! real savings: edit a big file and re-share it, and only the changed chunks
//! cost new storage.
//!
//! Layout: new chunks are appended to `<root>/packs/<id>.pack` (rotated at
//! [`PACK_ROTATE_SIZE`]), with a hash → `(pack_id, offset, length)` index in
//! `<root>/packs/index` — one `write` per chunk on an already-open file
//! handle, instead of a whole new small file (open + write + rename) per
//! chunk. A store opened from before this existed still reads its old
//! `<root>/objects/<aa>/<full-hex>` layout (`aa` = first hash byte, a 256-way
//! fan-out) — `get`/`has`/`put`'s dedup check all fall back to it — but never
//! write to it again; every new chunk goes to the packfile.
//!
//! `--no-copy` references (a chunk's bytes read from a source file the caller
//! keeps in place, never copied in) are a separate mechanism from either
//! layout above — see `refs.tsv` below.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

/// Where a packed chunk's bytes live: a byte range inside one of this store's
/// own `packs/<id>.pack` files.
type PackLoc = (u32, u64, u32);

/// Roll over to a new pack file once the current one reaches this size, so
/// no single file grows unbounded and a crash mid-write only risks the tail
/// of one pack, not the whole store.
const PACK_ROTATE_SIZE: u64 = 256 * 1024 * 1024;

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
    packs_dir: PathBuf,
    pack_index_path: PathBuf,
    pack_index: RwLock<HashMap<Hash, PackLoc>>,
    /// Bytes of `pack_index_path` already parsed into `pack_index` — lets
    /// [`Store::refresh_pack_index`] tail just the new bytes appended by a
    /// sibling instance/process since last time, instead of re-reading and
    /// re-parsing the whole (ever-growing) index file on every call. Without
    /// this, packing N new chunks costs O(N²): every one of the N `put()`
    /// calls would re-parse all chunks packed before it.
    pack_index_pos: Mutex<u64>,
    /// The currently-open pack file being appended to. A `Mutex` (not
    /// `RwLock`): every write needs `current_size` read and advanced as one
    /// atomic step, so there's never a reader-only case worth optimizing for.
    pack_state: Mutex<PackState>,
}

impl Store {
    /// Open (creating if needed) a store rooted at `dir`.
    pub fn open(dir: impl AsRef<Path>) -> Result<Store, StoreError> {
        let dir = dir.as_ref();
        let objects = dir.join("objects");
        fs::create_dir_all(&objects)?;
        let refs_path = dir.join("refs.tsv");
        let refs = load_refs(&refs_path)?;

        let packs_dir = dir.join("packs");
        fs::create_dir_all(&packs_dir)?;
        let pack_index_path = packs_dir.join("index");
        let (pack_index, pack_index_pos) = load_pack_index(&pack_index_path)?;
        let pack_state = Mutex::new(PackState::open(&packs_dir)?);

        Ok(Store {
            objects,
            refs_path,
            refs: RwLock::new(refs),
            packs_dir,
            pack_index_path,
            pack_index: RwLock::new(pack_index),
            pack_index_pos: Mutex::new(pack_index_pos),
            pack_state,
        })
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

    /// True if this exact content is already stored (packed, copied in under
    /// the old per-chunk-file layout, or referenced).
    pub fn has(&self, h: &Hash) -> bool {
        if self.pack_index.read().unwrap().contains_key(h)
            || self.path_for(h).exists()
            || self.refs.read().unwrap().contains_key(h)
        {
            return true;
        }
        // Miss so far — another `Store` instance/process pointed at this same
        // directory (a very normal pattern here: e.g. `Network::spawn` owns
        // one handle, a caller opens a second) may have packed this chunk
        // after we last loaded our in-memory index. Refresh once and re-check
        // before concluding it's really absent.
        let _ = self.refresh_pack_index();
        self.pack_index.read().unwrap().contains_key(h)
    }

    /// Pick up entries a sibling `Store` instance/process appended to
    /// `packs/index` since we last checked — tailing only the *new* bytes
    /// (a `stat` plus, only if the size grew, a seek+read of just the
    /// tail), never re-reading the whole file. That distinction matters:
    /// this runs on every `put()`/`has()` miss, so re-parsing the entire
    /// index each time would make packing N new chunks cost O(N²).
    fn refresh_pack_index(&self) -> Result<(), StoreError> {
        let mut pos = self.pack_index_pos.lock().unwrap();
        let len = match fs::metadata(&self.pack_index_path) {
            Ok(m) => m.len(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        if len <= *pos {
            return Ok(()); // nothing new (the common case — just the one stat)
        }
        let mut f = File::open(&self.pack_index_path)?;
        f.seek(SeekFrom::Start(*pos))?;
        let mut tail = String::new();
        f.read_to_string(&mut tail)?;
        // Only fully-written lines are safe to parse — a concurrent
        // append's write() may not have landed in full yet. Whatever's past
        // the last newline is picked up on a later refresh once complete.
        let Some(last_nl) = tail.rfind('\n') else { return Ok(()) };
        let complete = &tail[..=last_nl];
        let mut index = self.pack_index.write().unwrap();
        for line in complete.lines() {
            if let Some((h, loc)) = parse_pack_index_line(line) {
                index.insert(h, loc);
            }
        }
        drop(index);
        *pos += complete.len() as u64;
        Ok(())
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
    /// callers can measure dedup. A no-op if the content is already present
    /// (checked against the packfile index first, then the old per-chunk-file
    /// layout — but new writes always go to the packfile).
    pub fn put(&self, bytes: &[u8]) -> Result<(Hash, bool), StoreError> {
        let h = Hash::of(bytes);
        if self.has(&h) {
            return Ok((h, false)); // dedup hit
        }
        let mut state = self.pack_state.lock().unwrap();
        // Re-check while holding the pack lock — it's the single
        // serialization point for "is this new" for concurrent writers in
        // *this* process; refresh first in case a sibling `Store`
        // instance/process wrote this exact hash between our check above and
        // taking the lock. Held across the write AND the index update below
        // (not dropped until then) — otherwise a second thread could pass
        // this same re-check before the first thread's write is indexed.
        let _ = self.refresh_pack_index();
        if self.pack_index.read().unwrap().contains_key(&h) || self.path_for(&h).exists() {
            return Ok((h, false));
        }
        let (pack_id, offset) = state.write(&self.packs_dir, bytes)?;
        let length = bytes.len() as u32;
        let written = append_pack_index_line(&self.pack_index_path, &h, pack_id, offset, length)?;
        self.pack_index.write().unwrap().insert(h, (pack_id, offset, length));
        // Advance our own read cursor past the line we just wrote, so a
        // later refresh (e.g. the very next put()) never re-reads it —
        // refresh_pack_index only exists to pick up *other* instances' writes.
        *self.pack_index_pos.lock().unwrap() += written;
        drop(state);
        Ok((h, true))
    }

    /// Fetch content by hash, verifying it still matches on the way out.
    /// Checks the packfile index, then the old per-chunk-file layout, then
    /// the `--no-copy` reference index.
    pub fn get(&self, h: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        if let Some(bytes) = self.get_from_pack_index(h)? {
            return Ok(Some(bytes));
        }
        match fs::read(self.path_for(h)) {
            Ok(bytes) => {
                if Hash::of(&bytes) != *h {
                    return Err(StoreError::Corrupt(*h));
                }
                return Ok(Some(bytes));
            }
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            Err(_) => {}
        }
        if let Some(bytes) = self.get_reference(h)? {
            return Ok(Some(bytes));
        }
        // Still nothing local — refresh in case a sibling `Store`
        // instance/process packed this chunk after we last loaded our
        // in-memory index (see `has`), then try the pack index one more time.
        self.refresh_pack_index()?;
        self.get_from_pack_index(h)
    }

    fn get_from_pack_index(&self, h: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let Some((pack_id, offset, length)) = self.pack_index.read().unwrap().get(h).copied() else {
            return Ok(None);
        };
        let bytes = self.read_from_pack(pack_id, offset, length)?;
        if Hash::of(&bytes) != *h {
            return Err(StoreError::Corrupt(*h));
        }
        Ok(Some(bytes))
    }

    fn read_from_pack(&self, pack_id: u32, offset: u64, length: u32) -> Result<Vec<u8>, StoreError> {
        let mut file = File::open(self.packs_dir.join(format!("{pack_id}.pack")))?;
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0u8; length as usize];
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    /// Number of distinct objects currently stored: packed chunks plus
    /// whatever's left in the old per-chunk-file layout (walks its fan-out
    /// dirs). Never counts `--no-copy` references.
    pub fn object_count(&self) -> Result<usize, StoreError> {
        self.refresh_pack_index()?;
        let mut count = self.pack_index.read().unwrap().len();
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

/// Parse one `packs/index` line (`<hash-hex>\t<pack_id>\t<offset>\t<length>`).
/// `None` for a malformed or truncated line (e.g. a crash mid-append) —
/// tolerated the same way [`load_refs`] tolerates one, by skipping it.
fn parse_pack_index_line(line: &str) -> Option<(Hash, PackLoc)> {
    let mut fields = line.splitn(4, '\t');
    let (Some(hash), Some(pack_id), Some(offset), Some(length)) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return None;
    };
    let (Ok(hash), Ok(pack_id), Ok(offset), Ok(length)) =
        (Hash::from_hex(hash), pack_id.parse::<u32>(), offset.parse::<u64>(), length.parse::<u32>())
    else {
        return None;
    };
    Some((hash, (pack_id, offset, length)))
}

/// Load the whole of `packs/index`, if it exists, returning the parsed
/// entries plus the file's byte length at read time (the starting point for
/// [`Store::refresh_pack_index`]'s incremental tailing).
fn load_pack_index(path: &Path) -> Result<(HashMap<Hash, PackLoc>, u64), StoreError> {
    let mut index = HashMap::new();
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((index, 0)),
        Err(e) => return Err(e.into()),
    };
    for line in text.lines() {
        if let Some((h, loc)) = parse_pack_index_line(line) {
            index.insert(h, loc);
        }
    }
    Ok((index, text.len() as u64))
}

/// Append one line to `packs/index` for a chunk just written to a pack file.
/// Returns the number of bytes written, so the caller can advance its own
/// read cursor past it (see [`Store::refresh_pack_index`]).
fn append_pack_index_line(path: &Path, h: &Hash, pack_id: u32, offset: u64, length: u32) -> io::Result<u64> {
    let mut line = String::new();
    line.push_str(&h.to_hex());
    line.push('\t');
    line.push_str(&pack_id.to_string());
    line.push('\t');
    line.push_str(&offset.to_string());
    line.push('\t');
    line.push_str(&length.to_string());
    line.push('\n');
    let mut f = fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())?;
    Ok(line.len() as u64)
}

/// The pack file currently being appended to, plus enough state to rotate to
/// a new one once it grows past [`PACK_ROTATE_SIZE`].
struct PackState {
    current_id: u32,
    current_file: File,
    current_size: u64,
}

impl PackState {
    /// Resume the highest-numbered `<id>.pack` found in `packs_dir` (0 if
    /// none exist yet) — rotation itself is checked lazily on the next
    /// [`PackState::write`], not here, so a store that was already past the
    /// threshold when it last closed just rotates on its first new write.
    fn open(packs_dir: &Path) -> io::Result<PackState> {
        let mut max_id: Option<u32> = None;
        for entry in fs::read_dir(packs_dir)? {
            let entry = entry?;
            if let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(".pack"))
                .and_then(|stem| stem.parse::<u32>().ok())
            {
                max_id = Some(max_id.map_or(id, |m| m.max(id)));
            }
        }
        let current_id = max_id.unwrap_or(0);
        let path = packs_dir.join(format!("{current_id}.pack"));
        let current_file = fs::OpenOptions::new().create(true).append(true).open(&path)?;
        let current_size = fs::metadata(&path)?.len();
        Ok(PackState { current_id, current_file, current_size })
    }

    /// Append `bytes` to the current pack (rotating first if it's already at
    /// capacity), returning where they landed.
    fn write(&mut self, packs_dir: &Path, bytes: &[u8]) -> io::Result<(u32, u64)> {
        if self.current_size >= PACK_ROTATE_SIZE {
            self.current_id += 1;
            let path = packs_dir.join(format!("{}.pack", self.current_id));
            self.current_file = fs::OpenOptions::new().create(true).append(true).open(&path)?;
            self.current_size = 0;
        }
        let offset = self.current_size;
        self.current_file.write_all(bytes)?;
        self.current_size += bytes.len() as u64;
        Ok((self.current_id, offset))
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

    /// Write a chunk directly into the *old* per-chunk-file layout, bypassing
    /// `put()` entirely — simulates a store that existed before packfiles did.
    fn write_legacy_object(root: &Path, bytes: &[u8]) -> Hash {
        let h = Hash::of(bytes);
        let hex = h.to_hex();
        let dir = root.join("objects").join(&hex[..2]);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(&hex), bytes).unwrap();
        h
    }

    #[test]
    fn reads_a_chunk_from_the_old_per_chunk_file_layout() {
        let dir = TmpDir::new();
        // Object written before the store is ever `open()`ed by this code —
        // exactly what upgrading in place looks like.
        let bytes = sample(50_000, 70);
        let h = write_legacy_object(dir.path(), &bytes);

        let store = Store::open(dir.path()).unwrap();
        assert!(store.has(&h), "must recognize a pre-existing legacy object");
        assert_eq!(store.get(&h).unwrap().unwrap(), bytes);
    }

    #[test]
    fn put_on_a_legacy_object_dedups_instead_of_double_writing_to_the_pack() {
        let dir = TmpDir::new();
        let bytes = sample(50_000, 71);
        let h = write_legacy_object(dir.path(), &bytes);

        let store = Store::open(dir.path()).unwrap();
        let (h2, is_new) = store.put(&bytes).unwrap();
        assert_eq!(h2, h);
        assert!(!is_new, "content already present under the legacy layout must dedup, not re-copy into a pack");

        // Only the one legacy object exists — nothing new got written to
        // packs/index (object_count would double-count if put() didn't
        // check the legacy layout before writing).
        assert_eq!(store.object_count().unwrap(), 1);
        assert!(!dir.path().join("packs").join("index").exists());
    }

    #[test]
    fn new_writes_after_upgrade_go_to_a_pack_not_the_legacy_layout() {
        let dir = TmpDir::new();
        // Establish the store with the legacy layout already present, then
        // open it (the "upgrade" moment) and write something brand new.
        let old = sample(50_000, 72);
        write_legacy_object(dir.path(), &old);
        let store = Store::open(dir.path()).unwrap();

        let new_bytes = sample(50_000, 73);
        let (h, is_new) = store.put(&new_bytes).unwrap();
        assert!(is_new);
        assert_eq!(store.get(&h).unwrap().unwrap(), new_bytes);

        // The new chunk landed in a pack file, not a new legacy object path.
        assert!(dir.path().join("packs").join("0.pack").exists());
        assert!(dir.path().join("packs").join("index").exists());
        let hex = h.to_hex();
        assert!(!dir.path().join("objects").join(&hex[..2]).join(&hex).exists());

        // Both old (legacy) and new (packed) chunks are visible side by side.
        assert_eq!(store.object_count().unwrap(), 2);
    }

    #[test]
    fn pack_index_survives_a_reopened_store() {
        let dir = TmpDir::new();
        let data = sample(300_000, 74);
        let m = {
            let store = Store::open(dir.path()).unwrap();
            store.ingest(&data, None).unwrap()
        };
        // Fresh `Store` handle, as a later `serve` process would open — the
        // packfile index must be reloaded from disk, not just in-memory state.
        let reopened = Store::open(dir.path()).unwrap();
        assert_eq!(reopened.export(&m).unwrap(), data);
        assert_eq!(reopened.object_count().unwrap(), m.chunks.len());
    }

    #[test]
    fn pack_rotates_to_a_new_file_past_the_size_threshold() {
        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();
        // Bytes bigger than the rotation threshold in one `put()` — forces an
        // immediate rotation on the *next* write, without needing to actually
        // accumulate hundreds of MB of real chunks in this test.
        let big = vec![7u8; (PACK_ROTATE_SIZE + 1) as usize];
        store.put(&big).unwrap();
        assert!(dir.path().join("packs").join("0.pack").exists());

        let small = sample(1_000, 75);
        let (h, _) = store.put(&small).unwrap();
        assert!(dir.path().join("packs").join("1.pack").exists(), "should have rotated to a new pack file");
        assert_eq!(store.get(&h).unwrap().unwrap(), small);

        // Reopening must resume from the highest-numbered pack, not restart
        // at 0.pack (which would silently start overwriting old data).
        let small2 = sample(1_000, 76);
        let reopened = Store::open(dir.path()).unwrap();
        let (h2, _) = reopened.put(&small2).unwrap();
        assert_eq!(reopened.get(&h2).unwrap().unwrap(), small2);
        assert!(!dir.path().join("packs").join("2.pack").exists(), "should keep appending to 1.pack, not roll again");
    }

    #[test]
    fn concurrent_put_of_identical_bytes_writes_exactly_once() {
        use std::sync::Arc;
        use std::thread;

        let dir = TmpDir::new();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let bytes = Arc::new(sample(100_000, 77));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                let bytes = bytes.clone();
                thread::spawn(move || store.put(&bytes).unwrap())
            })
            .collect();
        let results: Vec<(Hash, bool)> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        assert_eq!(results.iter().filter(|(_, is_new)| *is_new).count(), 1, "exactly one thread should win the write");
        assert_eq!(store.object_count().unwrap(), 1);
        assert_eq!(store.get(&results[0].0).unwrap().unwrap(), *bytes);
    }

    /// Two independent `Store` handles opened on the *same* directory is a
    /// completely normal pattern in this codebase (e.g. `Network::spawn`
    /// takes ownership of one, a caller opens a second right after). Each
    /// keeps its own in-memory pack index, so a naive implementation would
    /// have one handle unable to see chunks the other just packed — this
    /// pinned a real regression caught via `np2ptp-bridge`'s
    /// `second_node_resolves_torrent_from_np2ptp_without_converting` test,
    /// where a node's own request-handling `Store` handle couldn't serve a
    /// chunk a *different* handle (same directory) had just packed.
    #[test]
    fn a_second_store_instance_sees_chunks_the_first_one_packed() {
        let dir = TmpDir::new();
        let writer = Store::open(dir.path()).unwrap();
        let reader = Store::open(dir.path()).unwrap(); // opened before the write below

        let bytes = sample(60_000, 78);
        let (h, is_new) = writer.put(&bytes).unwrap();
        assert!(is_new);

        assert!(reader.has(&h), "a sibling instance must see a chunk packed after it was opened");
        assert_eq!(reader.get(&h).unwrap().unwrap(), bytes);
        assert_eq!(reader.object_count().unwrap(), 1);

        // And it must dedup against the writer's chunk, not silently pack a
        // second copy under its own stale view.
        let (h2, is_new2) = reader.put(&bytes).unwrap();
        assert_eq!(h2, h);
        assert!(!is_new2, "the second instance must recognize this chunk as already packed");
        assert_eq!(reader.object_count().unwrap(), 1);
    }
}
