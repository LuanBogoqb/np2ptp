# NP2PTP Python client

A thin Python wrapper around the `np2ptp` Rust binary. It drives the real Rust
engine (so it speaks the exact protocol) while hiding the rough edges — it finds
the binary for you and lets you pass `host:port` + a peer id instead of a libp2p
multiaddr.

## Requirements

- Python 3.8+
- The `np2ptp` binary. The wrapper looks for it in this order:
  1. `$NP2PTP_BIN` (set this to the binary's path on machines without the repo)
  2. `../target/release/np2ptp[.exe]` then `../target/debug/...` (relative to the repo)
  3. `np2ptp` on `PATH`

Build it once with: `cargo build --release -p np2ptp-node`

## CLI

```sh
# Pack a file or folder, write a .nptp and populate a store
python np2ptp.py pack ./myfolder --out my.nptp --store seedstore

# Seed it on the network (prints ready-to-run fetch commands; Ctrl-C to stop)
python np2ptp.py serve my.nptp --store seedstore --port 4001

# On another machine: fetch by host:port + peer id (no multiaddr needed)
python np2ptp.py fetch my.nptp --peer 100.110.12.28:4001 --id 12D3Koo... --out got

# Inspect a .nptp
python np2ptp.py info my.nptp
```

`fetch` also accepts a full multiaddr in `--peer` (then `--id` is optional), and
`--fec` to reconstruct via RaptorQ symbols.

## Library

```python
import np2ptp

c = np2ptp.Client()                 # or Client(binary="/path/to/np2ptp")
print(c.info("my.nptp"))            # -> dict of manifest fields
print(c.pack("./myfolder", out="my.nptp", store="seedstore")["link"])
c.fetch("my.nptp", peer="100.110.12.28:4001", peer_id="12D3Koo...", out="got")
```

## On a second machine (no repo)

Copy `np2ptp.py` and the `np2ptp` binary over, then:

```sh
set NP2PTP_BIN=C:\path\to\np2ptp.exe      # Windows (or `export` on Linux/macOS)
python np2ptp.py fetch my.nptp --peer <host>:<port> --id <peer-id> --out got
```
