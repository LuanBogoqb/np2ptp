# NP2PTP tracker

A tiny **discovery tracker** for NP2PTP. It is not a P2P node: it only
exchanges contact info, BitTorrent-tracker style. The actual transfer stays
peer-to-peer.

## API

- `POST /announce`: body `{ "cid": "<nptp-root-or-infohash>", "peer": "<peer-id>", "addr": "<multiaddr>" }`
- `GET  /peers?cid=<content-id>`: `{ cid, count, peers: [{ peer, addr, ts }] }`
- `GET  /health`: `{ ok, backend: "upstash" | "memory" }`

Announcements expire after 30 min (peers must re-announce, like a tracker interval).

## State

The store falls back automatically depending on what's configured:

- If `UPSTASH_REDIS_REST_URL`/`UPSTASH_REDIS_REST_TOKEN` (or
  `KV_REST_API_URL`/`KV_REST_API_TOKEN`) are set, it uses Upstash Redis.
- Otherwise it uses an in-process `Map`. That's fine for a single always-on
  instance (which is how the live tracker runs); it just means a service
  restart clears the current announcements, and peers naturally re-announce
  within the 30-minute window.

## Deploy (self-hosted, systemd)

The tracker is plain Node with no framework dependency, so it runs anywhere
Node runs. `server.js` is a small `http` wrapper around the same handler
functions in `api/`.

1. Copy this directory to the host, e.g. `/opt/np2ptp-tracker`, and
   `npm install --omit=dev` there.
2. Create a systemd unit, e.g. `/etc/systemd/system/np2ptp-tracker.service`:

   ```ini
   [Unit]
   Description=NP2PTP discovery tracker
   After=network.target

   [Service]
   User=nptptracker
   Group=nptptracker
   WorkingDirectory=/opt/np2ptp-tracker
   Environment=PORT=8787
   Environment=HOST=127.0.0.1
   ExecStart=/usr/bin/node server.js
   Restart=on-failure
   RestartSec=3
   NoNewPrivileges=true
   PrivateTmp=true

   [Install]
   WantedBy=multi-user.target
   ```

3. `systemctl enable --now np2ptp-tracker`.
4. Put a reverse proxy in front of it for TLS. The live instance uses Caddy:

   ```
   nptp.example.com {
       reverse_proxy 127.0.0.1:8787
   }
   ```

5. Check `https://<your-domain>/health`.

## How the NP2PTP client uses it

- On `serve`: `POST /announce { cid: <root>, peer: <peer-id>, addr: <public multiaddr> }`.
- On `fetch <link>` with no `--peer`: `GET /peers?cid=<root>`, then dial those peers.

Wired in `crates/np2ptp-node/src/tracker.rs`; both `serve` and `fetch` take a
`--tracker <url>` flag. Reachability across NATs still needs UPnP/relay; the
tracker only solves *discovery*.

## Live instance

The principal tracker is **https://nptp.bogotec.uk**, self-hosted on the same
VPS as the public relay. This is `tracker::DEFAULT_TRACKER`, so clients use it
automatically unless `--tracker` overrides it.
