# NP2PTP tracker (Vercel)

A tiny serverless **discovery tracker** for NP2PTP. It is **not** a P2P node —
Vercel can't run libp2p/QUIC (serverless = short-lived HTTP, no persistent UDP).
It only exchanges contact info, BitTorrent-tracker style; the actual transfer
stays peer-to-peer.

## API

- `POST /announce` — body `{ "cid": "<nptp-root-or-infohash>", "peer": "<peer-id>", "addr": "<multiaddr>" }`
- `GET  /peers?cid=<content-id>` — `{ cid, count, peers: [{ peer, addr, ts }] }`
- `GET  /health` — `{ ok, backend: "upstash" | "memory" }`

Announcements expire after 30 min (peers must re-announce, like a tracker interval).

## State

Uses **Upstash Redis** (Vercel KV) when configured, via either
`UPSTASH_REDIS_REST_URL`/`UPSTASH_REDIS_REST_TOKEN` or `KV_REST_API_URL`/`KV_REST_API_TOKEN`.
Without it, falls back to an in-memory map (works only within one warm instance —
fine for a first test, not for production).

## Deploy

### Easiest — Vercel Git import (no local tooling)
1. vercel.com → **Add New… → Project** → import `LuGB18/np2ptp`.
2. Set **Root Directory** to `tracker`.
3. Deploy. It works immediately on the in-memory fallback.
4. For real state: project → **Storage → Marketplace → Upstash for Redis** (free
   tier) → connect. It sets the env vars automatically. **Redeploy.**
5. Check `https://<your-app>.vercel.app/health` → `"backend":"upstash"`.

### CLI
```sh
npm i -g vercel
cd tracker
vercel link
vercel deploy --prod
# add Upstash via the dashboard or: vercel integration add upstash
```

## How the NP2PTP client uses it

- On `serve`: `POST /announce { cid: <root>, peer: <peer-id>, addr: <public multiaddr> }`.
- On `fetch <link>` with no `--peer`: `GET /peers?cid=<root>`, then dial those peers.

Wired in `crates/np2ptp-node/src/tracker.rs`; both `serve` and `fetch` take a
`--tracker <url>` flag. Reachability across NATs still needs UPnP/relay; the
tracker only solves *discovery*.

## Live instance

The "principal" tracker is self-hosted (not Vercel) at **https://nptp.bogotec.uk**,
behind Caddy on the same VPS that fronts the rest of `bogotec.uk`. It runs as
the `np2ptp-tracker` systemd unit under `/opt/np2ptp-tracker` (plain Node,
`server.js` adapts the Vercel-style handlers in `api/` to a standalone HTTP
server — no Upstash/Redis needed since the process stays warm, unlike
serverless). This is now `tracker::DEFAULT_TRACKER`, so clients hit it
automatically unless `--tracker` overrides it. The Vercel deploy steps below
still work if you ever want a second/backup tracker instance.
