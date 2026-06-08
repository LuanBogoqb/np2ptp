//! `np2ptp-core` — content addressing for NP2PTP.
//!
//! This crate is transport-agnostic: it turns bytes into content-defined,
//! BLAKE3-hashed chunks committed to by a Merkle root ([`manifest`]), and lets
//! peers verify individual chunks against that root ([`hash`]). Everything that
//! talks to the network or disk is built on top of these types.

pub mod chunk;
pub mod hash;
pub mod manifest;

pub use chunk::{chunk, ChunkSpan};
pub use hash::{merkle_proof, merkle_root, merkle_verify, Hash, MerkleProof, ProofStep};
pub use manifest::{
    ChunkRef, FileEntry, Manifest, ManifestError, NPTP_MAGIC, NPTP_VERSION, URI_SCHEME,
};
