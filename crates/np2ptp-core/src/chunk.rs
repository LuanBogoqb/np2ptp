//! Content-defined chunking (FastCDC).
//!
//! Fixed-size pieces (classic BitTorrent) shift every boundary when a byte is
//! inserted near the front of a file, so an edited or re-versioned file shares
//! almost nothing with the original. Content-defined boundaries depend on a
//! rolling hash of the surrounding bytes, so unchanged regions keep producing
//! identical chunks. That is what makes cross-content dedup in `np2ptp-store`
//! actually pay off.

use fastcdc::v2020::FastCDC;

use crate::hash::Hash;

/// Chunk-size targets. Boundaries land near `AVG`, clamped to `[MIN, MAX]`.
pub const MIN_CHUNK: u32 = 16 * 1024;
pub const AVG_CHUNK: u32 = 64 * 1024;
pub const MAX_CHUNK: u32 = 256 * 1024;

/// A chunk's position in the source plus its content hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkSpan {
    pub offset: u64,
    pub length: u32,
    pub hash: Hash,
}

/// Split `data` into content-defined chunks and hash each one.
pub fn chunk(data: &[u8]) -> Vec<ChunkSpan> {
    chunk_with(data, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK)
}

/// Same as [`chunk`] but with explicit size targets (used by tests/benchmarks).
pub fn chunk_with(data: &[u8], min: u32, avg: u32, max: u32) -> Vec<ChunkSpan> {
    if data.is_empty() {
        return Vec::new();
    }
    FastCDC::new(data, min, avg, max)
        .map(|c| {
            let bytes = &data[c.offset..c.offset + c.length];
            ChunkSpan {
                offset: c.offset as u64,
                length: c.length as u32,
                hash: Hash::of(bytes),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_no_chunks() {
        assert!(chunk(b"").is_empty());
    }

    #[test]
    fn chunks_cover_input_contiguously() {
        let data: Vec<u8> = (0..500_000u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
        let spans = chunk(&data);
        assert!(spans.len() > 1, "large input should split into many chunks");
        let mut cursor = 0u64;
        for s in &spans {
            assert_eq!(s.offset, cursor);
            cursor += s.length as u64;
        }
        assert_eq!(cursor, data.len() as u64);
    }

    #[test]
    fn insertion_near_front_preserves_most_chunks() {
        // Pseudo-random but deterministic data so boundaries are realistic.
        let base: Vec<u8> = (0..400_000u32).map(|i| (i.wrapping_mul(40503) >> 8) as u8).collect();

        let mut edited = base.clone();
        edited.splice(10..10, *b"INSERTED"); // 8 bytes inserted near the start

        let a: std::collections::HashSet<_> = chunk(&base).into_iter().map(|c| c.hash).collect();
        let b: std::collections::HashSet<_> = chunk(&edited).into_iter().map(|c| c.hash).collect();

        let shared = a.intersection(&b).count();
        // Fixed-size chunking would share ~0 here; CDC should keep the tail.
        assert!(shared * 2 >= a.len(), "expected majority of chunks reused, shared={shared} of {}", a.len());
    }
}
