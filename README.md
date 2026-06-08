# NP2PTP — New Peer-To-Peer Transfer Protocol

A research prototype exploring a "BitTorrent 2.0": fix what torrents do badly and
improve what they already do well. The goal is **measurable** experiments, not a
production client.

## Pain points being targeted (in priority order)

1. **NAT / connectivity** — too many peers can't accept inbound connections.
2. **Permanence / incentives** — content dies when seeders leave; seeding is unrewarded.
3. **Integrity / dedup** — coarse verification and no cross-content deduplication.

Out of scope for the MVP: privacy/anonymity, streaming, mutable content.

## Design in one paragraph

Don't reinvent the plumbing. Build on `rust-libp2p` (QUIC transport, key-based
identity, Noise, Kademlia DHT, NAT traversal, gossip). The novelty lives in the
layers above: content addressing with BLAKE3 + Merkle trees and content-defined
chunking for cross-content dedup, RaptorQ erasure coding for permanence, and a
persistent reputation ledger for incentives — plus a simulation harness that
measures whether any of it actually beats a baseline.

## Crates

| Crate           | Status      | Responsibility                                            |
|-----------------|-------------|-----------------------------------------------------------|
| `np2ptp-core`   | ✅ built     | Content-defined chunking, BLAKE3 hashing, Merkle trees, multi-file manifests, `.nptp` format |
| `np2ptp-store`  | ✅ built     | Content-addressed chunk store with cross-content dedup    |
| `np2ptp-fec`    | ✅ built     | RaptorQ erasure coding (k-of-n recovery); wired into the network download (`--fec`) |
| `np2ptp-node`   | ✅ built     | `.nptp` linker (`pack`, files **and folders**) + client (`get`/`info`/`serve`/`fetch`) CLI |
| `np2ptp-rep`    | ✅ built     | Ed25519 identity, signed receipts, reputation ledger; wired into net (accounting + choke) |
| `np2ptp-net`    | 🚧 partial   | libp2p/QUIC: e2e download by content id, DHT discovery, reputation choke, FEC symbols, relay reservation; full relayed transfer + DCUtR need real NATs |
| `np2ptp-sim`    | ✅ built     | Research harness: measures dedup, permanence, free-riding, FEC cost |

62 unit/integration tests today, all green — including real libp2p nodes downloading a
whole file over QUIC (chunk-by-chunk *and* via RaptorQ symbols), discovering each other
via the DHT, choking a non-reciprocating peer, and a behind-NAT node obtaining a relay
reservation.

## Research results (`np2ptp-sim`)

`cargo run -p np2ptp-sim` spins up real nodes and reports (representative run):

| Experiment | Result |
|---|---|
| **Dedup** — store a file then a lightly-edited v2 | **~49%** of chunks deduplicated |
| **Permanence** — seeder leaves after one peer re-shares | survives **only with** re-sharing (with ✓ / without ✗) |
| **Free-riding** — leech with the reputation choke | choke off → completes; **choke on → cut off** |
| **FEC cost** — chunk vs RaptorQ-symbol download (1 MB, `--release`) | chunk ~107 ms vs FEC ~110 ms |

The FEC result tells an optimization story: the first cut fetched one ~1.2 KB symbol per
request (~875 round-trips/MB) and decoded after every symbol, taking ~25 s. With **symbol
batching** (128/request) and **decoding once enough symbols arrive**, erasure-coded
download now ~matches plain chunk download (~110 ms) while adding any-*k*-of-*n* resilience
— so permanence is essentially free. (Run the harness with `--release`; RaptorQ's GF(256)
math is much slower in a debug build.) The scenario assertions run in CI
(`cargo test -p np2ptp-sim`).

### NAT traversal status

The relay (v2), DCUtR, and AutoNAT behaviours are wired in. On a single dev machine:
a behind-NAT node successfully reserves a slot on a relay and gets a dialable
`/…/p2p-circuit/p2p/<peer>` address (covered by a passing test). A *full content
download through the relay* is flaky on loopback — the relayed QUIC stream tears down and
DCUtR has no real NAT to punch — so that test is `#[ignore]`d pending validation on real
NATed hosts (or via the simulation harness). The behaviours themselves are production-shaped.

## Build & test

Requires the Rust toolchain (https://rustup.rs).

```sh
cargo test            # run all unit tests (19 today, all green)
cargo test -p np2ptp-core
cargo test -p np2ptp-store
```

### Windows note (current dev machine)

Builds with the **MSVC** toolchain. Setup that was done once:

- `winget install Rustlang.Rustup`
- `winget install Microsoft.VisualStudio.2022.BuildTools` with the VC++ (VCTools) workload
  — provides `link.exe`, auto-detected by rustc/cc.
- `rustup default stable-x86_64-pc-windows-msvc`

If a fresh shell can't find `link.exe`, build from an "x64 Native Tools Command Prompt"
(or run `vcvars64.bat` first); usually rustc's auto-detection makes that unnecessary.

## What `np2ptp-core` gives you today

- `Manifest::from_bytes(data, name)` — chunk + hash content into a shareable manifest.
- `manifest.uri()` → `np2ptp:<hex-root>` link; `Manifest::root_from_uri(uri)` to parse.
- `manifest.verify_chunk(i, bytes)` — verify one chunk against the Merkle root.
- `manifest.reconstruct(fetch)` — reassemble content, verifying every chunk.

## Try it: link a file (or folder) and download it

`pack` accepts a **single file or a whole directory tree** — like a torrent, a folder
keeps its relative paths and the top-level name.

```sh
# Linker: a single file
cargo run -p np2ptp-node --bin np2ptp -- pack myfile.bin --out myfile.nptp --store seedstore

# Linker: an entire folder (recurses; preserves subdirectories)
cargo run -p np2ptp-node --bin np2ptp -- pack ./myfolder --out folder.nptp --store seedstore

# Inspect a .nptp file (lists the files inside a tree)
cargo run -p np2ptp-node --bin np2ptp -- info folder.nptp

# Client: rebuild from a seed's store, verifying every chunk
cargo run -p np2ptp-node --bin np2ptp -- get folder.nptp --source seedstore --store clientstore --out ./restored
```

For a tree, `--out` is the destination directory and the structure is recreated under it
(`restored/data/blob.bin`, …). With no `--out`, files land in a folder named after the
manifest (torrent-style). Identical files anywhere in the tree are stored and transferred
**once** (content-defined chunking + dedup).

`--source` is the offline stand-in for a peer (another node's store dir). For the real
thing, use `serve`/`fetch` below. Every chunk is verified against the manifest's Merkle
root on arrival, so a corrupt/lying source is rejected immediately.

## Real network transfer (QUIC)

```sh
# On the seeding machine: pack, then serve over the network (QUIC + DHT)
cargo run -p np2ptp-node --bin np2ptp -- pack ./myfolder --out folder.nptp --store seedstore
cargo run -p np2ptp-node --bin np2ptp -- serve folder.nptp --store seedstore
#   prints:  np2ptp fetch <link> --peer /ip4/<host>/udp/<port>/quic-v1/p2p/<peer-id>

# On another machine: fetch it by content id from that peer
cargo run -p np2ptp-node --bin np2ptp -- fetch <link> --peer /ip4/<host>/udp/<port>/quic-v1/p2p/<peer-id> --out ./restored
```

`fetch` pulls the manifest by content id, then every chunk, verifying each against the
Merkle root before writing — files and folders alike. The seeder announces itself on a
Kademlia DHT, so peers can also discover providers by content id (`find_providers`).

**Erasure-coded download:** add `--fec` to `fetch` to reconstruct from RaptorQ symbols
instead of exact chunks — any sufficiently large set of symbols rebuilds the content,
which is what lets it survive seeder churn. **Incentives:** each node keeps a reputation
ledger of bytes served/received per peer and can *choke* a peer that takes far more than
it gives back (`set_choke_threshold`).

NAT traversal (relay v2 + DCUtR hole punching) is the next milestone; today both peers
need a reachable address.

## What `np2ptp-store` gives you today

- `Store::open(dir)` — disk-backed, content-addressed, atomic writes.
- `store.ingest(data, name)` → `Manifest`, storing each chunk once (dedup).
- `store.export(&manifest)` — rebuild content from stored chunks, fully verified.
