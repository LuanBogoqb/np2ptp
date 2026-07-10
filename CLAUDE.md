# CLAUDE.md — guide for AI agents working on NP2PTP

NP2PTP is a P2P transfer protocol in Rust, inspired by BitTorrent. It fixes
BitTorrent's weak spots: content-defined chunking + BLAKE3 Merkle for integrity &
dedup, RaptorQ for permanence, a reputation ledger for incentives, and libp2p/QUIC
for transport. **Read [`ROADMAP.md`](ROADMAP.md) for the full plan and status.**

## Build & test

```sh
cargo test --workspace          # ~100 tests, keep green
cargo clippy --workspace --all-targets   # keep at 0 warnings
cargo run --release -p np2ptp-sim        # research harness -> reports/
cargo build --release -p np2ptp-node     # the `np2ptp` CLI binary
```

Windows dev machine: **MSVC toolchain**. `cargo`/`rustc` may not be on a fresh
shell's PATH — prefix with `$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"`.
**Build `--release` when timing FEC/large content** (RaptorQ is ~100x slower in debug).

## Golden rules (violating these has bitten us)

1. **Determinism is sacred.** The content id is the BLAKE3 Merkle root over
   content-defined chunks (FastCDC, fixed params in `np2ptp-core::chunk`). Two
   nodes chunking the same bytes in the same file order MUST produce the same
   root. There is a test asserting streaming == in-memory chunking — never break it.
2. **Never verify chunks with `Manifest::verify_chunk` in a loop.** It rebuilds
   the whole Merkle tree per call (O(n²) total — dies at scale). Use
   `root_is_consistent()` ONCE, then `chunk_hash_ok(i, bytes)` per chunk.
3. **Stream large content.** Use `Store::ingest_tree_files` / `export_tree_to_dir`
   / `export_to` and the streaming download path — never load a whole file (let
   alone a whole torrent) into memory. Real content is 10s of GB.
4. **The manifest is trusted only after** `root == requested_root` AND
   `root_is_consistent()`. `get_manifest` already does this on the network path.
5. **Two `Store` handles on the same directory is normal** (e.g. `Network::spawn`
   owns one, a caller opens a second right after) — one MUST see what the other
   just packed. `put`/`get`/`has` refresh from `packs/index` on a miss
   (`Store::refresh_pack_index`), tailing only the bytes appended since last
   checked. Don't "simplify" that into a full re-read/re-parse of the index —
   that makes packing N new chunks (the common case) an accidental O(n²); it
   once hung packing 1 GB for 3+ minutes before this was caught by actually
   timing a real pack, not just the unit tests (which never write enough
   chunks for the quadratic cost to show up).

## Conventions

- Errors: `thiserror` per crate. Tests: self-cleaning temp dirs (no `tempfile`
  dep — keeps the tree pure-Rust where it matters). Match the surrounding style.
- libp2p 0.55, pinned. Kademlia runs in **Server mode** (set in `Network::spawn`).
  `put_record` (Quorum::One) needs ≥1 reachable remote peer.
- Keep the workspace green + clippy-clean before committing. Commit messages end
  with the `Co-Authored-By` trailer. On Windows, pass commit bodies via
  `git commit -F <file>` (PowerShell mangles quotes in `-m`).

## Layout

`crates/np2ptp-{core,store,fec,rep,net,node,sim,bridge}` + `python/` (CLI wrapper).
See ROADMAP.md "Architecture" for what each does and "Gotchas" for the rest.
