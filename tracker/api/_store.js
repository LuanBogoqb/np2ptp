// Peer store for the NP2PTP tracker.
//
// Uses Upstash Redis (Vercel KV) when its env vars are present, so state
// survives across serverless invocations and instances. Falls back to an
// in-process Map when no KV is configured — handy for a first deploy or local
// test, but only consistent within a single warm instance (don't rely on it).

import { Redis } from "@upstash/redis";

// How long a peer's announcement is considered fresh.
const TTL_SECONDS = 1800;

let redis = null;
try {
  if (process.env.UPSTASH_REDIS_REST_URL && process.env.UPSTASH_REDIS_REST_TOKEN) {
    redis = Redis.fromEnv();
  } else if (process.env.KV_REST_API_URL && process.env.KV_REST_API_TOKEN) {
    redis = new Redis({
      url: process.env.KV_REST_API_URL,
      token: process.env.KV_REST_API_TOKEN,
    });
  }
} catch {
  redis = null;
}

const mem = new Map(); // cid -> Map(peer -> { addr, ts })

export function backend() {
  return redis ? "upstash" : "memory";
}

export async function announce(cid, peer, addr) {
  const now = Date.now();
  if (redis) {
    await redis.hset(`cid:${cid}`, { [peer]: JSON.stringify({ addr, ts: now }) });
    await redis.expire(`cid:${cid}`, TTL_SECONDS);
  } else {
    if (!mem.has(cid)) mem.set(cid, new Map());
    mem.get(cid).set(peer, { addr, ts: now });
  }
}

export async function peers(cid) {
  const cutoff = Date.now() - TTL_SECONDS * 1000;
  const out = [];
  if (redis) {
    const h = (await redis.hgetall(`cid:${cid}`)) || {};
    for (const [peer, v] of Object.entries(h)) {
      const rec = typeof v === "string" ? JSON.parse(v) : v;
      if (rec && rec.ts > cutoff) out.push({ peer, addr: rec.addr, ts: rec.ts });
    }
  } else {
    const m = mem.get(cid) || new Map();
    for (const [peer, rec] of m) {
      if (rec.ts > cutoff) out.push({ peer, addr: rec.addr, ts: rec.ts });
    }
  }
  return out;
}
