<p align="center">
  <img src="docs/assets/logo.svg" alt="NP2PTP logo" width="88">
</p>

# NP2PTP: New Peer-To-Peer Transfer Protocol

[![Release](https://github.com/LuanBogoqb/np2ptp/actions/workflows/release.yml/badge.svg)](https://github.com/LuanBogoqb/np2ptp/actions/workflows/release.yml)
[![Latest release](https://img.shields.io/github/v/release/LuanBogoqb/np2ptp)](https://github.com/LuanBogoqb/np2ptp/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Share a file or a whole folder over P2P with one command. It works behind
CGNAT with zero configuration, keeps content alive after the original seeder
leaves, gives seeders a reputation that actually matters, and can pull
existing BitTorrent content into the network. Written in Rust on top of
`rust-libp2p`, and every design claim here is measured by a simulation
harness (`np2ptp-sim`) rather than taken on faith.

## Quick Start

Grab a single-binary release for
[Windows or Linux](https://github.com/LuanBogoqb/np2ptp/releases/latest)
(no install needed), then:

```sh
# Link what you want to share (a file or an entire folder)
np2ptp pack ./my-folder --out my-folder.nptp

# Make it available. Works behind CGNAT or a closed router port:
# it detects that and falls back to a public relay on its own.
# (serve takes several .nptp files at once, or --all for everything you've packed)
np2ptp serve my-folder.nptp

# On the other side: download, verifying every chunk against a Merkle root
np2ptp fetch my-folder.nptp --out ./downloaded
```

The `.nptp` file holds only hashes (a few KB), so send it over email, Discord,
wherever. If the other side only has the link (`np2ptp:abc123...`), `fetch`
accepts that too and finds providers by itself through the DHT and tracker.
Full walkthrough in [Basic Usage](docs/USAGE.md).

## What It Fixes About Torrents

Each pain point has a shipped mechanism behind it, and a result measured by
`np2ptp-sim` using real nodes, not a model. Numbers below are from a
representative run; the scenario assertions run in CI.

| Pain point | Mechanism | Measured result |
|---|---|---|
| Peers behind NAT/CGNAT can't connect | QUIC hole punching + automatic relay fallback | `serve` works behind CGNAT with no configuration |
| Content dies when seeders leave | RaptorQ erasure coding: any sufficiently large set of symbols rebuilds the content | FEC download ~110 ms vs ~107 ms plain (1 MB), so resilience costs almost nothing |
| Seeding earns nothing | Ed25519 identities, signed receipts, reputation-based choking | free-rider with choke on: cut off; unknown peer vouched by a signed receipt: completes |
| Coarse verification, no dedup | BLAKE3 Merkle trees + content-defined chunking shared across contents | ~49% of chunks deduplicated between a file and a lightly edited v2 |

Out of scope for now: privacy/anonymity, streaming, mutable content.

## Bring Your Torrents With You

`np2ptp-bridge` is a two-way gateway to BitTorrent. Data you already
downloaded with a regular BitTorrent client can be bridged in place, verified
against the torrent's own piece hashes (streamed from disk, so a 50+ GB
torrent never touches RAM):

```sh
np2ptp torrent my-linux-iso.torrent --data ~/Downloads/my-linux-iso
```

If another peer already bridged that exact torrent (matched by infohash),
your copy is fetched from NP2PTP directly instead of being re-verified. And
if you don't have the data at all, a build with `--features librqbit` will
download it from the BitTorrent swarm first, then bridge it the same way:

```sh
np2ptp torrent <magnet-link-or-.torrent-or-url>
```

## Scripting and Embedding

`pack`, `get`, `fetch`, and `serve` all take `--json` and emit
newline-delimited JSON on stdout (progress, results, errors, periodic `serve`
status). Driving NP2PTP from a launcher, script, or CI job needs no FFI, just
a child process and a line parser. Details in
[Usage Examples](docs/EXAMPLES.md#non-interactive-usage---json), which also
covers the public Rust API.

For an app that stays open, `np2ptp daemon` is the better fit. One long-lived
process owns the store, the identity, and the network connection, and takes
commands as JSON lines on stdin:

```sh
np2ptp daemon --store ~/.np2ptp
```

```jsonc
// in:
{"id":1,"cmd":"fetch","uri":"np2ptp:abc...","out":"./downloads/game"}
// out:
{"event":"ready","version":"0.1.8","peer_id":"12D3KooWSzXt...","addrs":["/ip4/192.168.1.10/tcp/4001"]}
{"id":1,"event":"progress","op":"fetch","done":42,"total":900}
{"id":1,"event":"result","ok":true,"root":"np2ptp:abc..."}
```

Other commands: `convert` (bridge a downloaded torrent, or pack a folder),
`torrent` (download over BitTorrent and bridge it on the way in), `provide`
and `unprovide` to start and stop seeding without a restart, `status`, and
`shutdown`. Every event carries the `id` of the request it belongs to, so
several operations can run at once. A malformed line gets an error event and
the daemon keeps going.

Because one process holds the identity across restarts, the reputation a
seeder earns accumulates instead of resetting. Running several `serve`
processes against one store cannot do that.

## Staying Up to Date

`np2ptp update` checks the latest GitHub release and, if it's newer, downloads
and verifies it (Authenticode pin on Windows, `SHA256SUMS` on Linux) before
swapping it in next to the running binary. Nothing is installed on a failed
check: a bad signature deletes the download and leaves the current binary
alone.

```sh
np2ptp update
# already up to date (0.1.9)
# or: updated 0.1.8 -> 0.1.9, restart to use it
```

`np2ptp daemon` does the same check on every start, unless you pass
`--no-auto-update`. If it finds and installs a newer binary, it tells you
about it with an `updated` event (`{"event":"updated","from":"0.1.8","to":"0.1.9"}`)
instead of restarting itself; the new code takes effect the next time the
daemon (or whatever embeds it) starts.

## Design in One Paragraph

Do not reinvent the plumbing. Build on `rust-libp2p` (QUIC transport,
key-based identity, Noise, Kademlia DHT, NAT traversal, gossip). The novelty
lives in the layers above: content addressing with BLAKE3 and Merkle trees,
content-defined chunking for cross-content deduplication, RaptorQ erasure
coding for permanence, and a persistent reputation ledger for incentives. On
top of that, the simulation harness measures whether any of it actually beats
a baseline (see [Research Harness](docs/EXAMPLES.md#research-harness)).

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
| `np2ptp-bridge` | BitTorrent gateway: convert an already-downloaded torrent, or fetch one you don't have yet (`np2ptp torrent`) |

There is also a small **tracker**: BitTorrent-tracker-style peer discovery
over plain HTTP, self-hostable. See [`tracker/README.md`](tracker/README.md).
For running your own relay/bootstrap node (needed behind CGNAT), see
[Relay Setup](docs/RELAY.md).

## Documentation

| Section | Description |
|---|---|
| [Basic Usage](docs/USAGE.md) | Install and run your first pack / serve / fetch in a few minutes. |
| [Usage Examples](docs/EXAMPLES.md) | Real network transfers, non-interactive (`--json`) usage for scripting, and the Rust API. |
| [Building from Source](docs/BUILDING.md) | Compiling the workspace and running the test suite. |
| [Download Prebuilt Binaries](https://github.com/LuanBogoqb/np2ptp/releases/latest) | Linux and Windows binaries, no toolchain required. |

## Verifying the Windows Binary

The Windows release binary is Authenticode-signed (SHA-256, timestamped) as
part of the [release workflow](.github/workflows/release.yml). The
certificate is a personal one, not an EV certificate from a large commercial
CA, so Windows SmartScreen may still show an "unrecognized publisher" warning
the first few times someone runs it. That is SmartScreen's reputation system
still catching up, not a sign the signature is invalid or the file was
tampered with.

To check the signature yourself, either right-click the `.exe`, open
Properties, and look at the Digital Signatures tab, or in PowerShell:

```powershell
Get-AuthenticodeSignature .\np2ptp-windows-x86_64.exe | Format-List *
```

`Status` should read `Valid`, and the signer should match:

```
Subject:    CN=Luan Bogo, E=LuanBogoqb@users.noreply.github.com, C=BR
Thumbprint: 36477BB5DCB10D2C0381A2D79533F0386C5CCACA
```

The thumbprint changes whenever the certificate is renewed. The `Subject` is
what stays stable across renewals, so treat that as the primary check.

## License

Released under the MIT License. You can use, modify, and redistribute it,
including in commercial work, as long as the copyright notice stays intact.
Full text in [LICENSE](LICENSE).
