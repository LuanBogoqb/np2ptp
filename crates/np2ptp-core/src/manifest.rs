//! The manifest: NP2PTP's answer to the `.torrent` file / magnet link.
//!
//! A manifest is the ordered list of chunks that make up a piece of content,
//! plus the Merkle root that commits to them. The root *is* the content id, so
//! a link is just `np2ptp:<hex-root>`. Anyone holding the manifest can fetch
//! chunks by hash from any peer and verify each one against the root.

use serde::{Deserialize, Serialize};

use crate::chunk::{self, ChunkSpan};
use crate::hash::{merkle_proof, merkle_root, merkle_verify, Hash, MerkleProof};

/// URI scheme for NP2PTP content links.
pub const URI_SCHEME: &str = "np2ptp";

/// Magic header identifying a `.nptp` container file.
pub const NPTP_MAGIC: [u8; 4] = *b"NPTP";
/// Current `.nptp` on-disk format version.
pub const NPTP_VERSION: u8 = 1;

/// One chunk's place in the reconstructed byte stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRef {
    pub hash: Hash,
    pub offset: u64,
    pub length: u32,
}

/// One file within a manifest, mapping a relative path to a contiguous range of
/// the global chunk list. Each file is chunked independently (a chunk boundary is
/// forced at every file edge), so an identical file appearing in two different
/// directory trees shares all of its chunks regardless of layout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Relative path, always '/'-separated for cross-platform stability.
    pub path: String,
    pub size: u64,
    /// Index of this file's first chunk in [`Manifest::chunks`].
    pub chunk_start: usize,
    /// How many chunks belong to this file.
    pub chunk_count: usize,
}

/// The full content descriptor shared between peers — a single file *or* a whole
/// directory tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Merkle root over the chunk hashes (in `chunks` order) — the content id.
    pub root: Hash,
    pub total_size: u64,
    /// Every chunk across every file, in file order. The Merkle root commits to
    /// this list, so chunk verification is identical for single files and trees.
    pub chunks: Vec<ChunkRef>,
    /// The files this manifest describes (length 1 for a single file).
    pub files: Vec<FileEntry>,
    /// Optional human-facing name (top-level file or directory). Not part of the id.
    pub name: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("serialization failed: {0}")]
    Codec(#[from] bincode::Error),
    #[error("bad np2ptp uri")]
    BadUri,
    #[error("invalid hex in uri: {0}")]
    Hex(#[from] hex::FromHexError),
    #[error("chunk {index} failed hash check")]
    BadChunk { index: usize },
    #[error("reconstructed size {got} != manifest size {expected}")]
    SizeMismatch { got: u64, expected: u64 },
    #[error("not an nptp file (bad magic header)")]
    BadMagic,
    #[error("unsupported nptp file version {0} (this build supports {NPTP_VERSION})")]
    UnsupportedVersion(u8),
    #[error("manifest structure invalid: {0}")]
    Invalid(&'static str),
}

impl Manifest {
    /// Build a single-file manifest by chunking `data` in memory.
    pub fn from_bytes(data: &[u8], name: Option<String>) -> Manifest {
        let path = name.clone().unwrap_or_else(|| "data".to_string());
        Manifest::from_files(std::iter::once((path, data)), name)
    }

    /// Build a manifest from an ordered set of `(relative_path, bytes)` files —
    /// a single file or a whole directory tree. Each file is chunked
    /// independently and their chunk lists are concatenated; the Merkle root is
    /// taken over the combined list.
    pub fn from_files<'a, I>(files: I, name: Option<String>) -> Manifest
    where
        I: IntoIterator<Item = (String, &'a [u8])>,
    {
        let mut chunks: Vec<ChunkRef> = Vec::new();
        let mut entries: Vec<FileEntry> = Vec::new();
        let mut global_offset: u64 = 0;
        for (path, data) in files {
            let chunk_start = chunks.len();
            for span in chunk::chunk(data) {
                chunks.push(ChunkRef {
                    hash: span.hash,
                    offset: global_offset + span.offset,
                    length: span.length,
                });
            }
            entries.push(FileEntry {
                path,
                size: data.len() as u64,
                chunk_start,
                chunk_count: chunks.len() - chunk_start,
            });
            global_offset += data.len() as u64;
        }
        let hashes: Vec<Hash> = chunks.iter().map(|c| c.hash).collect();
        Manifest {
            root: merkle_root(&hashes),
            total_size: global_offset,
            chunks,
            files: entries,
            name,
        }
    }

    /// Build a single-file manifest from pre-computed chunk spans.
    pub fn from_spans(spans: &[ChunkSpan], total_size: u64, name: Option<String>) -> Manifest {
        let path = name.clone().unwrap_or_else(|| "data".to_string());
        let chunks: Vec<ChunkRef> = spans
            .iter()
            .map(|s| ChunkRef {
                hash: s.hash,
                offset: s.offset,
                length: s.length,
            })
            .collect();
        let hashes: Vec<Hash> = chunks.iter().map(|c| c.hash).collect();
        let files = vec![FileEntry {
            path,
            size: total_size,
            chunk_start: 0,
            chunk_count: chunks.len(),
        }];
        Manifest {
            root: merkle_root(&hashes),
            total_size,
            chunks,
            files,
            name,
        }
    }

    /// True if this manifest describes exactly one file.
    pub fn is_single_file(&self) -> bool {
        self.files.len() == 1
    }

    /// The shareable link, e.g. `np2ptp:1a2b...`.
    pub fn uri(&self) -> String {
        format!("{}:{}", URI_SCHEME, self.root.to_hex())
    }

    /// Parse the root hash out of an `np2ptp:<hex>` link.
    pub fn root_from_uri(uri: &str) -> Result<Hash, ManifestError> {
        let rest = uri.strip_prefix(URI_SCHEME).and_then(|r| r.strip_prefix(':'));
        let hex = rest.ok_or(ManifestError::BadUri)?;
        Ok(Hash::from_hex(hex)?)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, ManifestError> {
        Ok(bincode::serialize(self)?)
    }

    pub fn from_serialized(bytes: &[u8]) -> Result<Manifest, ManifestError> {
        Ok(bincode::deserialize(bytes)?)
    }

    /// Encode as a `.nptp` container: `NPTP` magic + version byte + manifest.
    /// This is the file a "linker" writes and a client reads to drive a download.
    pub fn to_nptp(&self) -> Result<Vec<u8>, ManifestError> {
        let body = self.to_bytes()?;
        let mut out = Vec::with_capacity(5 + body.len());
        out.extend_from_slice(&NPTP_MAGIC);
        out.push(NPTP_VERSION);
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Parse a `.nptp` container, validating the magic header and version.
    pub fn from_nptp(bytes: &[u8]) -> Result<Manifest, ManifestError> {
        if bytes.len() < 5 || bytes[..4] != NPTP_MAGIC {
            return Err(ManifestError::BadMagic);
        }
        if bytes[4] != NPTP_VERSION {
            return Err(ManifestError::UnsupportedVersion(bytes[4]));
        }
        Manifest::from_serialized(&bytes[5..])
    }

    /// Recompute the root from the listed chunks and confirm it matches.
    /// Guards against a tampered manifest whose `root` doesn't match its chunks.
    pub fn root_is_consistent(&self) -> bool {
        let hashes: Vec<Hash> = self.chunks.iter().map(|c| c.hash).collect();
        merkle_root(&hashes) == self.root
    }

    /// Check the structural invariants the Merkle root does NOT commit to: the
    /// chunk lengths must sum to `total_size`, and each file's chunk range and
    /// byte size must line up with the chunk list. A peer-supplied manifest can
    /// be self-consistent (root over its own chunk list) and still carry ranges
    /// that index out of bounds or slice past the stream, so every
    /// network-entry reconstruction path runs this before touching `files`.
    ///
    /// Returns the same `ManifestError`s the reconstruction fns would have
    /// tripped over mid-flight (or panicked with, before this existed).
    pub fn validate(&self) -> Result<(), ManifestError> {
        let mut chunk_bytes: u64 = 0;
        for cref in &self.chunks {
            chunk_bytes = chunk_bytes.saturating_add(cref.length as u64);
        }
        if chunk_bytes != self.total_size {
            return Err(ManifestError::SizeMismatch {
                got: chunk_bytes,
                expected: self.total_size,
            });
        }
        let mut cursor = 0usize;
        let mut file_bytes: u64 = 0;
        for entry in &self.files {
            if entry.chunk_start != cursor {
                return Err(ManifestError::Invalid("file chunk ranges must tile the chunk list"));
            }
            // An empty file legitimately owns zero chunks; a non-empty one can't.
            if entry.chunk_count == 0 && entry.size != 0 {
                return Err(ManifestError::Invalid("non-empty file owns zero chunks"));
            }
            let end = entry
                .chunk_start
                .checked_add(entry.chunk_count)
                .ok_or(ManifestError::Invalid("file chunk range overflows usize"))?;
            if end > self.chunks.len() {
                return Err(ManifestError::Invalid("file chunk range out of bounds"));
            }
            // A file's own chunks must back its listed size — the global sums
            // alone would let bytes silently land in the wrong file on export.
            let own_bytes = self.chunks[entry.chunk_start..end]
                .iter()
                .map(|c| c.length as u64)
                .fold(0u64, |acc, l| acc.saturating_add(l));
            if own_bytes != entry.size {
                return Err(ManifestError::SizeMismatch {
                    got: own_bytes,
                    expected: entry.size,
                });
            }
            cursor = end;
            file_bytes = file_bytes.saturating_add(entry.size);
        }
        if cursor != self.chunks.len() {
            return Err(ManifestError::Invalid("file chunk ranges must tile the chunk list"));
        }
        if file_bytes != self.total_size {
            return Err(ManifestError::SizeMismatch {
                got: file_bytes,
                expected: self.total_size,
            });
        }
        Ok(())
    }

    /// Inclusion proof for the chunk at `index` against this manifest's root.
    pub fn proof(&self, index: usize) -> Option<MerkleProof> {
        let hashes: Vec<Hash> = self.chunks.iter().map(|c| c.hash).collect();
        merkle_proof(&hashes, index)
    }

    /// Cheap per-chunk check: the bytes hash to the listed chunk hash.
    ///
    /// This does NOT prove inclusion under the Merkle root on its own — use it
    /// only after the manifest's chunk list has been validated against the root
    /// once via [`root_is_consistent`](Self::root_is_consistent). That pairing is
    /// O(n) total, versus [`verify_chunk`](Self::verify_chunk)'s O(n) per call
    /// (which rebuilds the whole tree each time and is unusable at scale).
    pub fn chunk_hash_ok(&self, index: usize, bytes: &[u8]) -> bool {
        self.chunks.get(index).is_some_and(|c| Hash::of(bytes) == c.hash)
    }

    /// Verify a chunk's bytes belong here: content hash matches the ref *and*
    /// the ref is committed to by the root.
    pub fn verify_chunk(&self, index: usize, bytes: &[u8]) -> bool {
        let Some(cref) = self.chunks.get(index) else {
            return false;
        };
        if Hash::of(bytes) != cref.hash {
            return false;
        }
        match self.proof(index) {
            Some(p) => merkle_verify(&cref.hash, &p, &self.root),
            None => false,
        }
    }

    /// Reassemble the whole content, pulling each chunk's bytes from `fetch`
    /// and verifying every one before accepting it.
    pub fn reconstruct<F>(&self, mut fetch: F) -> Result<Vec<u8>, ManifestError>
    where
        F: FnMut(&Hash) -> Option<Vec<u8>>,
    {
        // Bounds every later allocation: a hostile manifest can't claim a
        // total_size its chunk list doesn't back.
        self.validate()?;
        // No `with_capacity(total_size)`: the sum is tied to *declared* chunk
        // lengths, which a hostile manifest inflates per-chunk. Grow instead —
        // extends are amortized O(n) over data that actually arrived.
        let mut out = Vec::new();
        for (i, cref) in self.chunks.iter().enumerate() {
            let bytes = fetch(&cref.hash).ok_or(ManifestError::BadChunk { index: i })?;
            if !self.verify_chunk(i, &bytes) {
                return Err(ManifestError::BadChunk { index: i });
            }
            out.extend_from_slice(&bytes);
        }
        if out.len() as u64 != self.total_size {
            return Err(ManifestError::SizeMismatch {
                got: out.len() as u64,
                expected: self.total_size,
            });
        }
        Ok(out)
    }

    /// Split a fully-reconstructed byte stream back into `(path, bytes)` files,
    /// using the file sizes recorded in the manifest. Used by the FEC download
    /// path, which recovers the whole stream at once rather than chunk-by-chunk.
    pub fn split_stream(&self, data: &[u8]) -> Result<Vec<(String, Vec<u8>)>, ManifestError> {
        if data.len() as u64 != self.total_size {
            return Err(ManifestError::SizeMismatch {
                got: data.len() as u64,
                expected: self.total_size,
            });
        }
        // File sizes aren't committed by the root — check before slicing so a
        // hostile manifest gets an error, not a panic.
        self.validate()?;
        let mut out = Vec::with_capacity(self.files.len());
        let mut offset = 0usize;
        for entry in &self.files {
            let end = offset
                .checked_add(entry.size as usize)
                .filter(|&end| end <= data.len())
                .ok_or(ManifestError::Invalid("file sizes exceed the content stream"))?;
            out.push((entry.path.clone(), data[offset..end].to_vec()));
            offset = end;
        }
        Ok(out)
    }

    /// Reassemble each file separately, returning `(relative_path, bytes)` pairs.
    /// Every chunk is verified against the root before being accepted, so a bad
    /// source is caught per chunk just like in [`reconstruct`].
    pub fn reconstruct_files<F>(&self, mut fetch: F) -> Result<Vec<(String, Vec<u8>)>, ManifestError>
    where
        F: FnMut(&Hash) -> Option<Vec<u8>>,
    {
        // File chunk ranges aren't committed by the root — bound them first.
        self.validate()?;
        let mut out = Vec::with_capacity(self.files.len());
        for entry in &self.files {
            // Grow incrementally: entry.size is attacker-declared and must not
            // command a pre-allocation.
            let mut file_bytes = Vec::new();
            for ci in entry.chunk_start..entry.chunk_start + entry.chunk_count {
                let Some(cref) = self.chunks.get(ci) else {
                    return Err(ManifestError::Invalid("file chunk range out of bounds"));
                };
                let bytes = fetch(&cref.hash).ok_or(ManifestError::BadChunk { index: ci })?;
                if !self.verify_chunk(ci, &bytes) {
                    return Err(ManifestError::BadChunk { index: ci });
                }
                file_bytes.extend_from_slice(&bytes);
            }
            if file_bytes.len() as u64 != entry.size {
                return Err(ManifestError::SizeMismatch {
                    got: file_bytes.len() as u64,
                    expected: entry.size,
                });
            }
            out.push((entry.path.clone(), file_bytes));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn sample(n: usize) -> Vec<u8> {
        (0..n as u32).map(|i| (i.wrapping_mul(2246822519) >> 16) as u8).collect()
    }

    fn store_of(data: &[u8], m: &Manifest) -> HashMap<Hash, Vec<u8>> {
        m.chunks
            .iter()
            .map(|c| {
                let bytes = data[c.offset as usize..(c.offset + c.length as u64) as usize].to_vec();
                (c.hash, bytes)
            })
            .collect()
    }

    #[test]
    fn manifest_round_trips_through_reconstruct() {
        let data = sample(300_000);
        let m = Manifest::from_bytes(&data, Some("blob.bin".into()));
        assert!(m.root_is_consistent());
        let store = store_of(&data, &m);
        let back = m.reconstruct(|h| store.get(h).cloned()).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn uri_round_trips() {
        let m = Manifest::from_bytes(&sample(100_000), None);
        assert_eq!(Manifest::root_from_uri(&m.uri()).unwrap(), m.root);
    }

    #[test]
    fn serialized_manifest_round_trips() {
        let m = Manifest::from_bytes(&sample(120_000), Some("x".into()));
        let bytes = m.to_bytes().unwrap();
        assert_eq!(Manifest::from_serialized(&bytes).unwrap(), m);
    }

    #[test]
    fn tampered_chunk_is_rejected() {
        let data = sample(200_000);
        let m = Manifest::from_bytes(&data, None);
        let mut store = store_of(&data, &m);
        // Corrupt the bytes of the first chunk.
        let first = m.chunks[0].hash;
        store.insert(first, vec![0xFF; m.chunks[0].length as usize]);
        let err = m.reconstruct(|h| store.get(h).cloned()).unwrap_err();
        assert!(matches!(err, ManifestError::BadChunk { index: 0 }));
    }

    #[test]
    fn nptp_container_round_trips() {
        let m = Manifest::from_bytes(&sample(150_000), Some("movie.mkv".into()));
        let file = m.to_nptp().unwrap();
        assert_eq!(&file[..4], b"NPTP");
        assert_eq!(Manifest::from_nptp(&file).unwrap(), m);
    }

    #[test]
    fn nptp_rejects_bad_magic() {
        let mut file = Manifest::from_bytes(&sample(50_000), None).to_nptp().unwrap();
        file[0] = b'X';
        assert!(matches!(Manifest::from_nptp(&file), Err(ManifestError::BadMagic)));
    }

    #[test]
    fn nptp_rejects_future_version() {
        let mut file = Manifest::from_bytes(&sample(50_000), None).to_nptp().unwrap();
        file[4] = 99;
        assert!(matches!(Manifest::from_nptp(&file), Err(ManifestError::UnsupportedVersion(99))));
    }

    fn hentropy(n: usize, seed: u64) -> Vec<u8> {
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

    fn tree_store(files: &[(String, &[u8])], m: &Manifest) -> HashMap<Hash, Vec<u8>> {
        let mut store = HashMap::new();
        for entry in &m.files {
            let data = files.iter().find(|(p, _)| *p == entry.path).unwrap().1;
            let mut local = 0usize;
            for ci in entry.chunk_start..entry.chunk_start + entry.chunk_count {
                let len = m.chunks[ci].length as usize;
                store.insert(m.chunks[ci].hash, data[local..local + len].to_vec());
                local += len;
            }
        }
        store
    }

    #[test]
    fn from_bytes_is_single_file() {
        let m = Manifest::from_bytes(&sample(50_000), Some("a.bin".into()));
        assert!(m.is_single_file());
        assert_eq!(m.files.len(), 1);
        assert_eq!(m.files[0].path, "a.bin");
        assert_eq!(m.files[0].size, m.total_size);
    }

    #[test]
    fn tree_manifest_round_trips_per_file() {
        let a = hentropy(200_000, 1);
        let b = hentropy(150_000, 2);
        let c = b"a small text file".to_vec();
        let files: Vec<(String, &[u8])> = vec![
            ("dir/a.bin".into(), a.as_slice()),
            ("dir/sub/b.bin".into(), b.as_slice()),
            ("c.txt".into(), c.as_slice()),
        ];
        let m = Manifest::from_files(files.clone(), Some("dir".into()));
        assert!(!m.is_single_file());
        assert_eq!(m.files.len(), 3);
        assert_eq!(m.total_size, (a.len() + b.len() + c.len()) as u64);
        assert!(m.root_is_consistent());

        let store = tree_store(&files, &m);
        let got = m.reconstruct_files(|h| store.get(h).cloned()).unwrap();
        let want: Vec<(String, Vec<u8>)> =
            files.iter().map(|(p, d)| (p.clone(), d.to_vec())).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn identical_file_in_two_trees_shares_chunks() {
        // Same file content, different paths and different neighbors.
        let shared = hentropy(300_000, 7);
        let unique1 = hentropy(100_000, 11);
        let unique2 = hentropy(120_000, 13);

        let t1 = Manifest::from_files(
            vec![
                ("shared.bin".into(), shared.as_slice()),
                ("only1.bin".into(), unique1.as_slice()),
            ],
            None,
        );
        let t2 = Manifest::from_files(
            vec![
                ("x/y/shared.bin".into(), shared.as_slice()),
                ("only2.bin".into(), unique2.as_slice()),
            ],
            None,
        );

        let hashes_of = |m: &Manifest, path: &str| -> Vec<Hash> {
            let e = m.files.iter().find(|f| f.path == path).unwrap();
            m.chunks[e.chunk_start..e.chunk_start + e.chunk_count]
                .iter()
                .map(|c| c.hash)
                .collect()
        };

        let h1 = hashes_of(&t1, "shared.bin");
        let h2 = hashes_of(&t2, "x/y/shared.bin");
        assert!(h1.len() > 1, "shared file should span several chunks");
        // Perfect per-file dedup regardless of path or surrounding files.
        assert_eq!(h1, h2);
    }

    #[test]
    fn empty_content_has_consistent_root() {
        let m = Manifest::from_bytes(b"", None);
        assert!(m.root_is_consistent());
        assert_eq!(m.total_size, 0);
        let back = m.reconstruct(|_| None).unwrap();
        assert!(back.is_empty());
    }

    // --- Adversarial manifests: self-consistent roots, hostile structure. ---
    // The Merkle root commits only to the chunk list; everything below checks
    // that the *uncommitted* fields (total_size, file sizes, chunk ranges)
    // produce clean errors instead of panics or huge allocations.

    /// A valid manifest, for tests to mutate into hostile shapes.
    fn hostile_base() -> Manifest {
        Manifest::from_files(
            [("dir/a.bin".into(), sample(3000).as_slice()), ("b.bin".into(), sample(2000).as_slice())],
            Some("hostile".into()),
        )
    }

    #[test]
    fn validate_accepts_what_from_files_builds() {
        let m = Manifest::from_files(
            [("one.bin".into(), sample(1500).as_slice()), ("empty.bin".into(), b"".as_slice())],
            None,
        );
        m.validate().expect("from_files output must satisfy invariants");
    }

    #[test]
    fn validate_rejects_total_size_that_chunks_do_not_back() {
        let mut m = hostile_base();
        m.total_size += 1 << 40; // claim far more than the chunks sum to
        assert!(matches!(m.validate(), Err(ManifestError::SizeMismatch { .. })));
    }

    #[test]
    fn validate_rejects_file_range_out_of_bounds() {
        let mut m = hostile_base();
        m.files[0].chunk_start = 4_000_000_000usize;
        m.files[0].chunk_count = 1;
        assert!(matches!(m.validate(), Err(ManifestError::Invalid(_))));
    }

    #[test]
    fn validate_rejects_overlapping_file_ranges() {
        let mut m = hostile_base();
        m.files[0].chunk_count += 1; // runs into file 1's chunks
        assert!(m.validate().is_err());
    }

    #[test]
    fn validate_rejects_file_sizes_beyond_total() {
        let mut m = hostile_base();
        m.files[0].size += m.files[1].size + 1;
        assert!(matches!(m.validate(), Err(ManifestError::SizeMismatch { .. })));
    }

    #[test]
    fn split_stream_errors_instead_of_panicking_on_hostile_sizes() {
        // Pre-fix this sliced data[offset..offset+size] with size > total and
        // panicked with "range end index out of range".
        let mut m = hostile_base();
        m.files[0].size = m.total_size + 1;
        let data = vec![0u8; m.total_size as usize];
        assert!(m.split_stream(&data).is_err());
    }

    #[test]
    fn reconstruct_errors_on_hostile_ranges_not_panics() {
        let m = hostile_base();
        let mut by_hash = std::collections::HashMap::new();
        for c in &m.chunks {
            by_hash.insert(c.hash, vec![0u8; c.length as usize]);
        }

        let mut bad_range = m.clone();
        bad_range.files[0].chunk_start = 999_999;
        assert!(bad_range
            .reconstruct_files(|h| by_hash.get(h).cloned())
            .is_err());

        // total_size the chunks can't back: reconstruct must not pre-allocate
        // a terabyte just because the manifest says so.
        let mut big_claim = m.clone();
        big_claim.total_size = 1 << 40;
        assert!(big_claim.reconstruct(|h| by_hash.get(h).cloned()).is_err());
    }
}
