// GET /peers?cid=<content-id>
// Returns the fresh peers known to serve `cid`, so a client can dial them.
import { peers } from "./_store.js";

export default async function handler(req, res) {
  const cid = req.query.cid;
  if (!cid) {
    return res.status(400).json({ error: "need ?cid=<content-id>" });
  }
  const list = await peers(String(cid));
  return res.status(200).json({ cid, count: list.length, peers: list });
}
