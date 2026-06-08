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

## How the NP2PTP client will use it (Phase 2)

- On `serve`: `POST /announce { cid: <root>, peer: <peer-id>, addr: <public multiaddr> }`.
- On `fetch <link>` with no `--peer`: `GET /peers?cid=<root>`, then dial those peers.

(The Rust wiring is a Phase-2 task — see ../ROADMAP.md. Reachability across NATs
still needs UPnP/relay; the tracker only solves *discovery*.)
