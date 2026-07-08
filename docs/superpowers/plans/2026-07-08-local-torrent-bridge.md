# Local Torrent Bridge Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a user convert an already-downloaded `.torrent`'s content into NP2PTP via `np2ptp torrent <file.torrent> --data <dir>`, without touching a real BitTorrent client or network (the `LibrqbitSource`/magnet half of ROADMAP Phase 2 is a separate, later round).

**Architecture:** A minimal bencode parser (`np2ptp-bridge/src/bencode.rs`) reads `.torrent` metainfo without ever reserializing it (the infohash needs the torrent's *original* `info`-dict bytes). A new streaming conversion path (`np2ptp-bridge/src/streaming.rs`) verifies piece hashes by reading files off disk in bounded windows and ingests via the already-streaming `Store::ingest_tree_files`, so a 51 GB torrent is never held in memory. A new `np2ptp torrent` CLI subcommand wires it together with a `Network` bootstrap.

**Tech Stack:** Rust, existing `np2ptp-bridge`/`np2ptp-store`/`np2ptp-net`/`np2ptp-node` crates, `sha1` (already a `np2ptp-bridge` dependency).

## Global Constraints

- Never hold a whole file (or the whole torrent's content) in memory — read and verify in bounded-size windows. (CLAUDE.md golden rule #3.)
- The infohash is `SHA1` of the **raw, byte-exact** original bytes of the top-level `info` dict, sliced from the input — never a reserialization of a parsed value.
- `TorrentFile.path` never includes the torrent's own `name` as a prefix (matches the existing `TorrentMeta`/`TorrentFile` convention already used by `crates/np2ptp-bridge/src/lib.rs`'s tests, and how `read_dir_paths` in `np2ptp-node` returns paths relative to an input directory without that directory's own name).
- Reject any parsed file path that is absolute or contains a `..`/other non-`Normal` path component — a malicious `.torrent` must not be able to write outside `--data <dir>`.
- The existing `TorrentSource` trait, `convert()`, `resolve_or_convert()`, and `verify_pieces()` in `crates/np2ptp-bridge/src/lib.rs` are untouched — this plan adds a parallel path, it does not modify them. All of `crates/np2ptp-bridge`'s existing tests must keep passing unmodified.
- `LibrqbitSource` (downloading a torrent you don't have) and magnet links are out of scope — the CLI only accepts a `.torrent` file path.
- Keep `cargo test --workspace` green and `cargo clippy --workspace --all-targets` at 0 warnings before each commit (CLAUDE.md).
- On Windows, pass commit bodies via `git commit -F <file>` — PowerShell mangles quotes in `-m`. Commit messages end with the `Co-Authored-By: Claude <noreply@anthropic.com>` trailer.

---

### Task 1: Bencode parser (`np2ptp-bridge/src/bencode.rs`)

**Files:**
- Create: `crates/np2ptp-bridge/src/bencode.rs`
- Modify: `crates/np2ptp-bridge/src/lib.rs` (add `mod bencode;` and `pub use bencode::parse_torrent_file;` near the top, after the existing `use` statements)

**Interfaces:**
- Consumes: `crate::{BridgeError, TorrentFile, TorrentMeta}` (already defined in `lib.rs`).
- Produces: `pub fn parse_torrent_file(bytes: &[u8]) -> Result<TorrentMeta, BridgeError>` — later tasks (Task 2's `convert_local`, Task 3's CLI) call this with the raw bytes of a `.torrent` file.

- [ ] **Step 1: Write the failing tests**

Create `crates/np2ptp-bridge/src/bencode.rs` with just the test module first (everything it calls doesn't exist yet, so this won't compile — that's the point):

```rust
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
}
```

- [ ] **Step 2: Run tests to verify they fail to compile**

Run: `cargo test -p np2ptp-bridge bencode:: 2>&1 | head -30`
Expected: compile error — `cannot find function 'parse_torrent_file' in this scope` (and `mod bencode` isn't wired into `lib.rs` yet, so nothing in this file is reachable either — that's fine, this step just proves the test file as written doesn't already silently pass).

- [ ] **Step 3: Implement the decoder and `parse_torrent_file`**

Add this above the `#[cfg(test)]` module in `crates/np2ptp-bridge/src/bencode.rs` (after the `Value` enum/impl block already written in Step 1):

```rust
fn err(msg: &str) -> BridgeError {
    BridgeError::Source(format!("bencode: {msg}"))
}

/// Decode one bencode value from the front of `input`, returning it and the
/// unconsumed remainder.
fn decode(input: &[u8]) -> Result<(Value, &[u8]), BridgeError> {
    match input.first() {
        Some(b'i') => decode_int(input),
        Some(b'l') => decode_list(input),
        Some(b'd') => decode_dict(input),
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

fn decode_list(input: &[u8]) -> Result<(Value, &[u8]), BridgeError> {
    let mut rest = input.strip_prefix(b"l").ok_or_else(|| err("expected 'l'"))?;
    let mut items = Vec::new();
    loop {
        match rest.first() {
            Some(b'e') => return Ok((Value::List(items), &rest[1..])),
            None => return Err(err("unterminated list")),
            _ => {
                let (v, r) = decode(rest)?;
                items.push(v);
                rest = r;
            }
        }
    }
}

fn decode_dict(input: &[u8]) -> Result<(Value, &[u8]), BridgeError> {
    let mut rest = input.strip_prefix(b"d").ok_or_else(|| err("expected 'd'"))?;
    let mut entries = Vec::new();
    loop {
        match rest.first() {
            Some(b'e') => return Ok((Value::Dict(entries), &rest[1..])),
            None => return Err(err("unterminated dict")),
            _ => {
                let (key, r) = decode_byte_string(rest)?;
                let (v, r2) = decode(r)?;
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
                let (_, r2) = decode(r)?;
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
    let (info_value, info_rest) = decode(info_slice)?;
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
        .ok_or_else(|| err("info.piece length missing"))? as u32;

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
                    .ok_or_else(|| err("files[].length missing"))? as u64;
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
                .ok_or_else(|| err("single-file torrent missing info.length"))? as u64;
            validate_relative_path(&name)?;
            vec![TorrentFile { path: name.clone(), length }]
        }
    };

    Ok(TorrentMeta { infohash, name, files, piece_length, piece_hashes })
}
```

- [ ] **Step 4: Wire the module into `lib.rs`**

In `crates/np2ptp-bridge/src/lib.rs`, right after the existing `use sha1::{Digest, Sha1};` line, add:

```rust
mod bencode;
pub use bencode::parse_torrent_file;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p np2ptp-bridge bencode::`
Expected: `test bencode::tests::parses_single_file_torrent ... ok`, `parses_multi_file_torrent ... ok`, `rejects_truncated_input ... ok`, `rejects_missing_info_key ... ok`, `rejects_path_traversal_in_multi_file_torrent ... ok` (5 passed).

Then run the full existing bridge suite to confirm nothing broke: `cargo test -p np2ptp-bridge` — expect all previously-passing tests still pass, plus these 5 new ones.

- [ ] **Step 6: Clippy**

Run: `cargo clippy -p np2ptp-bridge --all-targets`
Expected: 0 warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/np2ptp-bridge/src/bencode.rs crates/np2ptp-bridge/src/lib.rs
git commit -m "feat(bridge): add minimal bencode parser for .torrent metainfo"
```

---

### Task 2: Streaming convert/resolve path (`np2ptp-bridge/src/streaming.rs`)

**Files:**
- Create: `crates/np2ptp-bridge/src/streaming.rs`
- Modify: `crates/np2ptp-bridge/src/lib.rs` (add `mod streaming;` and re-exports)
- Test: `crates/np2ptp-bridge/tests/streaming_convert.rs` (new)

**Interfaces:**
- Consumes: `crate::{BridgeError, Outcome, TorrentFile, TorrentMeta, resolve, publish}` (all already `pub` in `lib.rs`); `np2ptp_store::Store::{ingest_tree_files, ingest_tree_files_no_copy}` (already exist); `np2ptp_net::Network` (already has `provide`, `put_mapping`, `get_mapping`, `find_providers`, `download` — used internally by the existing `resolve`/`publish`, unchanged here).
- Produces:
  - `pub fn verify_pieces_streaming(files: &[(String, PathBuf)], piece_length: usize, piece_hashes: &[[u8; 20]]) -> Result<(), BridgeError>`
  - `pub fn convert_local(store: &Store, meta: &TorrentMeta, data_dir: &Path, no_copy: bool) -> Result<Manifest, BridgeError>`
  - `pub async fn resolve_or_convert_local(net: &Network, store: &Store, meta: &TorrentMeta, data_dir: &Path, no_copy: bool) -> Result<Outcome, BridgeError>`
  
  Task 3's CLI calls `resolve_or_convert_local` directly.

- [ ] **Step 1: Write the failing tests**

Create `crates/np2ptp-bridge/tests/streaming_convert.rs`:

```rust
//! Streaming local-conversion path: verifies piece hashes and ingests torrent
//! content read from real files on disk (never the whole torrent in memory),
//! and checks it agrees with the existing in-memory `convert()` path.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use np2ptp_bridge::{convert, verify_pieces_streaming, BridgeError, TorrentFile, TorrentMeta};
use np2ptp_store::Store;
use sha1::{Digest, Sha1};

struct TmpDir(std::path::PathBuf);
impl TmpDir {
    fn new() -> TmpDir {
        static C: AtomicU64 = AtomicU64::new(0);
        let n = C.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("np2ptp-stream-{}-{}", std::process::id(), n));
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

fn sample(n: usize, seed: u64) -> Vec<u8> {
    let mut x = 0x9E3779B97F4A7C15u64 ^ seed.wrapping_mul(0xD1B54A32D192ED03);
    (0..n).map(|_| { x ^= x >> 12; x ^= x << 25; x ^= x >> 27; (x.wrapping_mul(0x2545F4914F6CDD1D) >> 33) as u8 }).collect()
}

fn piece_hashes_for(files: &[(String, Vec<u8>)], piece_length: usize) -> Vec<[u8; 20]> {
    let mut data = Vec::new();
    for (_, b) in files {
        data.extend_from_slice(b);
    }
    data.chunks(piece_length).map(|c| Sha1::digest(c).into()).collect()
}

#[test]
fn streaming_verifier_agrees_with_in_memory_verify_pieces() {
    // Two files, piece length chosen so a piece spans the file boundary, plus
    // a final undersized piece.
    let files = vec![
        ("a.bin".to_string(), sample(50_000, 1)),
        ("b.bin".to_string(), sample(37_777, 2)),
    ];
    let piece_length = 16_384;
    let hashes = piece_hashes_for(&files, piece_length);

    let dir = TmpDir::new();
    let mut disk_files = Vec::new();
    for (name, bytes) in &files {
        let p = dir.path().join(name);
        std::fs::write(&p, bytes).unwrap();
        disk_files.push((name.clone(), p));
    }

    assert!(verify_pieces_streaming(&disk_files, piece_length, &hashes).is_ok());

    // Corrupt one byte on disk -> must be rejected.
    let mut bad = std::fs::read(&disk_files[1].1).unwrap();
    bad[0] ^= 0xFF;
    std::fs::write(&disk_files[1].1, &bad).unwrap();
    assert!(matches!(
        verify_pieces_streaming(&disk_files, piece_length, &hashes),
        Err(BridgeError::PieceVerificationFailed)
    ));
}

#[test]
fn streaming_verifier_rejects_piece_count_mismatch() {
    let files = vec![("a.bin".to_string(), sample(1000, 3))];
    let dir = TmpDir::new();
    let p = dir.path().join("a.bin");
    std::fs::write(&p, &files[0].1).unwrap();
    let disk_files = vec![("a.bin".to_string(), p)];

    // Hashes for a completely different (empty) piece list.
    let wrong_hashes: Vec<[u8; 20]> = vec![];
    assert!(matches!(
        verify_pieces_streaming(&disk_files, 500, &wrong_hashes),
        Err(BridgeError::PieceVerificationFailed)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn convert_local_matches_in_memory_convert_root() {
    use np2ptp_bridge::{convert_local, TorrentSource, TorrentDownload};

    let files = vec![
        ("dir/a.bin".to_string(), sample(200_000, 4)),
        ("dir/b.bin".to_string(), sample(150_000, 5)),
    ];
    let piece_length = 32_768;
    let piece_hashes = piece_hashes_for(&files, piece_length);
    let meta = TorrentMeta {
        infohash: vec![5u8; 20],
        name: "pack".to_string(),
        files: files.iter().map(|(p, b)| TorrentFile { path: p.clone(), length: b.len() as u64 }).collect(),
        piece_length: piece_length as u32,
        piece_hashes,
    };

    // Reference: existing in-memory convert().
    struct FakeSource {
        meta: TorrentMeta,
        files: Vec<(String, Vec<u8>)>,
    }
    impl TorrentSource for FakeSource {
        async fn infohash(&self, _: &str) -> Result<Vec<u8>, BridgeError> {
            Ok(self.meta.infohash.clone())
        }
        async fn metadata(&self, _: &str) -> Result<Option<TorrentMeta>, BridgeError> {
            Ok(Some(self.meta.clone()))
        }
        async fn fetch(&self, _: &str) -> Result<TorrentDownload, BridgeError> {
            Ok(TorrentDownload { meta: self.meta.clone(), files: self.files.clone() })
        }
    }
    let ref_dir = TmpDir::new();
    let ref_store = Store::open(ref_dir.path()).unwrap();
    let src = FakeSource { meta: meta.clone(), files: files.clone() };
    let (ref_manifest, _) = convert(&ref_store, &src, "x.torrent").await.unwrap();

    // Streaming: files written to disk, then convert_local reads them back.
    let data_dir = TmpDir::new();
    for (rel, bytes) in &files {
        let p = data_dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
    }
    let store_dir = TmpDir::new();
    let store = Store::open(store_dir.path()).unwrap();
    let manifest = convert_local(&store, &meta, data_dir.path(), false).unwrap();

    assert_eq!(manifest.root, ref_manifest.root, "streaming and in-memory converters must agree on the content id");

    // And it's actually retrievable/correct.
    let rebuilt = store.export_tree(&manifest).unwrap();
    let mut rebuilt_sorted = rebuilt.clone();
    rebuilt_sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut expected_sorted = files.clone();
    expected_sorted.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(rebuilt_sorted, expected_sorted);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn convert_local_rejects_corrupted_file_on_disk() {
    use np2ptp_bridge::convert_local;

    let files = vec![("a.bin".to_string(), sample(60_000, 6))];
    let piece_length = 16_384;
    let piece_hashes = piece_hashes_for(&files, piece_length);
    let meta = TorrentMeta {
        infohash: vec![6u8; 20],
        name: "pack".to_string(),
        files: files.iter().map(|(p, b)| TorrentFile { path: p.clone(), length: b.len() as u64 }).collect(),
        piece_length: piece_length as u32,
        piece_hashes,
    };

    let data_dir = TmpDir::new();
    let mut bad = files[0].1.clone();
    bad[0] ^= 0xFF;
    std::fs::write(data_dir.path().join("a.bin"), &bad).unwrap();

    let store_dir = TmpDir::new();
    let store = Store::open(store_dir.path()).unwrap();
    assert!(matches!(
        convert_local(&store, &meta, data_dir.path(), false),
        Err(BridgeError::PieceVerificationFailed)
    ));
}
```

- [ ] **Step 2: Run tests to verify they fail to compile**

Run: `cargo test -p np2ptp-bridge --test streaming_convert 2>&1 | head -30`
Expected: compile error — `unresolved import 'np2ptp_bridge::verify_pieces_streaming'` (and `convert_local`) — neither exists yet.

- [ ] **Step 3: Implement `streaming.rs`**

Create `crates/np2ptp-bridge/src/streaming.rs`:

```rust
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
        StreamingPieceVerifier { piece_length, piece_hashes, buf: Vec::with_capacity(piece_length), next_piece: 0 }
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
```

- [ ] **Step 4: Wire the module into `lib.rs`**

In `crates/np2ptp-bridge/src/lib.rs`, right after the `mod bencode; pub use bencode::parse_torrent_file;` lines added in Task 1, add:

```rust
mod streaming;
pub use streaming::{convert_local, resolve_or_convert_local, verify_pieces_streaming};
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p np2ptp-bridge --test streaming_convert`
Expected: 4 tests pass (`streaming_verifier_agrees_with_in_memory_verify_pieces`, `streaming_verifier_rejects_piece_count_mismatch`, `convert_local_matches_in_memory_convert_root`, `convert_local_rejects_corrupted_file_on_disk`).

Then run the full bridge suite: `cargo test -p np2ptp-bridge` — expect every test (Task 1's, this task's, and the pre-existing ones in `lib.rs` and `tests/bridge_network.rs`) to pass.

- [ ] **Step 6: Clippy**

Run: `cargo clippy -p np2ptp-bridge --all-targets`
Expected: 0 warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/np2ptp-bridge/src/streaming.rs crates/np2ptp-bridge/src/lib.rs crates/np2ptp-bridge/tests/streaming_convert.rs
git commit -m "feat(bridge): stream torrent conversion from disk instead of loading it all into RAM"
```

---

### Task 3: CLI `np2ptp torrent` subcommand

**Files:**
- Modify: `crates/np2ptp-node/Cargo.toml` (add `np2ptp-bridge` dependency)
- Modify: `crates/np2ptp-node/src/main.rs` (add `cmd_torrent`, wire into the command dispatch and `print_usage`)
- Modify: `README.md` (mention the new command in the crate table / examples if present — see Step 6)
- Modify: `docs/EXAMPLES.md` (add a CLI usage example)
- Test: `crates/np2ptp-node/tests/integration.rs` (add a CLI smoke test)

**Interfaces:**
- Consumes: `np2ptp_bridge::{parse_torrent_file, resolve_or_convert_local}` (Tasks 1 and 2); existing `main.rs` helpers `parse`, `load_or_create_seed`, `DEFAULT_STORE`, `DEFAULT_RELAY`.
- Produces: the `np2ptp torrent <file.torrent> --data <dir> [--store <dir>] [--no-copy] [--relay <addr>] [--no-relay] [--json]` CLI command. Nothing later depends on this beyond the CLI itself — this is the final task.

- [ ] **Step 1: Write the failing test**

Add to `crates/np2ptp-node/tests/integration.rs` (append at the end of the file; it already has `use np2ptp_core::Hash;` etc. at the top — this test adds its own local bencode-encoding helper since `np2ptp-node` doesn't depend on `np2ptp-bridge`'s test-only encoder):

```rust
#[test]
fn torrent_json_converts_local_data_and_emits_a_final_result_event() {
    // Minimal hand-encoded single-file .torrent: a bencode dict with an
    // "info" dict (name/piece length/pieces/length). No announce needed —
    // parse_torrent_file only reads the "info" key.
    fn bencode_str(s: &str) -> Vec<u8> {
        let mut out = format!("{}:", s.len()).into_bytes();
        out.extend_from_slice(s.as_bytes());
        out
    }
    fn bencode_int(n: i64) -> Vec<u8> {
        format!("i{n}e").into_bytes()
    }
    fn bencode_bytes_field(key: &str, raw: &[u8]) -> Vec<u8> {
        let mut out = bencode_str(key);
        out.extend(format!("{}:", raw.len()).into_bytes());
        out.extend_from_slice(raw);
        out
    }

    let dir = TmpDir::new();
    let data = sample(300_000, 70);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(data_dir.join("movie.bin"), &data).unwrap();

    let piece_length: usize = 65_536;
    let piece_hashes: Vec<u8> = data
        .chunks(piece_length)
        .flat_map(|c| {
            use sha1::{Digest, Sha1};
            let h: [u8; 20] = Sha1::digest(c).into();
            h
        })
        .collect();

    let mut info = Vec::new();
    info.push(b'd');
    info.extend(bencode_str("length"));
    info.extend(bencode_int(data.len() as i64));
    info.extend(bencode_str("name"));
    info.extend(bencode_str("movie.bin"));
    info.extend(bencode_str("piece length"));
    info.extend(bencode_int(piece_length as i64));
    info.extend(bencode_bytes_field("pieces", &piece_hashes));
    info.push(b'e');

    let mut top = Vec::new();
    top.push(b'd');
    top.extend(bencode_str("info"));
    top.extend(info);
    top.push(b'e');

    let torrent_path = dir.path().join("movie.torrent");
    std::fs::write(&torrent_path, &top).unwrap();

    let store_dir = dir.path().join("store");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_np2ptp"))
        .arg("torrent")
        .arg(&torrent_path)
        .arg("--data")
        .arg(&data_dir)
        .arg("--store")
        .arg(&store_dir)
        .arg("--no-relay")
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert!(!lines.is_empty(), "expected at least one NDJSON line, got: {stdout}");

    let last: serde_json::Value = serde_json::from_str(lines.last().unwrap())
        .unwrap_or_else(|e| panic!("last line not valid JSON: {:?}: {e}", lines.last()));
    assert_eq!(last["event"], "result");
    assert_eq!(last["op"], "torrent");
    assert!(last["root"].as_str().unwrap().starts_with("np2ptp:"));
    assert_eq!(last["converted"], true);
    assert_eq!(last["bytes_total"], 300_000);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p np2ptp-node --test integration torrent_json_converts_local_data_and_emits_a_final_result_event 2>&1 | head -30`
Expected: fails — either a compile error (test file compiles fine on its own since it only uses `std`/`serde_json`/`sample`/`TmpDir`, all already present) or, once it compiles, a runtime failure because `np2ptp torrent` is not yet a recognized subcommand (`unknown command: torrent`).

- [ ] **Step 3: Add the `np2ptp-bridge` dependency**

In `crates/np2ptp-node/Cargo.toml`, in the `[dependencies]` section, add:

```toml
np2ptp-bridge = { path = "../np2ptp-bridge" }
```

`crates/np2ptp-node/Cargo.toml` has no `[dev-dependencies]` section yet (the
test in Step 1 hashes piece data with `sha1` directly) — add one at the end
of the file:

```toml
[dev-dependencies]
sha1 = "0.10"
```

- [ ] **Step 4: Implement `cmd_torrent` and wire it into `main.rs`**

In `crates/np2ptp-node/src/main.rs`, add `"torrent" => cmd_torrent(&args[1..]),` to the `match` in `run()` (right after the existing `Some("relay") => cmd_relay(&args[1..]),` line):

```rust
        Some("relay") => cmd_relay(&args[1..]),
        Some("torrent") => cmd_torrent(&args[1..]),
```

Then add the function itself (near `cmd_fetch`, since it's the most similar existing command — a one-shot operation that spawns a `Network`, does one thing, and exits):

```rust
/// Convert an already-downloaded `.torrent`'s content into NP2PTP. Only
/// reads local files (`--data <dir>`, which must contain the torrent's file
/// tree directly, e.g. what a BitTorrent client's save-path already looks
/// like for that torrent) — downloading a torrent you don't have yet is a
/// separate, later feature.
fn cmd_torrent(args: &[String]) -> Result<(), Box<dyn Error>> {
    let (pos, flags) = parse(args, &["--data", "--store", "--relay"]);
    let torrent_file = *pos.first().ok_or("torrent: missing <file.torrent>")?;
    let data_dir = flags.get("data").cloned().ok_or("torrent: missing --data <dir>")?;
    let store_dir = flags.get("store").map(String::as_str).unwrap_or(DEFAULT_STORE).to_string();
    let no_copy = flags.contains_key("no-copy");
    let no_relay = flags.contains_key("no-relay");
    let relay_override = flags.get("relay").cloned();
    let json = flags.contains_key("json");

    let torrent_bytes = fs::read(torrent_file)?;
    let meta = np2ptp_bridge::parse_torrent_file(&torrent_bytes)?;
    let identity_seed = load_or_create_seed(&format!("{store_dir}/identity.key"))?;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let net = Network::spawn(Store::open(&store_dir)?, Some(identity_seed))?;
        net.listen("/ip4/0.0.0.0/udp/0/quic-v1".parse()?).await?;

        if !no_relay {
            let relay_addr: Multiaddr = relay_override.unwrap_or_else(|| DEFAULT_RELAY.to_string()).parse()?;
            if !json {
                println!("relay: dialing {relay_addr} ...");
            }
            net.dial(relay_addr).await?;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        let store = Store::open(&store_dir)?;
        let outcome =
            np2ptp_bridge::resolve_or_convert_local(&net, &store, &meta, Path::new(&data_dir), no_copy).await?;

        if json {
            println!(
                "{}",
                serde_json::json!({
                    "event":"result","op":"torrent",
                    "root": outcome.manifest.uri(),
                    "converted": outcome.converted,
                    "files_total": outcome.manifest.files.len(),
                    "chunks_total": outcome.manifest.chunks.len(),
                    "bytes_total": outcome.manifest.total_size,
                })
            );
        } else {
            println!(
                "{} ({} files, {} chunks) - {}",
                outcome.manifest.uri(),
                outcome.manifest.files.len(),
                outcome.manifest.chunks.len(),
                if outcome.converted { "converted from BitTorrent" } else { "already bridged, served from NP2PTP" }
            );
        }
        Ok::<(), Box<dyn Error>>(())
    })
}
```

Update `print_usage()` to mention the new command — find its existing body (it prints the other commands' usage lines) and add a line for `torrent` in the same style, e.g. alongside the existing `serve`/`fetch` lines:

```rust
    println!("  np2ptp torrent <file.torrent> --data <dir> [--store <dir>] [--no-copy] [--json]");
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p np2ptp-node --test integration torrent_json_converts_local_data_and_emits_a_final_result_event`
Expected: PASS.

Then run the full node suite and the full workspace: `cargo test -p np2ptp-node` then `cargo test --workspace` — expect everything green.

- [ ] **Step 6: Update docs**

In `README.md`, in the `np2ptp-bridge` row of the crates table, the description already says "BitTorrent ↔ NP2PTP gateway (core logic only — see its own docs)" — update it to:

```markdown
| `np2ptp-bridge` | BitTorrent ↔ NP2PTP gateway: convert an already-downloaded torrent (`np2ptp torrent`) |
```

In `docs/EXAMPLES.md`, add a new subsection after the existing "CLI: Real Network Transfer (QUIC)" section (before "## Non-Interactive Usage"):

```markdown
## CLI: Converting a Torrent

Already have a torrent's data on disk (from a real BitTorrent client)? Bridge
it into NP2PTP instead of re-downloading it as a fresh NP2PTP transfer:

\`\`\`sh
np2ptp torrent my-linux-iso.torrent --data ~/Downloads/my-linux-iso --store seedstore
\`\`\`

`--data` must point at the directory that directly contains the torrent's
file tree — the same relationship `pack` already has to a directory input
(for a multi-file torrent, that's usually the sub-folder a BitTorrent client
saves it under, named after the torrent itself; for a single-file torrent,
it's the folder containing that one file). The content is verified against
the torrent's own piece hashes (streamed from disk — even a 50+ GB torrent is
never loaded into memory) before being bridged: `resolve_or_convert_local`
first checks whether another peer already bridged this exact torrent
(matched by BitTorrent infohash) and, if so, downloads it from NP2PTP instead
of re-verifying anything.

Only a `.torrent` **file** is supported for now (not a magnet link) — pulling
a torrent you don't already have, via a real BitTorrent swarm, is a separate,
later feature.
\`\`\`
```

(Remove the stray extra ` ``` ` at the end if your editor doesn't already balance the fenced block — this snippet is meant to close after the "later feature." sentence.)

- [ ] **Step 7: Update ROADMAP.md**

In `ROADMAP.md`, under "🚧 Phase 2 — Torrent bridge + automatic peer discovery (next)", update the first bullet to reflect what's done:

Find:
```markdown
1. **Finish the bridge** (`np2ptp-bridge`):
   - **`LocalTorrentSource`** — parse a `.torrent` (bencode → infohash, file list,
     piece hashes) and read already-downloaded files from disk. **Must stream**
     (the user's real torrent is 51 GB). Bencode parser must extract the raw `info`
     dict bytes to SHA-1 the infohash.
   - **`LibrqbitSource`** — download torrents you *don't* have (behind the
     `librqbit` feature, already in `Cargo.toml`). `.torrent` + magnet.
   - **`np2ptp torrent <file|magnet>`** CLI: run `resolve_or_convert` (lookup on the
     DHT → fast path; else convert → verify against piece hashes → publish mapping +
     provide). Stream verification (don't concat 51 GB in RAM).
```

Replace with:
```markdown
1. **Finish the bridge** (`np2ptp-bridge`):
   - ✅ **Local conversion** — `parse_torrent_file` (bencode → infohash, file list,
     piece hashes) + `resolve_or_convert_local`/`convert_local`, streaming both
     piece verification and ingestion from disk (never the whole torrent in RAM).
     `np2ptp torrent <file.torrent> --data <dir>` CLI command.
   - **`LibrqbitSource`** — download torrents you *don't* have (behind the
     `librqbit` feature, already in `Cargo.toml`). `.torrent` + magnet. Not yet
     started — a real BitTorrent swarm is harder to test deterministically, so
     the fully-offline local-conversion half shipped first.
```

- [ ] **Step 8: Clippy on the whole workspace**

Run: `cargo clippy --workspace --all-targets`
Expected: 0 warnings.

- [ ] **Step 9: Commit**

```bash
git add crates/np2ptp-node/Cargo.toml crates/np2ptp-node/src/main.rs crates/np2ptp-node/tests/integration.rs README.md docs/EXAMPLES.md ROADMAP.md Cargo.lock
git commit -m "feat(node): add 'np2ptp torrent' CLI command for local torrent conversion"
```

(`Cargo.lock` changes because `np2ptp-node` now depends on `np2ptp-bridge` — commit it alongside, same convention as every other dependency change in this repo.)
