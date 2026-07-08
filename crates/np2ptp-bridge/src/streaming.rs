//! Streaming counterpart to [`crate::convert`]/[`crate::resolve_or_convert`]:
//! reads already-downloaded torrent content straight from disk, verifying
//! piece hashes and ingesting in bounded-size windows, so a real (tens-of-GB)
//! torrent is never held in memory. Used by `LocalTorrentSource` (a `.torrent`
//! you already have the data for) — the separate in-memory `TorrentSource`
//! path stays available for a future network-fetching source.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use np2ptp_core::Manifest;
use np2ptp_net::Network;
use np2ptp_store::Store;
use sha1::{Digest, Sha1};

use crate::{publish, resolve, BridgeError, Outcome, TorrentMeta};

/// Verify `files` (given as `(relative_path, disk_path)` pairs, in torrent
/// order) against a torrent's v1 piece hashes by reading each file in
/// bounded-size windows — never holding more than one piece's worth of bytes
/// in memory. A piece may span a file boundary; this handles that.
pub fn verify_pieces_streaming(
    files: &[(String, PathBuf)],
    piece_length: usize,
    piece_hashes: &[[u8; 20]],
) -> Result<(), BridgeError> {
    if piece_length == 0 {
        return Err(BridgeError::PieceVerificationFailed);
    }
    let mut verifier = StreamingPieceVerifier::new(piece_length, piece_hashes);
    let mut buf = vec![0u8; 64 * 1024];
    for (name, path) in files {
        let mut f = File::open(path).map_err(|e| BridgeError::Source(format!("{name}: {e}")))?;
        loop {
            let n = f.read(&mut buf).map_err(|e| BridgeError::Source(format!("{name}: {e}")))?;
            if n == 0 {
                break;
            }
            verifier.feed(&buf[..n])?;
        }
    }
    verifier.finish()
}

struct StreamingPieceVerifier<'a> {
    piece_length: usize,
    piece_hashes: &'a [[u8; 20]],
    buf: Vec<u8>,
    next_piece: usize,
}

impl<'a> StreamingPieceVerifier<'a> {
    fn new(piece_length: usize, piece_hashes: &'a [[u8; 20]]) -> Self {
        StreamingPieceVerifier { piece_length, piece_hashes, buf: Vec::new(), next_piece: 0 }
    }

    fn feed(&mut self, mut data: &[u8]) -> Result<(), BridgeError> {
        while !data.is_empty() {
            let need = self.piece_length - self.buf.len();
            let take = need.min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buf.len() == self.piece_length {
                self.hash_and_check()?;
            }
        }
        Ok(())
    }

    fn hash_and_check(&mut self) -> Result<(), BridgeError> {
        let expected = self.piece_hashes.get(self.next_piece).ok_or(BridgeError::PieceVerificationFailed)?;
        let got: [u8; 20] = Sha1::digest(&self.buf).into();
        if &got != expected {
            return Err(BridgeError::PieceVerificationFailed);
        }
        self.buf.clear();
        self.next_piece += 1;
        Ok(())
    }

    fn finish(mut self) -> Result<(), BridgeError> {
        if !self.buf.is_empty() {
            self.hash_and_check()?;
        }
        if self.next_piece != self.piece_hashes.len() {
            return Err(BridgeError::PieceVerificationFailed);
        }
        Ok(())
    }
}

/// Convert an already-downloaded torrent into NP2PTP content: verify it
/// against the torrent's own piece hashes (streamed from disk), then ingest
/// it (also streamed — never a whole file in memory). `data_dir` must
/// contain `meta`'s file tree directly (`data_dir.join(&file.path)` for every
/// file — the same relationship `pack` already has to a directory input:
/// paths don't include the tree's own top-level name).
pub fn convert_local(
    store: &Store,
    meta: &TorrentMeta,
    data_dir: &Path,
    no_copy: bool,
) -> Result<Manifest, BridgeError> {
    let files: Vec<(String, PathBuf)> =
        meta.files.iter().map(|f| (f.path.clone(), data_dir.join(&f.path))).collect();
    verify_pieces_streaming(&files, meta.piece_length as usize, &meta.piece_hashes)?;
    let manifest = if no_copy {
        store.ingest_tree_files_no_copy(&files, Some(meta.name.clone()))?
    } else {
        store.ingest_tree_files(&files, Some(meta.name.clone()))?
    };
    Ok(manifest)
}

/// The streaming counterpart to [`crate::resolve_or_convert`]: serve `meta`
/// from the NP2PTP network if some other peer already bridged it, otherwise
/// convert it from the already-downloaded files under `data_dir` and bridge
/// it.
pub async fn resolve_or_convert_local(
    net: &Network,
    store: &Store,
    meta: &TorrentMeta,
    data_dir: &Path,
    no_copy: bool,
) -> Result<Outcome, BridgeError> {
    if let Some(manifest) = resolve(net, store, &meta.infohash, Some(meta)).await? {
        return Ok(Outcome { manifest, infohash: meta.infohash.clone(), converted: false });
    }
    let manifest = convert_local(store, meta, data_dir, no_copy)?;
    publish(net, &manifest, &meta.infohash).await?;
    Ok(Outcome { manifest, infohash: meta.infohash.clone(), converted: true })
}
