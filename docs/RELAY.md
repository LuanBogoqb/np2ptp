# Running an NP2PTP relay / bootstrap node

A relay is a node on a host with a **public IP and an open UDP port**. Peers behind
CGNAT (or broken IPv6) can't reach each other directly, but they can both make
**outbound** connections to the relay, which forwards traffic between them. It also
acts as a DHT bootstrap. This is the "central node" that makes NP2PTP work on any
network without a VPN.

## The defaults that ship in the binary

These are compiled in, and this doc never said which ones they are:

| Default | Value | Where |
|---|---|---|
| relay / bootstrap | `/ip4/194.163.191.81/udp/4001/quic-v1/p2p/12D3KooWSzXtDVLLFf2avw9bpcMCRsE7JvbdQNEcd45MKuRsGmyR` | `crates/np2ptp-node/src/main.rs:33` (`DEFAULT_RELAY`) |
| tracker | `https://nptp.bogotec.uk` | `crates/np2ptp-node/src/tracker.rs:13` (`DEFAULT_TRACKER`) |

Both are overridable by environment: `NP2PTP_RELAY` and `NP2PTP_TRACKER`.

**Measured on 2026-09-06, because "the default points at a dead box" had been assumed for a while
and it is only half true:** `194.163.191.81` answers ICMP (~204 ms) and serves HTTP 200 on port 80,
so the host is **up** — whether the relay is listening on UDP/4001 cannot be probed from a client
without a QUIC handshake. The tracker `nptp.bogotec.uk` resolves to Cloudflare
(`104.21.67.72` / `172.67.217.130`) and returns **502** on `/`, `/announce` and `/health`, so its
origin is down. Anything relying on peer discovery through the default tracker fails today.

## What you need from the host
- A **public IP** (e.g. `209.126.4.74`) — confirm it's NOT in the CGNAT range
  (`100.64.x`–`100.127.x`).
- **One open UDP port** reachable from the internet (e.g. `4001`). If the VM sits
  behind the host's NAT, port-forward `UDP 4001` from the public IP to the VM.
- Ubuntu Server 22.04+. **≥ 2 GB RAM to *build*** (the relay *runs* on ~128 MB; only
  compiling libp2p is memory-hungry — or build elsewhere and copy the binary).

## Set up (on the Ubuntu VM)

```sh
# 1. Build dependencies (libp2p's QUIC pulls C crypto)
sudo apt update
sudo apt install -y build-essential pkg-config cmake clang git curl

# 2. Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"

# 3. Build the binary (or just grab a prebuilt one from Releases instead)
git clone https://github.com/LuanBogoqb/np2ptp.git
cd np2ptp
cargo build --release -p np2ptp-node

# 4. Open the UDP port (and the cloud/host firewall too, if any)
sudo ufw allow 4001/udp

# 5. Run the relay (advertise the public IP)
./target/release/np2ptp relay --public 209.126.4.74 --listen /ip4/0.0.0.0/udp/4001/quic-v1
```

It prints a stable peer id (saved in `relay.key`, reused on restart) and the line
clients use:

```
clients use:   --relay /ip4/209.126.4.74/udp/4001/quic-v1/p2p/<relay-peer-id>
```

## Keep it running (systemd)

`/etc/systemd/system/np2ptp-relay.service`:

```ini
[Unit]
Description=NP2PTP relay
After=network-online.target

[Service]
ExecStart=/home/USER/np2ptp/target/release/np2ptp relay --public 209.126.4.74 --listen /ip4/0.0.0.0/udp/4001/quic-v1
WorkingDirectory=/home/USER/np2ptp
Restart=always
User=USER

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl enable --now np2ptp-relay
journalctl -u np2ptp-relay -f      # watch logs
```

## Then, on the clients

This is done — `serve` finds this relay on its own. A CGNAT seed just runs
`serve <file.nptp>` with no flags: it tries `--public`/UPnP/NAT-PMP first, and
if none of those work, it dials the relay (`DEFAULT_RELAY`, or `--relay <addr>`
to point at a different one) and reserves a circuit automatically. A fetcher just
runs `fetch <link>` — no `--relay` needed on that side, it dials whatever address
the tracker/DHT hands back, circuit or not. See [Validation Status](#validation-status)
below for how this was confirmed against a real CGNAT host.

## Validation Status

The relay (v2), DCUtR, and AutoNAT behaviours are wired in. On a single
development machine, a behind-NAT node successfully reserves a slot on a relay
and gets a dialable `/…/p2p-circuit/p2p/<peer>` address (covered by a passing
test), and a *full content download through the relay* now passes reliably on
loopback too. That test used to be `#[ignore]`d as "flaky, needs real NATs" —
the actual cause turned out to be `relay::Config::default()`'s 128 KiB circuit
cap, not loopback or NAT (see `np2ptp-net/tests/relay.rs`'s module doc). DCUtR
hole-punching itself still has nothing to punch through on `127.0.0.1` and is
only exercised for real against an actual NAT (below).

That path **has been validated by hand against a real NAT**: a Windows host
behind CGNAT (a Mikrotik router, no UPnP, no port forward) served content that a
separate machine downloaded end-to-end through this public relay, with the
downloaded bytes verified identical to the source. `serve` automates the whole
sequence:

1. Try `--public` (manual override), then UPnP, then NAT-PMP/PCP.
2. If none of those produce a reachable external address, dial the public relay
   (`DEFAULT_RELAY`) on its own, reserve a circuit, and announce that circuit
   address to the tracker/DHT — no flags needed. `--relay <multiaddr>` forces a
   specific relay; `--no-relay` disables the fallback.
3. `serve` also persists its identity per `--store` directory (`identity.key`)
   instead of generating a new peer id on every restart, so a seeder that
   restarts does not strand every existing reference to it.

The relay's per-circuit limits (how much data and how long a single relayed
connection may carry) are configured in `relay_config()` in
`crates/np2ptp-net/src/lib.rs` — the `libp2p-relay` defaults are sized for
signaling traffic, not file transfer, and are raised accordingly.
