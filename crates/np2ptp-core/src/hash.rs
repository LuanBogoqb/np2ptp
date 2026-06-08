//! BLAKE3 content hashing and a domain-separated binary Merkle tree.
//!
//! Every piece of content in NP2PTP is named by its BLAKE3 hash. A set of chunk
//! hashes is committed to with a Merkle tree whose root is the content id; a peer
//! can then prove that a single chunk belongs to that root with a compact
//! inclusion proof, so a lying peer is caught the moment a bad chunk arrives
//! instead of after the whole transfer.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A 32-byte BLAKE3 digest. Used both for chunk content ids and Merkle nodes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    /// Hash arbitrary bytes (a chunk's content).
    pub fn of(data: &[u8]) -> Hash {
        Hash(blake3::hash(data).into())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Parse a 64-char lowercase/uppercase hex string into a hash.
    pub fn from_hex(s: &str) -> Result<Hash, hex::FromHexError> {
        let mut out = [0u8; 32];
        hex::decode_to_slice(s, &mut out)?;
        Ok(Hash(out))
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Short prefix keeps logs readable.
        write!(f, "Hash({}…)", &self.to_hex()[..12])
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

// Domain-separation tags keep leaf, internal-node, and empty-tree hashes in
// distinct namespaces, preventing second-preimage tricks across levels.
const LEAF_TAG: u8 = 0x00;
const NODE_TAG: u8 = 0x01;
const EMPTY_TAG: u8 = 0x02;

fn leaf_hash(chunk: &Hash) -> Hash {
    let mut h = blake3::Hasher::new();
    h.update(&[LEAF_TAG]);
    h.update(&chunk.0);
    Hash(h.finalize().into())
}

fn node_hash(left: &Hash, right: &Hash) -> Hash {
    let mut h = blake3::Hasher::new();
    h.update(&[NODE_TAG]);
    h.update(&left.0);
    h.update(&right.0);
    Hash(h.finalize().into())
}

/// One step of an inclusion proof: a sibling digest plus which side it sits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofStep {
    /// True if the sibling is the left child (so the running hash is on the right).
    pub sibling_on_left: bool,
    pub hash: Hash,
}

/// A compact proof that a chunk hash is the `index`-th leaf under some root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MerkleProof {
    pub index: usize,
    pub steps: Vec<ProofStep>,
}

/// Build every level of the Merkle tree, bottom (leaves) to top (root).
///
/// Odd nodes at a level are carried up unchanged rather than duplicated; this
/// avoids the duplicate-leaf forgery that bites naive Merkle implementations.
fn build_levels(chunk_hashes: &[Hash]) -> Vec<Vec<Hash>> {
    let leaves: Vec<Hash> = chunk_hashes.iter().map(leaf_hash).collect();
    let mut levels = vec![leaves];
    while levels.last().map_or(0, |l| l.len()) > 1 {
        let cur = levels.last().unwrap();
        let mut next = Vec::with_capacity(cur.len().div_ceil(2));
        let mut i = 0;
        while i < cur.len() {
            if i + 1 < cur.len() {
                next.push(node_hash(&cur[i], &cur[i + 1]));
                i += 2;
            } else {
                next.push(cur[i]); // carry the odd one up untouched
                i += 1;
            }
        }
        levels.push(next);
    }
    levels
}

/// Compute the Merkle root over an ordered list of chunk hashes.
pub fn merkle_root(chunk_hashes: &[Hash]) -> Hash {
    if chunk_hashes.is_empty() {
        return Hash(blake3::hash(&[EMPTY_TAG]).into());
    }
    let levels = build_levels(chunk_hashes);
    levels.last().unwrap()[0]
}

/// Produce an inclusion proof for the chunk at `index`.
pub fn merkle_proof(chunk_hashes: &[Hash], index: usize) -> Option<MerkleProof> {
    if index >= chunk_hashes.len() {
        return None;
    }
    let levels = build_levels(chunk_hashes);
    let mut steps = Vec::new();
    let mut idx = index;
    for level in &levels[..levels.len() - 1] {
        let sib = idx ^ 1;
        if sib < level.len() {
            steps.push(ProofStep {
                sibling_on_left: sib < idx,
                hash: level[sib],
            });
        }
        // No sibling => this node was carried up; nothing to add at this level.
        idx /= 2;
    }
    Some(MerkleProof { index, steps })
}

/// Verify that `chunk_hash` is committed to by `root` via `proof`.
pub fn merkle_verify(chunk_hash: &Hash, proof: &MerkleProof, root: &Hash) -> bool {
    let mut acc = leaf_hash(chunk_hash);
    for step in &proof.steps {
        acc = if step.sibling_on_left {
            node_hash(&step.hash, &acc)
        } else {
            node_hash(&acc, &step.hash)
        };
    }
    &acc == root
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(n: usize) -> Vec<Hash> {
        (0..n).map(|i| Hash::of(format!("chunk-{i}").as_bytes())).collect()
    }

    #[test]
    fn hex_round_trip() {
        let h = Hash::of(b"hello");
        assert_eq!(Hash::from_hex(&h.to_hex()).unwrap(), h);
    }

    #[test]
    fn single_leaf_root_is_its_leaf_hash() {
        let l = leaves(1);
        let proof = merkle_proof(&l, 0).unwrap();
        assert!(proof.steps.is_empty());
        assert!(merkle_verify(&l[0], &proof, &merkle_root(&l)));
    }

    #[test]
    fn every_index_proves_for_various_sizes() {
        for n in [2usize, 3, 4, 5, 7, 8, 16, 31, 100] {
            let l = leaves(n);
            let root = merkle_root(&l);
            for i in 0..n {
                let proof = merkle_proof(&l, i).unwrap();
                assert!(merkle_verify(&l[i], &proof, &root), "n={n} i={i}");
            }
        }
    }

    #[test]
    fn wrong_leaf_fails_verification() {
        let l = leaves(8);
        let root = merkle_root(&l);
        let proof = merkle_proof(&l, 3).unwrap();
        let forged = Hash::of(b"not the real chunk");
        assert!(!merkle_verify(&forged, &proof, &root));
    }

    #[test]
    fn out_of_range_proof_is_none() {
        assert!(merkle_proof(&leaves(4), 4).is_none());
    }

    #[test]
    fn empty_and_nonempty_roots_differ() {
        assert_ne!(merkle_root(&[]), merkle_root(&leaves(1)));
    }
}
