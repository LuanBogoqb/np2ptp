// POST /announce  { cid, peer, addr }
// A seed/peer registers that it serves content `cid` (nptp root or torrent
// infohash) at libp2p `addr` (a multiaddr) under identity `peer` (peer id).
import { announce } from "./_store.js";

export default async function handler(req, res) {
  if (req.method !== "POST") {
    return res.status(405).json({ error: "use POST" });
  }
  const body = typeof req.body === "string" ? safeParse(req.body) : req.body || {};
  const { cid, peer, addr } = body;
  if (!cid || !peer || !addr) {
    return res.status(400).json({ error: "need cid, peer, addr" });
  }
  await announce(String(cid), String(peer), String(addr));
  return res.status(200).json({ ok: true });
}

function safeParse(s) {
  try {
    return JSON.parse(s);
  } catch {
    return {};
  }
}
