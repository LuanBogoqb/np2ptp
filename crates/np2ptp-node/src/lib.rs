//! `np2ptp-node` — the `.nptp` linker and the downloading client.
//!
//! Two halves of the user-facing flow:
//!
//! * **Linker** ([`pack`]): chunk a file into a content-addressed store and hand
//!   back a [`Manifest`]; the caller serializes it to a `.nptp` file. This is the
//!   NP2PTP equivalent of "create a torrent".
//! * **Client** ([`download`]): given a `.nptp` manifest, pull every chunk it
//!   names from a [`ChunkSource`], verifying each one against the Merkle root
//!   before accepting it, then reconstruct the file.
//!
//! [`ChunkSource`] is the seam between "works today" and "works on a real
//! network". Right now the only source is [`StoreSource`] — another node's
//! on-disk store, i.e. a local stand-in for a seed. When the `np2ptp-net` layer
//! lands, a libp2p-backed source implements the same trait and [`download`]
//! needs no changes.

use std::fs;
use std::path::{Path, PathBuf};

use np2ptp_core::{Hash, Manifest, ManifestError};
use np2ptp_store::{Store, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("source does not have chunk {0}")]
    MissingChunk(Hash),
    #[error("chunk {index} from source failed verification against the manifest root")]
    BadChunk { index: usize },
    #[error("refusing unsafe path in manifest: {0:?}")]
    UnsafePath(String),
}

/// Anywhere chunks can be fetched from by hash.
///
/// Today: a peer's on-disk store ([`StoreSource`]). Tomorrow: a libp2p swarm.
/// [`download`] is written against this trait so it never needs to know which.
pub trait ChunkSource {
    fn fetch(&self, hash: &Hash) -> Result<Option<Vec<u8>>, NodeError>;
}

/// A chunk source backed by another node's content-addressed store (a "seed").
pub struct StoreSource {
    store: Store,
}

impl StoreSource {
    pub fn open(dir: impl AsRef<Path>) -> Result<StoreSource, NodeError> {
        Ok(StoreSource { store: Store::open(dir)? })
    }
}

impl ChunkSource for StoreSource {
    fn fetch(&self, hash: &Hash) -> Result<Option<Vec<u8>>, NodeError> {
        Ok(self.store.get(hash)?)
    }
}

/// The **linker**: chunk `data` into `store` (deduping) and return its manifest.
/// The caller persists `manifest.to_nptp()` as the shareable `.nptp` file.
pub fn pack(data: &[u8], name: Option<String>, store: &Store) -> Result<Manifest, NodeError> {
    Ok(store.ingest(data, name)?)
}

/// What a download actually moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadReport {
    /// Chunks pulled from the source this run.
    pub fetched: usize,
    /// Chunks already present locally and skipped (cross-download dedup).
    pub deduped: usize,
}

/// The **client**: fetch every chunk named by `manifest` from `source` into
/// `local`, verifying each chunk against the Merkle root before storing it.
///
/// Chunks already in `local` are skipped, so re-downloading content that shares
/// chunks with something you already have is nearly free. A chunk that fails
/// verification aborts the download with [`NodeError::BadChunk`] — a lying peer
/// is caught immediately, before its bytes can corrupt the output.
pub fn download<S: ChunkSource>(
    manifest: &Manifest,
    source: &S,
    local: &Store,
) -> Result<DownloadReport, NodeError> {
    let mut fetched = 0;
    let mut deduped = 0;
    for (i, cref) in manifest.chunks.iter().enumerate() {
        if local.has(&cref.hash) {
            deduped += 1;
            continue;
        }
        let bytes = source
            .fetch(&cref.hash)?
            .ok_or(NodeError::MissingChunk(cref.hash))?;
        if !manifest.verify_chunk(i, &bytes) {
            return Err(NodeError::BadChunk { index: i });
        }
        local.put(&bytes)?;
        fetched += 1;
    }
    Ok(DownloadReport { fetched, deduped })
}

/// Recursively read a directory into ordered `(relative_path, bytes)` pairs.
///
/// Paths are '/'-separated and the list is sorted by path, so the same tree
/// produces the same manifest (and content id) on any OS, regardless of the
/// order the filesystem hands back directory entries.
pub fn read_dir_tree(root: &Path) -> Result<Vec<(String, Vec<u8>)>, NodeError> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<(), NodeError> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                walk(base, &path, out)?;
            } else if file_type.is_file() {
                let rel = path.strip_prefix(base).unwrap_or(&path);
                let rel_str = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push((rel_str, fs::read(&path)?));
            }
        }
        Ok(())
    }

    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Write reconstructed `(relative_path, bytes)` files under `out_dir`, creating
/// parent directories as needed. Rejects unsafe paths (absolute, `.` or `..`
/// components) so a malicious manifest can't escape the target directory.
pub fn write_tree(out_dir: &Path, files: &[(String, Vec<u8>)]) -> Result<(), NodeError> {
    for (rel, bytes) in files {
        let mut dest = PathBuf::from(out_dir);
        for comp in rel.split('/') {
            if comp.is_empty() || comp == "." || comp == ".." {
                return Err(NodeError::UnsafePath(rel.clone()));
            }
            dest.push(comp);
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, bytes)?;
    }
    Ok(())
}
