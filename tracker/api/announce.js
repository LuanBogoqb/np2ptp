// POST /announce  { cid, peer, addr }
// A seed/peer registers that it serves content `cid` (nptp root or torrent
// infohash) at libp2p `addr` (a multiaddr) under identity `peer` (peer id).
import { announce } from "./_store.js";

export default async function handler(req, res) {
  if (req.method !== "POST") {
    return res.status(405).json({ error: "use POST" });
  }
  const body = typeof req.body === "string" ? safeParse(req.body) : req.body || {};
  const { cid, peer } = body;
  // Accept a single `addr` or an array of `addrs`.
  const addrs = Array.isArray(body.addrs) ? body.addrs : body.addr ? [body.addr] : [];
  if (!cid || !peer || addrs.length === 0) {
    return res.status(400).json({ error: "need cid, peer, and addr or addrs" });
  }
  await announce(String(cid), String(peer), addrs.map(String));
  return res.status(200).json({ ok: true });
}

function safeParse(s) {
  try {
    return JSON.parse(s);
  } catch {
    return {};
  }
}
