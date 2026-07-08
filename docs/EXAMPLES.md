# Usage Examples

Worked examples beyond the [basic usage guide](USAGE.md): full CLI walkthroughs,
non-interactive (scripted) usage for integrating NP2PTP into another
application, and the public Rust API. For installation, see the
[README](../README.md).

## CLI: Linking and Downloading

`pack` accepts a single file or a whole directory tree — like a torrent, a folder
keeps its relative paths and top-level name.

```sh
# Linker: a single file
np2ptp pack myfile.bin --out myfile.nptp --store seedstore

# Linker: an entire folder (recurses; preserves subdirectories)
np2ptp pack ./myfolder --out folder.nptp --store seedstore

# Inspect a .nptp file (lists the files inside a tree, no download)
np2ptp info folder.nptp

# Client: rebuild from a seed's store on the same machine, verifying every chunk
np2ptp get folder.nptp --source seedstore --store clientstore --out ./restored
```

For a tree, `--out` is the destination directory and the structure is recreated
under it (`restored/data/blob.bin`, …). With no `--out`, files land in a folder
named after the manifest. Identical files anywhere in the tree are stored and
transferred **once** — content-defined chunking deduplicates automatically.

`--source` is an offline stand-in for a peer (another node's store directory).
For a real network transfer, see below.

### `--no-copy`: linking a file without duplicating it on disk

By default, `pack` copies every chunk into the store, so seeding a file costs
roughly double its size on disk (the original plus the store's copy). `--no-copy`
instead references the original file in place:

```sh
np2ptp pack myfile.bin --out myfile.nptp --store seedstore --no-copy
```

The tradeoff: the original file must stay at that path, unchanged, for as long as
you serve it. A moved or edited source file is caught safely (a hash-verification
error), never served as silently-wrong bytes.

## CLI: Real Network Transfer (QUIC)

```sh
# On the seeding machine: pack, then serve over the network (QUIC + DHT)
np2ptp pack ./myfolder --out folder.nptp --store seedstore
np2ptp serve folder.nptp --store seedstore
#   prints:  np2ptp fetch <link> --peer /ip4/<host>/udp/<port>/quic-v1/p2p/<peer-id>

# On another machine: fetch it by content id from that peer
np2ptp fetch <link> --peer /ip4/<host>/udp/<port>/quic-v1/p2p/<peer-id> --out ./restored
```

`fetch` pulls the manifest by content id, then every chunk, verifying each
against the Merkle root before writing. The seeder announces itself on a
Kademlia DHT and a tracker, so a peer can also discover providers without
`--peer`:

```sh
np2ptp fetch np2ptp:<content-id> --out ./restored
```

**Erasure-coded download:** add `--fec` to `fetch` to reconstruct from RaptorQ
symbols instead of exact chunks — any sufficiently large set of symbols rebuilds
the content, which is what lets it survive seeder churn.

**Behind CGNAT / no port forward?** `serve` handles it automatically by falling
back to a public relay — see [Relay Setup](RELAY.md) for how that works and how
to run your own.

## Non-Interactive Usage (`--json`)

`pack`, `get`, `fetch`, and `serve` all accept a `--json` flag that switches their
output from human-readable text to newline-delimited JSON (NDJSON) — one JSON
object per line on stdout. This is meant for driving NP2PTP from another
application (a launcher, a script, a CI job) without a terminal: spawn `np2ptp`
as a child process, read its stdout line by line, and parse each line as JSON.
No FFI or library binding is required.

Every line carries an `event` field to dispatch on and an `op` field naming the
subcommand. Progress lines are throttled to at most one every ~100ms.

```sh
np2ptp pack ./myfolder --out folder.nptp --store seedstore --json
```

```jsonc
{"event":"progress","op":"pack","bytes_done":1048576,"bytes_total":5242880}
{"event":"progress","op":"pack","bytes_done":5242880,"bytes_total":5242880}
{"event":"result","op":"pack","root":"np2ptp:e0cf...","chunks_total":70,"chunks_new":2,"bytes_total":5242880}
```

```sh
np2ptp fetch np2ptp:e0cf... --out ./restored --json
```

```jsonc
{"event":"progress","op":"fetch","phase":"downloading","chunks_done":1230,"chunks_total":36491}
{"event":"progress","op":"fetch","phase":"downloading","chunks_done":36491,"chunks_total":36491}
{"event":"progress","op":"fetch","phase":"writing","chunks_done":20000,"chunks_total":36491}
{"event":"result","op":"fetch","root":"np2ptp:e0cf...","path":"./restored","bytes_total":2400000000,"chunks_fetched":36491,"chunks_deduped":0}
```

`get` and `fetch` report two distinct progress phases: `"downloading"` (or, for
`get`, pulling from the local source store) and `"writing"` — rebuilding the
destination file from the store re-reads and re-verifies every chunk, which
can take a while on its own for large content and would otherwise look
indistinguishable from a hung process.

`serve --json` prints a periodic status line (roughly every 2 seconds) instead
of the interactive "serving …" banner, so a long-running process can be
monitored programmatically — how many peers are connected, which tracker is in
use, and aggregate bytes served/received:

```jsonc
{"event":"status","op":"serve","peers":3,"tracker":"https://nptp.bogotec.uk","bytes_served":10485760,"bytes_received":0}
```

If a command fails with `--json` set, the error is also emitted as one JSON line
on stdout (`{"event":"error","op":"fetch","message":"..."}`) in addition to the
usual text on stderr — a consumer only needs to read stdout to get everything.

## Research Harness

`np2ptp-sim` measures whether the project's core ideas — deduplication,
permanence, and reputation-based incentives — actually outperform a naive
baseline, using real nodes rather than a model.

```sh
cargo run --release -p np2ptp-sim
```

A representative run reports:

| Experiment | Result |
|---|---|
| **Dedup** — store a file, then a lightly-edited v2 | **~49%** of chunks deduplicated |
| **Permanence** — seeder leaves after one peer re-shares | survives **only with** re-sharing |
| **Free-riding** — leech under the reputation choke | choke off → completes; choke on → cut off |
| **FEC cost** — chunk vs. RaptorQ-symbol download (1 MB) | chunk ~107 ms vs. FEC ~110 ms |

Build with `--release` — RaptorQ's GF(256) arithmetic is roughly 100x slower in a
debug build. The scenario assertions run in CI (`cargo test -p np2ptp-sim`).

The FEC result is itself an optimization story: the first implementation
fetched one ~1.2 KB symbol per request (~875 round-trips per MB) and decoded
after every symbol arrived, taking roughly 25 seconds. With **symbol batching**
(128 symbols per request) and **decoding only once enough symbols have
arrived**, erasure-coded download now roughly matches plain chunk download
(~110 ms) while adding any-*k*-of-*n* resilience — so permanence comes at
essentially no cost.

## Rust API

The crates that make up NP2PTP are usable directly, without the CLI.

### `np2ptp-core`: content addressing

```rust
use np2ptp_core::Manifest;

let manifest = Manifest::from_bytes(data, Some("myfile.bin".into()));
let link = manifest.uri();                 // "np2ptp:<hex-root>"
let ok = manifest.verify_chunk(0, &chunk_bytes);
let content = manifest.reconstruct(|hash| fetch_chunk_somehow(hash));
```

### `np2ptp-store`: the content-addressed chunk store

```rust
use np2ptp_store::Store;

let store = Store::open("mystore")?;
let manifest = store.ingest(&data, Some("myfile.bin".into()))?; // dedup on write
let restored = store.export(&manifest)?;                        // fully verified
```

### `np2ptp-net`: the network layer

```rust
use np2ptp_net::Network;

let net = Network::spawn(store, None)?;
net.listen("/ip4/0.0.0.0/udp/0/quic-v1".parse()?).await?;
net.provide(&manifest).await?;
let downloaded = net.download(root, provider_peer_id, &local_store).await?;
```

See each crate's own documentation comments (`cargo doc --open -p np2ptp-net`,
etc.) for the full API surface.
