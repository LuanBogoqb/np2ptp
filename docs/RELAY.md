# Running an NP2PTP relay / bootstrap node

A relay is a node on a host with a **public IP and an open UDP port**. Peers behind
CGNAT (or broken IPv6) can't reach each other directly, but they can both make
**outbound** connections to the relay, which forwards traffic between them. It also
acts as a DHT bootstrap. This is the "central node" that makes NP2PTP work on any
network without a VPN.

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

# 3. Build the binary
git clone https://github.com/LuGB18/np2ptp.git
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
Client-side `--relay` wiring (make a reservation when seeding, dial circuit
addresses when fetching) is the next step — see ../ROADMAP.md Phase 2. Once that's
in, a CGNAT seed runs `serve --relay <addr>` and a CGNAT fetcher just runs
`fetch <link>` and reaches it through the relay.
