// Standalone HTTP server for self-hosting the tracker outside Vercel — adapts
// the Vercel-style (req, res) handlers in api/ to plain Node `http`. No other
// deps beyond what api/_store.js already needs. Run with `node server.js`;
// PORT/HOST default to 127.0.0.1:8787 (put a reverse proxy in front for TLS).
import http from "node:http";
import { URL } from "node:url";

import announceHandler from "./api/announce.js";
import peersHandler from "./api/peers.js";
import healthHandler from "./api/health.js";

const PORT = process.env.PORT || 8787;
const HOST = process.env.HOST || "127.0.0.1";

const routes = {
  "/announce": announceHandler,
  "/peers": peersHandler,
  "/health": healthHandler,
};

function withVercelShim(res) {
  res.status = (code) => {
    res.statusCode = code;
    return res;
  };
  res.json = (obj) => {
    const body = JSON.stringify(obj);
    res.setHeader("Content-Type", "application/json");
    res.end(body);
  };
  return res;
}

async function readBody(req) {
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  if (chunks.length === 0) return undefined;
  const raw = Buffer.concat(chunks).toString("utf8");
  try {
    return JSON.parse(raw);
  } catch {
    return raw;
  }
}

const server = http.createServer(async (req, res) => {
  withVercelShim(res);
  const url = new URL(req.url, `http://${req.headers.host || "localhost"}`);
  const handler = routes[url.pathname];
  if (!handler) {
    res.status(404).json({ error: "not found" });
    return;
  }

  req.query = Object.fromEntries(url.searchParams);
  if (req.method === "POST") {
    try {
      req.body = await readBody(req);
    } catch (e) {
      res.status(400).json({ error: "bad body" });
      return;
    }
  }

  try {
    await handler(req, res);
  } catch (e) {
    res.status(500).json({ error: String(e && e.message ? e.message : e) });
  }
});

server.listen(PORT, HOST, () => {
  console.log(`np2ptp-tracker listening on http://${HOST}:${PORT}`);
});
