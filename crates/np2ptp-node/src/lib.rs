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

pub mod serve_set;
pub mod daemon;

pub use serve_set::{collect_serve_manifests, register_manifest};

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    InvalidUsage(String),
    #[error("source does not have chunk {0}")]
    MissingChunk(Hash),
    #[error("chunk {index} from source failed verification against the manifest root")]
    BadChunk { index: usize },
    #[error("refusing unsafe path in manifest: {0:?}")]
    UnsafePath(String),
}

/// Reduce a peer-supplied display name to something safe to use as a local
/// output path. `manifest.name` is NOT part of the content id — any provider
/// can set it to an arbitrary path — so the CLI only ever writes to its final
/// path component, and only if that component passes
/// [`np2ptp_core::validate_relative_path`]. Returns `None` when nothing safe
/// remains, and callers fall back to a fixed default.
pub fn sanitize_output_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Final component across both separators; kills directory escapes and
    // drive/UNC prefixes in one step.
    let last = trimmed.rsplit(['/', '\\']).next()?;
    match np2ptp_core::validate_relative_path(last) {
        Ok(()) => Some(last.to_string()),
        Err(_) => None,
    }
}

#[cfg(test)]
mod name_tests {
    use super::{sanitize_output_name, write_tree};
    use std::path::Path;

    #[test]
    fn keeps_plain_names_and_drops_escapes() {
        assert_eq!(sanitize_output_name("my-folder"), Some("my-folder".into()));
        assert_eq!(sanitize_output_name("file.txt"), Some("file.txt".into()));
        assert_eq!(sanitize_output_name("dir/inner"), Some("inner".into()));
        assert_eq!(sanitize_output_name("a\\b\\c.txt"), Some("c.txt".into()));
        assert_eq!(sanitize_output_name("  spaced  "), Some("spaced".into()));
        // Hostile names: leading traversal and drive prefixes die by
        // construction — only the final component survives. Some cases still
        // yield a safe fragment (write "evil", not "C:\Users\evil"); None
        // means nothing safe remains and the caller falls back.
        assert_eq!(sanitize_output_name(".."), None);
        assert_eq!(sanitize_output_name("../escape"), Some("escape".into()));
        assert_eq!(sanitize_output_name("C:\\Users\\evil"), Some("evil".into()));
        assert_eq!(sanitize_output_name("C:evil"), None);
        assert_eq!(sanitize_output_name("CON"), None);
        assert_eq!(sanitize_output_name("file.exe:ads"), None);
        assert_eq!(sanitize_output_name("   "), None);
        assert_eq!(sanitize_output_name(""), None);
    }

    #[test]
    fn write_tree_rejects_traversal_paths_without_writing_outside() {
        let dir = std::env::temp_dir().join(format!("np2ptp-writetree-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let outside = dir.parent().unwrap().join("evil.txt");
        let _ = std::fs::remove_file(&outside); // clean slate for the assertion

        let files = vec![("a\\..\\..\\evil.txt".to_string(), b"x".to_vec())];
        assert!(write_tree(&dir, &files).is_err());
        assert!(!outside.exists(), "backslash traversal must not escape the output dir");

        let files = vec![("C:\\Windows\\evil.txt".to_string(), b"x".to_vec())];
        assert!(write_tree(Path::new(&dir), &files).is_err());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&outside);
    }
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
    download_with_progress(manifest, source, local, |_, _| {})
}

/// Like [`download`], but calls `on_progress(chunks_done, chunks_total)` once
/// per chunk (fetched or deduped) as it's accounted for.
pub fn download_with_progress<S: ChunkSource>(
    manifest: &Manifest,
    source: &S,
    local: &Store,
    mut on_progress: impl FnMut(usize, usize),
) -> Result<DownloadReport, NodeError> {
    // Validate the chunk list against the Merkle root once; then a cheap
    // per-chunk content-hash check is enough (and stays O(n) at scale).
    if !manifest.root_is_consistent() {
        return Err(NodeError::BadChunk { index: 0 });
    }
    let total = manifest.chunks.len();
    let mut fetched = 0;
    let mut deduped = 0;
    for (i, cref) in manifest.chunks.iter().enumerate() {
        if local.has(&cref.hash) {
            deduped += 1;
        } else {
            let bytes = source
                .fetch(&cref.hash)?
                .ok_or(NodeError::MissingChunk(cref.hash))?;
            if !manifest.chunk_hash_ok(i, &bytes) {
                return Err(NodeError::BadChunk { index: i });
            }
            local.put(&bytes)?;
            fetched += 1;
        }
        on_progress(i + 1, total);
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
            } else {
                // Symlinks (and anything else non-regular) used to vanish
                // silently: the manifest came out self-consistent and the
                // gap only surfaced when a fetcher failed verification.
                eprintln!(
                    "warning: skipping {} — not a regular file (symlink or special)",
                    path.display()
                );
            }
        }
        Ok(())
    }

    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Recursively list a directory as ordered `(relative_path, disk_path)` pairs,
/// without reading file contents — for streaming ingestion of large trees.
pub fn read_dir_paths(root: &Path) -> Result<Vec<(String, PathBuf)>, NodeError> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<(), NodeError> {
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
                out.push((rel_str, path.clone()));
            } else {
                // Same silent-skip problem as read_dir_tree: say it out loud.
                eprintln!(
                    "warning: skipping {} — not a regular file (symlink or special)",
                    path.display()
                );
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
/// components, Windows backslash/drive/ADS escapes) so a malicious manifest
/// can't escape the target directory — same rules as the store's exporter,
/// shared via `np2ptp_core::validate_relative_path`.
pub fn write_tree(out_dir: &Path, files: &[(String, Vec<u8>)]) -> Result<(), NodeError> {
    for (rel, bytes) in files {
        if np2ptp_core::validate_relative_path(rel).is_err() {
            return Err(NodeError::UnsafePath(rel.clone()));
        }
        let mut dest = PathBuf::from(out_dir);
        for comp in rel.split('/') {
            dest.push(comp);
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, bytes)?;
    }
    Ok(())
}
