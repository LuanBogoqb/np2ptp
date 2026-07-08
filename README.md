# NP2PTP — New Peer-To-Peer Transfer Protocol

[![Release](https://github.com/LuanBogoqb/np2ptp/actions/workflows/release.yml/badge.svg)](https://github.com/LuanBogoqb/np2ptp/actions/workflows/release.yml)
[![Latest release](https://img.shields.io/github/v/release/LuanBogoqb/np2ptp)](https://github.com/LuanBogoqb/np2ptp/releases/latest)

A research prototype exploring a "BitTorrent 2.0": fix what torrents do badly and
improve what they already do well. The goal is **measurable** experiments, not a
production client.

## Pain Points Being Targeted

1. **NAT / connectivity** — too many peers cannot accept inbound connections.
2. **Permanence / incentives** — content dies when seeders leave; seeding is unrewarded.
3. **Integrity / dedup** — coarse verification and no cross-content deduplication.

Out of scope for the MVP: privacy/anonymity, streaming, mutable content.

## Design in One Paragraph

Do not reinvent the plumbing. Build on `rust-libp2p` (QUIC transport, key-based
identity, Noise, Kademlia DHT, NAT traversal, gossip). The novelty lives in the
layers above: content addressing with BLAKE3 and Merkle trees, content-defined
chunking for cross-content deduplication, RaptorQ erasure coding for permanence,
and a persistent reputation ledger for incentives — plus a simulation harness
that measures whether any of it actually beats a baseline (see
[Usage Examples](docs/EXAMPLES.md#research-harness)).

## Crates

| Crate           | Responsibility                                                       |
|-----------------|-----------------------------------------------------------------------|
| `np2ptp-core`   | Content-defined chunking, BLAKE3 hashing, Merkle trees, `.nptp` format |
| `np2ptp-store`  | Content-addressed chunk store with cross-content dedup                |
| `np2ptp-fec`    | RaptorQ erasure coding (k-of-n recovery)                               |
| `np2ptp-node`   | `.nptp` linker (`pack`) and client CLI (`get` / `info` / `serve` / `fetch`) |
| `np2ptp-rep`    | Ed25519 identity, signed receipts, reputation ledger                  |
| `np2ptp-net`    | libp2p/QUIC transport, DHT discovery, reputation choke, relay/NAT traversal |
| `np2ptp-sim`    | Research harness measuring dedup, permanence, free-riding, FEC cost    |
| `np2ptp-bridge` | BitTorrent ↔ NP2PTP gateway (core logic only — see its own docs)      |

There is also a small **tracker** — BitTorrent-tracker-style peer discovery over
plain HTTP, self-hostable — see [`tracker/README.md`](tracker/README.md). For
running your own relay/bootstrap node (needed behind CGNAT), see
[Relay Setup](docs/RELAY.md).

## Documentation

| Section | Description |
|---|---|
| [Basic Usage](docs/USAGE.md) | Install and run your first pack / serve / fetch in a few minutes. |
| [Usage Examples](docs/EXAMPLES.md) | Real network transfers, non-interactive (`--json`) usage for scripting, and the Rust API. |
| [Building from Source](docs/BUILDING.md) | Compiling the workspace and running the test suite. |
| [Download Prebuilt Binaries](https://github.com/LuanBogoqb/np2ptp/releases/latest) | Linux and Windows binaries, no toolchain required. |

Additional references: [Relay Setup](docs/RELAY.md) (running your own
relay/bootstrap node) and [`tracker/README.md`](tracker/README.md) (the
self-hosted discovery tracker).
