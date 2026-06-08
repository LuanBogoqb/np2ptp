// GET /health — liveness + which storage backend is active.
import { backend } from "./_store.js";

export default function handler(_req, res) {
  res.status(200).json({ ok: true, service: "np2ptp-tracker", backend: backend() });
}
