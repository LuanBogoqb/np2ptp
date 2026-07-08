# Local torrent bridge (LocalTorrentSource) — Design

## Goal

Let a user convert an **already-downloaded** `.torrent`'s content into NP2PTP,
without touching a real BitTorrent client or the network — `np2ptp torrent
<file.torrent> --data <dir>`. This is the first half of ROADMAP Phase 2's
"Finish the bridge" item. The second half — `LibrqbitSource`, downloading a
torrent you *don't* have yet, via `librqbit` — is out of scope for this round
and deferred to a follow-up (it needs a real BitTorrent swarm, which is much
harder to test deterministically in CI; shipping the deterministic,
fully-offline half first is lower risk).

## Why the existing bridge core isn't enough as-is

`np2ptp-bridge`'s `TorrentSource` trait, `convert()`, and `verify_pieces()`
already work (tests pass), but they hold the **entire torrent's content in
RAM** as `Vec<(String, Vec<u8>)>` — fine for tests, not for a real 51 GB
torrent. ROADMAP Phase 2 explicitly calls out that `LocalTorrentSource`
**must stream**. Rather than force streaming through the existing
`TorrentSource` trait (whose `fetch()` returns in-memory `TorrentDownload`,
useful for `LibrqbitSource`'s future magnet path), this design adds a
**parallel, streaming-only path** for the local case: `convert_local()` /
`resolve_or_convert_local()`. The existing trait, `convert()`, and
`resolve_or_convert()` are untouched, and stay the entry point for the future
`LibrqbitSource` round.

## Architecture

Three new pieces in `crates/np2ptp-bridge`:

### 1. `bencode.rs` — minimal `.torrent` parser

A `.torrent` file is a bencoded dict. We only need enough of bencode to read
one: integers (`i<n>e`), byte strings (`<len>:<bytes>`), lists (`l...e`), and
dicts (`d...e`). A recursive-descent decoder returns `(Value, &[u8])` — the
parsed value and the unconsumed remainder of the input — which is the
standard trick for also recovering **exactly which bytes** made up a given
value: `consumed = before.len() - after.len()`.

`parse_torrent_file(bytes: &[u8]) -> Result<TorrentMeta, BridgeError>`:
- Decodes the top-level dict, and when walking its keys, specifically
  captures the raw byte slice consumed by the `"info"` key's value (not a
  reserialization — reserializing bencode isn't guaranteed byte-identical,
  e.g. key order or integer formatting, and BitTorrent infohashes are
  famously sensitive to this).
- `infohash = SHA1(raw info bytes)`.
- Reads `info.name` (`Bytes` → UTF-8 lossy `String`).
- Reads `info."piece length"` (`Int`) and splits `info.pieces` (`Bytes`) into
  20-byte chunks for `piece_hashes`.
- Reads `info.files` if present (multi-file torrent): each entry is a dict
  with `length` (`Int`) and `path` (`List` of `Bytes` segments, joined with
  `/`). If absent (single-file torrent), the one file is `{ path: info.name,
  length: info.length }`.
- `TorrentFile.path` never includes the torrent's own `name` as a prefix —
  matching the existing convention (`TorrentMeta.name` is separate metadata,
  same relationship `Store::ingest_tree`'s `name` param already has to its
  `files` list, and how `pack`'s `read_dir_paths` returns paths relative to
  the input directory, not including the directory's own name).

### 2. Streaming convert/resolve path

`resolve_or_convert_local(net: &Network, store: &Store, meta: &TorrentMeta,
data_dir: &Path) -> Result<Outcome, BridgeError>`:
1. Calls the existing `resolve(net, store, &meta.infohash, Some(meta))` — if
   some other peer already bridged this infohash, done, no local I/O needed
   beyond what `resolve` already does.
2. Otherwise, `convert_local(store, meta, data_dir)`:
   - Builds `Vec<(String, PathBuf)>` = `meta.files.iter().map(|f| (f.path,
     data_dir.join(&f.path)))`.
   - Streams piece verification: walks the files **in torrent order**,
     reading each in fixed-size buffers (not the whole file), feeding bytes
     into a small streaming piece-hasher that accumulates exactly
     `piece_length`-byte windows *across file boundaries* (a piece can span
     two files), SHA-1-hashes each completed window, and compares it against
     `meta.piece_hashes[i]` — erroring out (`BridgeError::PieceVerificationFailed`)
     on the first mismatch or a piece-count mismatch at the end. Never holds
     more than one piece's worth of bytes in memory at a time.
   - Only after verification succeeds: `store.ingest_tree_files(&files,
     Some(meta.name.clone()))` (already exists, already streams — reads each
     file from disk in bounded-size windows, chunks it, and dedups against
     the store without ever holding a whole file in memory).
   - Returns `(Manifest, TorrentMeta)`, same shape as `convert()`.
3. Calls the existing `publish(net, &manifest, &meta.infohash)`.
4. Returns `Outcome { manifest, infohash: meta.infohash.clone(), converted:
   true/false }`, same struct as the existing in-memory path.

`--data <dir>` contract: `dir` contains the torrent's file tree directly —
`dir.join(&file.path)` for every file, no extra `name` subfolder inserted by
us. (Real BitTorrent clients typically save a multi-file torrent under
`<save-path>/<name>/...`; the CLI help text says to point `--data` at that
already-named folder, not its parent — this keeps the on-disk contract
identical to what `pack`'s directory input already expects, so there's one
convention to explain, not two.)

**Error handling:** a missing or wrong-length file under `data_dir` surfaces
as a distinct `BridgeError` variant (`BridgeError::Source(String)` with a
message naming the file) rather than a panic or an opaque I/O error —
matches the existing pattern where `TorrentSource::fetch` failures also use
`BridgeError::Source`.

### 3. CLI: `np2ptp torrent`

```
np2ptp torrent <file.torrent> --data <dir> [--store <dir>] [--no-copy] [--json]
```

- Only accepts a `.torrent` file path — no magnet support yet (that needs a
  live BitTorrent fetch for metadata, i.e. `LibrqbitSource`, next round).
- `--data <dir>`: required. The directory holding the torrent's already-downloaded
  files (see contract above).
- `--store`: defaults to `.np2ptp-store`, same as every other command.
- `--no-copy`: same meaning as `pack --no-copy` — reference files in place
  (via `Store::ingest_tree_files_no_copy`) instead of copying into the store.
- Bootstraps a `Network` the same way `serve` does today (persistent identity
  keyed off `--store`, default relay + tracker, listens on
  `/ip4/0.0.0.0/udp/0/quic-v1`) so `resolve_or_convert_local` can check the
  DHT and, if it converts, immediately start providing.
- Prints the resulting `np2ptp:<root>` link and whether it hit the network
  path or converted (mirrors `fetch`'s and `pack`'s existing output shape,
  including `--json` event lines).

## Testing

- **`bencode.rs` unit tests**: hand-written bencode byte literals (not files
  on disk) covering a single-file torrent and a multi-file torrent; assert
  the parsed `TorrentMeta` (name, files, piece_length, piece_hashes,
  infohash) is correct. A malformed/truncated input returns an `Err`, not a
  panic.
- **Streaming piece verifier unit tests**: for the same random file-set
  fixture pattern already used in `np2ptp-bridge`'s existing tests (the
  `sample(n, seed)` PRNG-based generator), assert the streaming verifier
  agrees with the existing in-memory `verify_pieces()` — across a piece that
  spans a file boundary, a final undersized piece, and a fed-in-arbitrary-chunk-sizes
  case (proving it doesn't depend on read-buffer size lining up with piece
  boundaries). Also assert it rejects corrupted bytes and a piece-count
  mismatch.
- **`convert_local` integration test**: writes real files to a temp
  directory (reusing the `TmpDir` helper pattern already in
  `crates/np2ptp-bridge/tests/bridge_network.rs`), builds a `TorrentMeta` by
  hand (same `fake_meta`-style helper), runs `convert_local`, and asserts (a)
  it produces the **same Merkle root** as the existing in-memory `convert()`
  given the same bytes (determinism holds across the two code paths), and
  (b) a corrupted on-disk file is rejected with
  `BridgeError::PieceVerificationFailed`.
- **CLI**: a smoke-level integration test in `np2ptp-node`'s existing test
  style (`crates/np2ptp-node/tests/integration.rs`) — pack a small directory
  as if it were torrent content, hand-build a matching `.torrent` file (via
  the same bencode encoding used in the unit tests, or a tiny raw literal),
  run the `torrent` subcommand against it, and check it prints an
  `np2ptp:` link and the content is retrievable from the resulting store.

## Out of scope (deferred)

- `LibrqbitSource` (downloading a torrent you don't have via `librqbit`,
  magnet support) — next round, per the earlier scope decision.
- Automatic discovery (mDNS, bootstrap DHT nodes) and NAT traversal
  improvements (UPnP/NAT-PMP) — separate ROADMAP Phase 2 bullets, unrelated
  to torrent conversion itself.
- No-copy verification-by-streaming-from-disk for the *existing* in-memory
  `TorrentSource`/`convert()` path — that path is intentionally left as-is
  for now; it's only exercised by tests and the future `LibrqbitSource`,
  which will decide its own on-disk strategy when it's designed.
