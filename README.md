<p align="center">
  <img src="docs/assets/logo.svg" alt="NP2PTP logo" width="88">
</p>

# NP2PTP: New Peer-To-Peer Transfer Protocol

[![Release](https://github.com/LuanBogoqb/np2ptp/actions/workflows/release.yml/badge.svg)](https://github.com/LuanBogoqb/np2ptp/actions/workflows/release.yml)
[![Latest release](https://img.shields.io/github/v/release/LuanBogoqb/np2ptp)](https://github.com/LuanBogoqb/np2ptp/releases/latest)

A peer-to-peer transfer protocol inspired by BitTorrent: keeps what torrents do
well, fixes what they don't. Every change is checked against a measurement
harness (`np2ptp-sim`) rather than taken on faith.

## Pain Points Being Targeted

1. **NAT / connectivity**: too many peers cannot accept inbound connections.
2. **Permanence / incentives**: content dies when seeders leave, and seeding earns nothing.
3. **Integrity / dedup**: coarse verification and no cross-content deduplication.

Out of scope for now: privacy/anonymity, streaming, mutable content.

## Design in One Paragraph

Do not reinvent the plumbing. Build on `rust-libp2p` (QUIC transport, key-based
identity, Noise, Kademlia DHT, NAT traversal, gossip). The novelty lives in the
layers above: content addressing with BLAKE3 and Merkle trees, content-defined
chunking for cross-content deduplication, RaptorQ erasure coding for permanence,
and a persistent reputation ledger for incentives. On top of that, a simulation
harness measures whether any of it actually beats a baseline (see
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
| `np2ptp-bridge` | BitTorrent ↔ NP2PTP gateway: convert an already-downloaded torrent, or fetch one you don't have yet (`np2ptp torrent`) |

There is also a small **tracker**: BitTorrent-tracker-style peer discovery over
plain HTTP, self-hostable. See [`tracker/README.md`](tracker/README.md). For
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

## Verifying the Windows Binary

The Windows release binary is Authenticode-signed (SHA-256, timestamped) as
part of the [release workflow](.github/workflows/release.yml). The
certificate is a personal one, not an EV certificate from a large commercial
CA, so Windows SmartScreen may still show an "unrecognized publisher" warning
the first few times someone runs it — that's SmartScreen's reputation system
still catching up, not a sign the signature is invalid or the file was
tampered with.

To check the signature yourself, either right-click the `.exe` → Properties →
Digital Signatures tab, or in PowerShell:

```powershell
Get-AuthenticodeSignature .\np2ptp-windows-x86_64.exe | Format-List *
```

`Status` should read `Valid`, and the signer should match:

```
Subject:    CN=Luan Bogo, E=LuanBogoqb@users.noreply.github.com, C=BR
Thumbprint: 36477BB5DCB10D2C0381A2D79533F0386C5CCACA
```

The thumbprint changes whenever the certificate is renewed — the `Subject`
is what stays stable across renewals, so treat that as the primary check.
