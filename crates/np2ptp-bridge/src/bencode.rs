//! Minimal bencode decoder — only what's needed to read a `.torrent` metainfo
//! file (integers, byte strings, lists, dicts). Not a general-purpose bencode
//! library: no encoder for real use, no support for turning a `Value` back
//! into bytes (bencode re-encoding is not guaranteed byte-identical to the
//! original, and the infohash needs the original bytes exactly).

use crate::{BridgeError, TorrentFile, TorrentMeta};
use sha1::{Digest, Sha1};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(Vec<(Vec<u8>, Value)>),
}

impl Value {
    fn as_dict(&self) -> Option<&[(Vec<u8>, Value)]> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }
    fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }
    fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }
    fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }
    fn dict_get(&self, key: &[u8]) -> Option<&Value> {
        self.as_dict()?.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

fn err(msg: &str) -> BridgeError {
    BridgeError::Source(format!("bencode: {msg}"))
}

const MAX_DEPTH: usize = 32;

/// Decode one bencode value from the front of `input`, returning it and the
/// unconsumed remainder.
fn decode(input: &[u8], depth: usize) -> Result<(Value, &[u8]), BridgeError> {
    if depth > MAX_DEPTH {
        return Err(err("bencode nesting too deep"));
    }
    match input.first() {
        Some(b'i') => decode_int(input),
        Some(b'l') => decode_list(input, depth),
        Some(b'd') => decode_dict(input, depth),
        Some(b'0'..=b'9') => {
            let (b, rest) = decode_byte_string(input)?;
            Ok((Value::Bytes(b), rest))
        }
        _ => Err(err("unexpected byte at start of value")),
    }
}

fn decode_int(input: &[u8]) -> Result<(Value, &[u8]), BridgeError> {
    let rest = input.strip_prefix(b"i").ok_or_else(|| err("expected 'i'"))?;
    let end = rest.iter().position(|&b| b == b'e').ok_or_else(|| err("unterminated integer"))?;
    let s = std::str::from_utf8(&rest[..end]).map_err(|_| err("integer is not utf-8"))?;
    let n: i64 = s.parse().map_err(|_| err("invalid integer"))?;
    Ok((Value::Int(n), &rest[end + 1..]))
}

/// Decode a bencode byte string (`<len>:<bytes>`), returning the raw bytes
/// (not wrapped in `Value`) since this is also used to read dict keys.
fn decode_byte_string(input: &[u8]) -> Result<(Vec<u8>, &[u8]), BridgeError> {
    let colon = input.iter().position(|&b| b == b':').ok_or_else(|| err("missing ':' in byte string"))?;
    let len_s = std::str::from_utf8(&input[..colon]).map_err(|_| err("byte string length is not utf-8"))?;
    let len: usize = len_s.parse().map_err(|_| err("invalid byte string length"))?;
    let start = colon + 1;
    let end = start.checked_add(len).ok_or_else(|| err("byte string length overflow"))?;
    let bytes = input.get(start..end).ok_or_else(|| err("byte string runs past end of input"))?;
    Ok((bytes.to_vec(), &input[end..]))
}

fn decode_list(input: &[u8], depth: usize) -> Result<(Value, &[u8]), BridgeError> {
    let mut rest = input.strip_prefix(b"l").ok_or_else(|| err("expected 'l'"))?;
    let mut items = Vec::new();
    loop {
        match rest.first() {
            Some(b'e') => return Ok((Value::List(items), &rest[1..])),
            None => return Err(err("unterminated list")),
            _ => {
                let (v, r) = decode(rest, depth + 1)?;
                items.push(v);
                rest = r;
            }
        }
    }
}

fn decode_dict(input: &[u8], depth: usize) -> Result<(Value, &[u8]), BridgeError> {
    let mut rest = input.strip_prefix(b"d").ok_or_else(|| err("expected 'd'"))?;
    let mut entries = Vec::new();
    loop {
        match rest.first() {
            Some(b'e') => return Ok((Value::Dict(entries), &rest[1..])),
            None => return Err(err("unterminated dict")),
            _ => {
                let (key, r) = decode_byte_string(rest)?;
                let (v, r2) = decode(r, depth + 1)?;
                entries.push((key, v));
                rest = r2;
            }
        }
    }
}

/// Find the byte offset (within `bytes`) where the top-level dict's `"info"`
/// value starts. Re-walks the dict manually (rather than reusing the already-
/// parsed `Value` tree) because the parsed tree doesn't retain original byte
/// offsets, and the infohash needs the *original* bytes.
fn find_info_offset(bytes: &[u8]) -> Result<usize, BridgeError> {
    let mut rest = bytes.strip_prefix(b"d").ok_or_else(|| err("expected top-level dict"))?;
    loop {
        match rest.first() {
            Some(b'e') => return Err(err("top-level dict has no 'info' key")),
            None => return Err(err("unterminated dict")),
            _ => {
                let (key, r) = decode_byte_string(rest)?;
                let value_offset = bytes.len() - r.len();
                if key == b"info" {
                    return Ok(value_offset);
                }
                let (_, r2) = decode(r, 0)?;
                rest = r2;
            }
        }
    }
}

/// Reject a parsed file path that is absolute or escapes `--data <dir>` via
/// `..` — a malicious `.torrent` must not be able to write outside it.
fn validate_relative_path(path: &str) -> Result<(), BridgeError> {
    if path.is_empty() {
        return Err(err("file path must not be empty"));
    }
    let p = std::path::Path::new(path);
    for comp in p.components() {
        match comp {
            std::path::Component::Normal(_) => {}
            _ => return Err(err("file path must not be absolute or contain '..'")),
        }
    }
    Ok(())
}

/// Parse a `.torrent` file's bytes into [`TorrentMeta`]. The infohash is the
/// SHA-1 of the raw, original bytes of the top-level `info` dict — never a
/// reserialization (bencode re-encoding is not guaranteed byte-identical).
pub fn parse_torrent_file(bytes: &[u8]) -> Result<TorrentMeta, BridgeError> {
    let info_offset = find_info_offset(bytes)?;
    let info_slice = &bytes[info_offset..];
    let (info_value, info_rest) = decode(info_slice, 0)?;
    let consumed = info_slice.len() - info_rest.len();
    let infohash = Sha1::digest(&info_slice[..consumed]).to_vec();

    let name = info_value
        .dict_get(b"name")
        .and_then(Value::as_bytes)
        .ok_or_else(|| err("info.name missing"))?;
    let name = String::from_utf8_lossy(name).into_owned();

    let piece_length = info_value
        .dict_get(b"piece length")
        .and_then(Value::as_int)
        .ok_or_else(|| err("info.piece length missing"))?;
    let piece_length = u32::try_from(piece_length).map_err(|_| err("info.piece length out of range"))?;
    const MAX_PIECE_LENGTH: u32 = 64 * 1024 * 1024; // 64 MiB — generous vs. real torrents (typically <= 16-32 MB)
    if piece_length == 0 || piece_length > MAX_PIECE_LENGTH {
        return Err(err("info.piece length is zero or unreasonably large"));
    }

    let pieces = info_value
        .dict_get(b"pieces")
        .and_then(Value::as_bytes)
        .ok_or_else(|| err("info.pieces missing"))?;
    if pieces.len() % 20 != 0 {
        return Err(err("info.pieces length is not a multiple of 20"));
    }
    let piece_hashes: Vec<[u8; 20]> = pieces.chunks(20).map(|c| c.try_into().expect("chunked by exactly 20")).collect();

    let files = match info_value.dict_get(b"files") {
        Some(list_value) => {
            let list = list_value.as_list().ok_or_else(|| err("info.files is not a list"))?;
            let mut out = Vec::with_capacity(list.len());
            for entry in list {
                let length = entry
                    .dict_get(b"length")
                    .and_then(Value::as_int)
                    .ok_or_else(|| err("files[].length missing"))?;
                let length = u64::try_from(length).map_err(|_| err("files[].length out of range"))?;
                let path_segments = entry
                    .dict_get(b"path")
                    .and_then(Value::as_list)
                    .ok_or_else(|| err("files[].path missing"))?;
                let mut segments = Vec::with_capacity(path_segments.len());
                for seg in path_segments {
                    let b = seg.as_bytes().ok_or_else(|| err("files[].path segment is not a byte string"))?;
                    segments.push(String::from_utf8_lossy(b).into_owned());
                }
                let path = segments.join("/");
                validate_relative_path(&path)?;
                out.push(TorrentFile { path, length });
            }
            out
        }
        None => {
            let length = info_value
                .dict_get(b"length")
                .and_then(Value::as_int)
                .ok_or_else(|| err("single-file torrent missing info.length"))?;
            let length = u64::try_from(length).map_err(|_| err("info.length out of range"))?;
            validate_relative_path(&name)?;
            vec![TorrentFile { path: name.clone(), length }]
        }
    };

    Ok(TorrentMeta { infohash, name, files, piece_length, piece_hashes })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only encoder, so fixtures can be built as a `Value` tree instead of
    /// hand-typed bencode byte literals.
    fn encode(v: &Value) -> Vec<u8> {
        match v {
            Value::Int(n) => format!("i{n}e").into_bytes(),
            Value::Bytes(b) => {
                let mut out = format!("{}:", b.len()).into_bytes();
                out.extend_from_slice(b);
                out
            }
            Value::List(items) => {
                let mut out = vec![b'l'];
                for item in items {
                    out.extend(encode(item));
                }
                out.push(b'e');
                out
            }
            Value::Dict(entries) => {
                let mut out = vec![b'd'];
                for (k, v) in entries {
                    out.extend(encode(&Value::Bytes(k.clone())));
                    out.extend(encode(v));
                }
                out.push(b'e');
                out
            }
        }
    }

    fn str_val(s: &str) -> Value {
        Value::Bytes(s.as_bytes().to_vec())
    }

    #[test]
    fn parses_single_file_torrent() {
        let piece_hashes: Vec<[u8; 20]> = vec![[1u8; 20], [2u8; 20]];
        let mut pieces = Vec::new();
        for h in &piece_hashes {
            pieces.extend_from_slice(h);
        }
        let info = Value::Dict(vec![
            (b"length".to_vec(), Value::Int(40000)),
            (b"name".to_vec(), str_val("movie.mp4")),
            (b"piece length".to_vec(), Value::Int(20000)),
            (b"pieces".to_vec(), Value::Bytes(pieces)),
        ]);
        let top = Value::Dict(vec![
            (b"announce".to_vec(), str_val("udp://tracker.example:80")),
            (b"info".to_vec(), info.clone()),
        ]);
        let bytes = encode(&top);

        let meta = parse_torrent_file(&bytes).unwrap();
        assert_eq!(meta.name, "movie.mp4");
        assert_eq!(meta.piece_length, 20000);
        assert_eq!(meta.piece_hashes, piece_hashes);
        assert_eq!(meta.files, vec![TorrentFile { path: "movie.mp4".to_string(), length: 40000 }]);

        // infohash must be SHA-1 of exactly the encoded `info` dict's bytes.
        let info_bytes = encode(&info);
        let expected: Vec<u8> = Sha1::digest(&info_bytes).to_vec();
        assert_eq!(meta.infohash, expected);
    }

    #[test]
    fn parses_multi_file_torrent() {
        let piece_hashes: Vec<[u8; 20]> = vec![[7u8; 20]];
        let mut pieces = Vec::new();
        for h in &piece_hashes {
            pieces.extend_from_slice(h);
        }
        let files = Value::List(vec![
            Value::Dict(vec![
                (b"length".to_vec(), Value::Int(100)),
                (b"path".to_vec(), Value::List(vec![str_val("sub"), str_val("a.bin")])),
            ]),
            Value::Dict(vec![
                (b"length".to_vec(), Value::Int(200)),
                (b"path".to_vec(), Value::List(vec![str_val("b.bin")])),
            ]),
        ]);
        let info = Value::Dict(vec![
            (b"name".to_vec(), str_val("pack")),
            (b"piece length".to_vec(), Value::Int(300)),
            (b"pieces".to_vec(), Value::Bytes(pieces)),
            (b"files".to_vec(), files),
        ]);
        let top = Value::Dict(vec![(b"info".to_vec(), info)]);
        let bytes = encode(&top);

        let meta = parse_torrent_file(&bytes).unwrap();
        assert_eq!(meta.name, "pack");
        assert_eq!(
            meta.files,
            vec![
                TorrentFile { path: "sub/a.bin".to_string(), length: 100 },
                TorrentFile { path: "b.bin".to_string(), length: 200 },
            ]
        );
    }

    #[test]
    fn rejects_truncated_input() {
        let bytes = b"d4:infod4:name7:movie.m";
        assert!(parse_torrent_file(bytes).is_err());
    }

    #[test]
    fn rejects_missing_info_key() {
        let top = Value::Dict(vec![(b"announce".to_vec(), str_val("x"))]);
        let bytes = encode(&top);
        assert!(parse_torrent_file(&bytes).is_err());
    }

    #[test]
    fn rejects_path_traversal_in_multi_file_torrent() {
        let files = Value::List(vec![Value::Dict(vec![
            (b"length".to_vec(), Value::Int(10)),
            (b"path".to_vec(), Value::List(vec![str_val(".."), str_val("evil.bin")])),
        ])]);
        let info = Value::Dict(vec![
            (b"name".to_vec(), str_val("pack")),
            (b"piece length".to_vec(), Value::Int(10)),
            (b"pieces".to_vec(), Value::Bytes(vec![9u8; 20])),
            (b"files".to_vec(), files),
        ]);
        let top = Value::Dict(vec![(b"info".to_vec(), info)]);
        let bytes = encode(&top);
        assert!(parse_torrent_file(&bytes).is_err());
    }

    #[test]
    fn rejects_negative_piece_length() {
        let info = Value::Dict(vec![
            (b"length".to_vec(), Value::Int(100)),
            (b"name".to_vec(), str_val("f.bin")),
            (b"piece length".to_vec(), Value::Int(-1)),
            (b"pieces".to_vec(), Value::Bytes(vec![1u8; 20])),
        ]);
        let top = Value::Dict(vec![(b"info".to_vec(), info)]);
        let bytes = encode(&top);
        assert!(parse_torrent_file(&bytes).is_err());
    }

    #[test]
    fn rejects_absurdly_large_piece_length() {
        let info = Value::Dict(vec![
            (b"length".to_vec(), Value::Int(100)),
            (b"name".to_vec(), str_val("f.bin")),
            (b"piece length".to_vec(), Value::Int(u32::MAX as i64)),
            (b"pieces".to_vec(), Value::Bytes(vec![1u8; 20])),
        ]);
        let top = Value::Dict(vec![(b"info".to_vec(), info)]);
        let bytes = encode(&top);
        assert!(parse_torrent_file(&bytes).is_err());
    }

    #[test]
    fn rejects_excessively_nested_input() {
        // 100 nested empty lists inside a dict value, followed by unwinding 'e's.
        let mut bytes = b"d4:infol".to_vec();
        bytes.extend(std::iter::repeat_n(b'l', 100));
        bytes.extend(std::iter::repeat_n(b'e', 100));
        bytes.extend_from_slice(b"ee");
        assert!(parse_torrent_file(&bytes).is_err());
    }
}
