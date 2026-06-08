//! `np2ptp-fec` — forward error correction via RaptorQ fountain codes.
//!
//! Classic BitTorrent stores a file as N fixed pieces and you need *every* one.
//! That gives two chronic problems: the "rare piece" (one piece only a departing
//! seeder had) and the endgame stall on the last block. With a fountain code you
//! instead generate as many interchangeable encoding symbols as you like, and
//! **any** sufficiently large subset rebuilds the original. A swarm can keep
//! producing fresh repair symbols, so content survives heavy seeder churn — which
//! is exactly the "permanence" goal of NP2PTP.
//!
//! This crate is a thin, transport-agnostic wrapper over the `raptorq` codec:
//! [`encode`] turns bytes into self-describing symbols, [`decode`] rebuilds the
//! bytes from any large-enough collection of them.

use raptorq::{Decoder, Encoder, EncodingPacket, ObjectTransmissionInformation};

/// Compute the RaptorQ transmission config from the source length and symbol
/// size alone — no need to ship it. A decoder that knows the content's total
/// size (from the manifest) and the symbol size can reconstruct the same config.
pub fn config_for(source_len: u64, symbol_size: u16) -> [u8; 12] {
    ObjectTransmissionInformation::with_defaults(source_len, symbol_size).serialize()
}

/// Default symbol payload size (bytes). Small enough to fit a typical network
/// datagram once framing is added later.
pub const DEFAULT_SYMBOL_SIZE: u16 = 1200;

/// An erasure-coded object: the codec parameters plus a bag of symbols.
///
/// `config` and `source_len` are the only metadata a peer needs (alongside the
/// symbols) to reconstruct; ship them with the manifest. Symbols themselves are
/// self-identifying, so they can be fetched from many peers in any order.
#[derive(Clone, Debug)]
pub struct Encoded {
    /// RaptorQ transmission info (12 bytes), needed to decode.
    pub config: [u8; 12],
    /// Length of the original data, so decoding trims any codec padding.
    pub source_len: u64,
    /// Serialized encoding symbols (source + repair), each independently usable.
    pub symbols: Vec<Vec<u8>>,
}

impl Encoded {
    pub fn symbol_count(&self) -> usize {
        self.symbols.len()
    }
}

/// Encode `data` into source + `repair_symbols` repair symbols at the default
/// symbol size. More repair symbols = more redundancy = more churn tolerance.
pub fn encode(data: &[u8], repair_symbols: u32) -> Encoded {
    encode_with_symbol_size(data, DEFAULT_SYMBOL_SIZE, repair_symbols)
}

/// Like [`encode`] but with an explicit symbol size.
pub fn encode_with_symbol_size(data: &[u8], symbol_size: u16, repair_symbols: u32) -> Encoded {
    if data.is_empty() {
        // RaptorQ is undefined for empty input; represent it explicitly.
        return Encoded { config: [0u8; 12], source_len: 0, symbols: Vec::new() };
    }
    let encoder = Encoder::with_defaults(data, symbol_size);
    let symbols = encoder
        .get_encoded_packets(repair_symbols)
        .iter()
        .map(|p| p.serialize())
        .collect();
    Encoded {
        config: encoder.get_config().serialize(),
        source_len: data.len() as u64,
        symbols,
    }
}

/// Rebuild the original bytes from any large-enough set of symbols.
///
/// Returns `Some(data)` as soon as enough symbols have been supplied, or `None`
/// if the whole iterator is exhausted without reaching the decoding threshold.
pub fn decode<I>(config: &[u8; 12], source_len: u64, symbols: I) -> Option<Vec<u8>>
where
    I: IntoIterator<Item = Vec<u8>>,
{
    if source_len == 0 {
        return Some(Vec::new());
    }
    let mut decoder = Decoder::new(ObjectTransmissionInformation::deserialize(config));
    for bytes in symbols {
        if let Some(mut data) = decoder.decode(EncodingPacket::deserialize(&bytes)) {
            data.truncate(source_len as usize); // strip any codec padding
            return Some(data);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(n: usize, seed: u64) -> Vec<u8> {
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

    #[test]
    fn recovers_after_heavy_symbol_loss() {
        let data = sample(100_000, 1);
        let enc = encode(&data, 60); // generous redundancy
        // Drop the first 40 symbols (simulating peers/seeders that vanished).
        let kept: Vec<Vec<u8>> = enc.symbols.iter().skip(40).cloned().collect();
        assert!(kept.len() < enc.symbol_count());
        let out = decode(&enc.config, enc.source_len, kept).expect("should still decode");
        assert_eq!(out, data);
    }

    #[test]
    fn decoding_order_does_not_matter() {
        let data = sample(80_000, 2);
        let enc = encode(&data, 40);
        // Feed repair symbols first, then source symbols: still reconstructs.
        let mut reordered: Vec<Vec<u8>> = enc.symbols.iter().rev().cloned().collect();
        reordered.truncate(enc.symbol_count() - 10); // also drop a few
        let out = decode(&enc.config, enc.source_len, reordered).expect("decodes regardless of order");
        assert_eq!(out, data);
    }

    #[test]
    fn too_few_symbols_cannot_decode() {
        let data = sample(100_000, 3);
        let enc = encode(&data, 10);
        let too_few: Vec<Vec<u8>> = enc.symbols.iter().take(5).cloned().collect();
        assert!(decode(&enc.config, enc.source_len, too_few).is_none());
    }

    #[test]
    fn empty_round_trips() {
        let enc = encode(b"", 5);
        assert_eq!(enc.symbol_count(), 0);
        assert_eq!(decode(&enc.config, enc.source_len, enc.symbols).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn config_for_matches_encoder_config() {
        let data = sample(123_456, 9);
        let enc = encode(&data, 10);
        assert_eq!(config_for(data.len() as u64, DEFAULT_SYMBOL_SIZE), enc.config);
        // And a decoder built from the derived config can reconstruct.
        let out = decode(&config_for(data.len() as u64, DEFAULT_SYMBOL_SIZE), data.len() as u64, enc.symbols).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn small_payload_round_trips() {
        let data = b"hello np2ptp".to_vec();
        let enc = encode(&data, 5);
        let out = decode(&enc.config, enc.source_len, enc.symbols).unwrap();
        assert_eq!(out, data);
    }
}
