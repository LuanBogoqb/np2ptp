# NP2PTP Roadmap

A guide for whoever (human or AI) continues this project. It captures the vision,
what exists, the architecture, the non-obvious gotchas, and the concrete next steps.

---

## 1. Vision

NP2PTP ("New Peer-To-Peer Transfer Protocol") is inspired by BitTorrent: it
keeps what BitTorrent does well and fixes what it does badly. The user's
priorities, in order:

1. **NAT / connectivity** — most peers can't accept inbound connections.
2. **Permanence / incentives** — content dies when seeders leave; seeding is unrewarded.
3. **Integrity / dedup** — coarse verification, no cross-content deduplication.

Changes are checked against a measurement harness (see `np2ptp-sim`) rather
than taken on faith. Out of scope (for now): privacy/anonymity, streaming
playback, mutable content.

Guiding principle: **don't reinvent the plumbing.** Build on `libp2p` (QUIC,
Noise, Kademlia, NAT traversal). The novelty is the layers above.

---

## 2. Current state (what works)

8 Rust crates + a Python CLI wrapper. ~100 tests green, clippy clean. Pushed to
`github.com/LuGB18/np2ptp` (private).

| Crate | Status | Responsibility |
|---|---|---|
| `np2ptp-core` | ✅ | FastCDC chunking, BLAKE3 + Merkle, multi-file manifests, `.nptp` format |
| `np2ptp-store` | ✅ | Content-addressed chunk store + dedup; **streaming** ingest/export |
| `np2ptp-fec` | ✅ | RaptorQ erasure coding (k-of-n) |
| `np2ptp-rep` | ✅ | Ed25519 identity, signed receipts, reputation `Ledger<K>` |
| `np2ptp-net` | ✅ (core) | libp2p/QUIC: download by content id, DHT discovery + infohash mapping, reputation choke + **signed receipt exchange** (portable reputation), FEC symbols, relay reservation |
| `np2ptp-node` | ✅ | CLI: `pack` / `info` / `get` / `serve` / `fetch` (files **and** folders, streaming) |
| `np2ptp-sim` | ✅ | Research harness: measures dedup, permanence, free-riding, FEC cost; writes `reports/` |
| `np2ptp-bridge` | ✅ | Torrent↔NP2PTP: `TorrentSource` trait, resolve-or-convert flow, piece verification, streaming local conversion, and `LibrqbitSource` (remote magnet/`.torrent` fetch) behind the `librqbit` feature. |
| `python/np2ptp.py` | ✅ | Friendly wrapper over the binary (host:port instead of multiaddr) |

**Validated for real:**
- Machine-to-machine transfer over QUIC (two laptops, Tailscale/LAN) — file & folder.
- Download via plain chunks and via RaptorQ symbols (`--fec`).
- DHT provider discovery + infohash→root mapping (tests).
- Reputation choke cutting off a leech (test + sim).
- Signed receipts: a peer that earned one serving someone else bypasses a
  strict choke on first contact with a brand-new peer, while an equally cold
  peer with no receipt is choked (test + sim). Reputation now persists across
  `serve` restarts (`ledger.bin`/`receipts.bin` next to `identity.key`).
- Streaming pack of a real **3.1 GB** file (no OOM); streaming root == in-memory root.

---

## 3. Architecture

**Content identity.** Content is split by **FastCDC** (content-defined chunking,
fixed params in `core::chunk`), each chunk hashed with **BLAKE3**. A **Merkle root**
over the chunk hashes is the **content id**. A share link is `np2ptp:<hex-root>`.
Files are chunked *independently in file order* (a chunk boundary is forced at each
file edge), so identical files dedup regardless of directory layout, and the root
depends only on file contents + order (not paths).

**Manifest (`.nptp`).** `core::Manifest` = `{ root, total_size, chunks: [ChunkRef],
files: [FileEntry], name }`. `ChunkRef = {hash, offset, length}`. `FileEntry =
{path, size, chunk_start, chunk_count}`. Serialized with a `NPTP` magic header.
This is the "torrent file" equivalent.

**Store.** Content-addressed: chunk bytes written to `objects/<aa>/<hex>` (atomic
temp+rename), so identical chunks are stored once. Streaming variants chunk from /
write to disk one chunk at a time.

**Network (`net::Network`).** A handle that drives a background tokio task owning a
libp2p `Swarm`. Transport = **QUIC**. Behaviours: Kademlia (provider records +
`put/get_record` for the bridge mapping), request-response (manifest / chunk /
RaptorQ-symbol, CBOR codec), identify, relay (client+server), dcutr, autonat. The
headline op is `download(root, provider, into_store)`: fetch manifest, validate it
against the root, then fetch+verify+store every chunk (concurrently, streaming).

**Incentives.** `rep::Ledger<K>` tracks bytes served/received per peer, plus
bytes credited by valid signed receipts. `net` embeds `Ledger<PeerId>` and a
choke threshold: a peer that has taken far more than it gave (net of any
receipt credit) is refused chunks. After a download, the client signs one
`Receipt` crediting the server and sends it over the wire (`SubmitReceipt`);
a server persists received receipts (`receipts.bin`, capped at 50) and, on
first contact with a peer it has no ledger history for, pulls that peer's
bag (`GetReceipts`) and credits it — reputation that survives restarts and
travels to peers you've never talked to directly, unlike BitTorrent's
memoryless tit-for-tat. This is *not* Sybil-resistant (a receipt only proves
a key signed it, not that the signer is a distinct real peer) — see
`docs/superpowers/specs/2026-07-08-signed-receipt-exchange-design.md`'s
"Trust model / limitations" for what it does and doesn't defend against.

**Research harness (`sim`).** Spins up real `Network` nodes and runs A/B scenarios,
writing `reports/REPORT.md` + `results.csv`.

---

## 4. Gotchas (read before you code)

- **Toolchain (Windows):** MSVC. `cargo` via `~\.cargo\bin` (may need PATH prefix
  mid-session). Build `--release` for anything touching RaptorQ or large content.
- **Determinism:** streaming and in-memory chunking MUST agree (test enforces it).
  Don't change FastCDC params casually — it changes every content id.
- **O(n²) trap:** `Manifest::verify_chunk` rebuilds the whole Merkle tree per call.
  In loops use `root_is_consistent()` once + `chunk_hash_ok(i, bytes)` per chunk.
- **Memory:** real content is 10s of GB. Always use the streaming store/download
  paths. The in-memory `ingest`/`export`/`export_tree` exist for small data & tests.
- **Kademlia:** server mode is set in `Network::spawn`. `put_record` (Quorum::One)
  needs a reachable remote peer — a lone node can't publish to itself.
- **Relay:** a relay node must call `add_external_address(its_listen_addr)` or the
  reservations it grants are address-less and clients reject them
  (`NoAddressesInReservation`). Connect to the relay BEFORE listening on the
  circuit. **Relayed *data transfer* over QUIC is flaky on loopback** — the
  reservation works, but the relayed stream tears down; needs real NATs to validate
  (`download_through_a_relay` is `#[ignore]`d).
- **Tailscale is a TEST crutch, not the answer.** Real NAT story = UPnP/NAT-PMP +
  DCUtR + a public relay fallback (see Phase 2). Requiring a VPN would kill adoption.
- **Bridge determinism:** convert torrents by chunking files in the **torrent's
  file order**, so two converters of the same torrent produce the same root.
- **PowerShell:** the sandbox blocks `Remove-Item` on `D:\` and on commands
  containing regex-like literals (`\S+`). Pass git commit bodies via `-F <file>`
  (quotes get mangled in `-m`). `2>&1` on native commands wraps stderr as errors.

---

## 5. Roadmap

### ✅ Phase 0 — Foundations (done)
Content addressing, dedup store, FEC, reputation primitives.

### ✅ Phase 1 — Networking core + scale (done)
QUIC transport, DHT discovery, end-to-end download, `serve`/`fetch` CLI, FEC over
the wire, reputation choke, research harness, and **streaming** for large content.

### 🚧 Phase 2 — Torrent bridge + automatic peer discovery (next)
Goal: "drop a `.torrent`/magnet (or link) and it just works", like a torrent.

1. **Finish the bridge** (`np2ptp-bridge`):
   - ✅ **Local conversion** — `parse_torrent_file` (bencode → infohash, file list,
     piece hashes) + `resolve_or_convert_local`/`convert_local`, streaming both
     piece verification and ingestion from disk (never the whole torrent in RAM).
     `np2ptp torrent <file.torrent> --data <dir>` CLI command.
   - ✅ **`LibrqbitSource`** — downloads a torrent/magnet you *don't* have yet
     (behind the `librqbit` feature) straight to disk, then feeds it through
     the same streaming `resolve_or_convert_local` path as an already-
     downloaded torrent (never the whole thing in RAM). `np2ptp torrent
     <file.torrent|magnet:...>` (no `--data`) drives it. Validated end-to-end
     with a real BitTorrent peer-wire download (seeder + downloader via
     direct peer injection, no DHT/tracker dependency — deterministic and
     fast); a live public-swarm/magnet-DHT run hasn't been done yet.
2. **Automatic discovery** (so `fetch <link>` needs no `--peer`):
   - ✅ **HTTP discovery tracker** — LIVE at `https://nptp.bogotec.uk`, self-hosted
     on the VPS (`tracker/`, systemd + Caddy). `serve` announces; `fetch <link>`
     with no `--peer` discovers providers and downloads. Validated end-to-end.
   - **mDNS** — libp2p mDNS behaviour for zero-config discovery on the same LAN.
   - **Bootstrap DHT nodes** — run 1+ stable nodes (persist the Ed25519 key for a
     fixed peer id) so `find_providers(root)` works without the tracker too.
   - Wire the `torrent` command to use discovery as well.
3. **NAT without a VPN** (the real adoption unlock):
   - **UPnP / NAT-PMP** — libp2p port-mapping behaviour so the node auto-opens a
     router port and becomes publicly reachable (this is *the* big win; how torrents
     "just work").
   - Finish **DCUtR + relay** (debug the relayed-QUIC teardown on real NATs).
   - Run a public **relay** as the always-works fallback.

### ⏳ Phase 3 — Hardening & performance
- **Store performance:** packing 3 GB took ~219 s (~15 MB/s) because every chunk is
  a separate small file. Consider packfiles / larger avg chunk / batched writes.
- **No-copy / streaming bridge:** avoid duplicating 51 GB into the store; verify
  pieces by streaming from disk.
- **FEC permanence for real:** today only a full holder can mint symbols. Store and
  forward *partial* symbol sets across peers for true churn resilience.
- **Resumable / multi-source downloads:** fetch chunks from several providers at once.
- ✅ **Signed-receipt exchange over the wire** — done. `SubmitReceipt`/`GetReceipts`
  ride the existing request-response protocol; see "Incentives" above. Remaining:
  per-peer state (`rep_peers`, `receipts_pulled_from`) has no GC on disconnect —
  fine for now, worth pruning if a long-lived node sees heavy peer churn.
- **Mutable content:** signed pointers (a key "names" a feed) — IPNS/Dat style.

### ⏳ Phase 4 — Product & UX
Better CLI ergonomics, packaging/distribution of the binary, maybe a GUI, public
bootstrap/relay infrastructure, docs site.

---

## 6. How to build, test, run

```sh
cargo test --workspace                     # keep green (~100 tests)
cargo clippy --workspace --all-targets     # keep at 0 warnings
cargo test -p np2ptp-bridge --features librqbit   # + the real-BitTorrent-download test
cargo run --release -p np2ptp-sim          # research report -> reports/

# CLI (build once: cargo build --release -p np2ptp-node)
np2ptp pack ./folder --out f.nptp --store seedstore
np2ptp serve f.nptp --store seedstore --listen /ip4/0.0.0.0/udp/4001/quic-v1
np2ptp fetch f.nptp --peer /ip4/<host>/udp/4001/quic-v1/p2p/<id> --out got [--fec]
np2ptp torrent 'magnet:?xt=urn:btih:...' --store seedstore   # needs --features librqbit
# or via Python: python python/np2ptp.py fetch f.nptp --peer <host>:4001 --id <id> --out got
```

## 7. Where to look

- Chunking / Merkle / manifest / `.nptp`: `crates/np2ptp-core/src/{chunk,hash,manifest}.rs`
- Store + streaming: `crates/np2ptp-store/src/lib.rs`
- Network protocol, DHT, download, choke, FEC, relay: `crates/np2ptp-net/src/lib.rs`
- CLI: `crates/np2ptp-node/src/main.rs`; reusable bits in `.../src/lib.rs`
- Bridge: `crates/np2ptp-bridge/src/lib.rs`; remote fetch: `.../src/librqbit_source.rs`
- Research scenarios: `crates/np2ptp-sim/src/lib.rs`
