#!/usr/bin/env python3
"""Friendly Python wrapper around the `np2ptp` Rust binary.

It drives the real Rust engine under the hood (so it speaks the exact protocol),
but hides the rough edges: it finds the binary for you and lets you say
``--peer host:port --id <peer-id>`` instead of typing a libp2p multiaddr.

Use it as a CLI::

    python np2ptp.py pack ./myfolder --out my.nptp --store seedstore
    python np2ptp.py serve my.nptp --store seedstore --port 4001
    python np2ptp.py fetch my.nptp --peer 100.110.12.28:4001 --id 12D3Koo... --out got
    python np2ptp.py info my.nptp

...or as a library::

    import np2ptp
    c = np2ptp.Client()
    print(c.info("my.nptp"))
    c.fetch("my.nptp", peer="100.110.12.28:4001", peer_id="12D3Koo...", out="got")
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

_DIAL_RE = re.compile(r"/ip4/(?P<host>[0-9.]+)/udp/(?P<port>\d+)/quic-v1/p2p/(?P<id>\w+)")


class Np2ptpError(RuntimeError):
    pass


def _find_binary() -> str:
    """Locate the np2ptp binary: $NP2PTP_BIN, then target/{release,debug}, then PATH."""
    env = os.environ.get("NP2PTP_BIN")
    if env and Path(env).exists():
        return env

    exe = "np2ptp.exe" if os.name == "nt" else "np2ptp"
    repo = Path(__file__).resolve().parent.parent
    for profile in ("release", "debug"):
        candidate = repo / "target" / profile / exe
        if candidate.exists():
            return str(candidate)

    on_path = shutil.which("np2ptp")
    if on_path:
        return on_path

    raise Np2ptpError(
        "could not find the np2ptp binary. Build it with "
        "`cargo build --release -p np2ptp-node`, or set NP2PTP_BIN to its path."
    )


def _build_multiaddr(peer: str, peer_id: str | None) -> str:
    """Accept a full multiaddr (`/ip4/.../p2p/ID`) or `host:port` + a peer id."""
    if peer.startswith("/"):
        return peer
    if not peer_id:
        raise Np2ptpError("a peer id is required (--id) when --peer is host:port")
    if ":" not in peer:
        raise Np2ptpError("--peer must be host:port (or a full multiaddr)")
    host, port = peer.rsplit(":", 1)
    return f"/ip4/{host}/udp/{port}/quic-v1/p2p/{peer_id}"


class Client:
    """A thin wrapper around the np2ptp CLI."""

    def __init__(self, binary: str | None = None):
        self.binary = binary or _find_binary()

    # -- internal helpers --------------------------------------------------

    def _capture(self, args: list[str]) -> str:
        proc = subprocess.run(
            [self.binary, *args],
            capture_output=True,
            text=True,
        )
        if proc.returncode != 0:
            raise Np2ptpError((proc.stderr or proc.stdout).strip() or "np2ptp failed")
        return proc.stdout

    def _stream(self, args: list[str], on_line=None) -> int:
        """Run, echoing output live. Optionally call on_line(line) for each line."""
        proc = subprocess.Popen(
            [self.binary, *args],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1,
        )
        try:
            assert proc.stdout is not None
            for line in proc.stdout:
                sys.stdout.write(line)
                sys.stdout.flush()
                if on_line:
                    on_line(line.rstrip("\n"))
            return proc.wait()
        except KeyboardInterrupt:
            proc.terminate()
            proc.wait()
            return proc.returncode or 0

    # -- commands ----------------------------------------------------------

    def pack(self, path, out=None, store=None, name=None) -> dict:
        """Chunk a file/folder into the store and write a .nptp. Returns its link."""
        args = ["pack", str(path)]
        if out:
            args += ["--out", str(out)]
        if store:
            args += ["--store", str(store)]
        if name:
            args += ["--name", name]
        text = self._capture(args)
        link = re.search(r"(np2ptp:\w+)", text)
        files = re.search(r"files:\s*(\d+)", text)
        chunks = re.search(r"chunks:\s*(\d+)", text)
        return {
            "link": link.group(1) if link else None,
            "files": int(files.group(1)) if files else None,
            "chunks": int(chunks.group(1)) if chunks else None,
            "raw": text.strip(),
        }

    def info(self, nptp) -> dict:
        """Parse `info` output into a dict."""
        text = self._capture(["info", str(nptp)])
        out: dict[str, str] = {}
        for line in text.splitlines():
            if ":" in line and not line.startswith(" "):
                key, _, value = line.partition(":")
                out[key.strip()] = value.strip()
        return out

    def fetch(self, target, peer=None, peer_id=None, out=None, store=None, fec=False, tracker=None) -> int:
        """Download content over the network. With `peer` it dials directly;
        without it, it auto-discovers providers via the tracker."""
        args = ["fetch", str(target)]
        if peer:
            args += ["--peer", _build_multiaddr(peer, peer_id)]
        if tracker:
            args += ["--tracker", tracker]
        if out:
            args += ["--out", str(out)]
        if store:
            args += ["--store", str(store)]
        if fec:
            args += ["--fec"]
        return self._stream(args)

    def serve(self, nptp, port=4001, store=None) -> int:
        """Seed content on the network (blocks until Ctrl-C). Prints a friendly
        summary with ready-to-run `python np2ptp.py fetch` commands."""
        args = ["serve", str(nptp), "--listen", f"/ip4/0.0.0.0/udp/{port}/quic-v1"]
        if store:
            args += ["--store", str(store)]

        seen: list[tuple[str, str, str]] = []

        def on_line(line: str):
            m = _DIAL_RE.search(line)
            if m and (m["host"], m["port"], m["id"]) not in seen:
                seen.append((m["host"], m["port"], m["id"]))
            if "Providing on the DHT" in line and seen:
                pid = seen[0][2]
                print("\n--- python clients can fetch with ---")
                for host, prt, _ in seen:
                    print(f"  python np2ptp.py fetch <file.nptp> --peer {host}:{prt} --id {pid} --out got")
                print("-------------------------------------\n")

        return self._stream(args, on_line=on_line)


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(prog="np2ptp", description="NP2PTP Python client (wraps the Rust binary).")
    sub = parser.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("pack", help="chunk a file/folder into a store and write a .nptp")
    p.add_argument("input")
    p.add_argument("--out")
    p.add_argument("--store")
    p.add_argument("--name")

    i = sub.add_parser("info", help="inspect a .nptp file")
    i.add_argument("nptp")

    f = sub.add_parser("fetch", help="download content (auto-discovers peers if --peer omitted)")
    f.add_argument("target", help="a .nptp file or an np2ptp:<root> link")
    f.add_argument("--peer", help="host:port (with --id) or a full multiaddr; omit to auto-discover")
    f.add_argument("--id", dest="peer_id", help="peer id (when --peer is host:port)")
    f.add_argument("--tracker", help="tracker URL (default https://np2ptp.vercel.app)")
    f.add_argument("--out")
    f.add_argument("--store")
    f.add_argument("--fec", action="store_true", help="reconstruct via RaptorQ symbols")

    s = sub.add_parser("serve", help="seed content on the network")
    s.add_argument("nptp")
    s.add_argument("--port", type=int, default=4001)
    s.add_argument("--store")

    args = parser.parse_args(argv)
    try:
        client = Client()
        if args.cmd == "pack":
            result = client.pack(args.input, out=args.out, store=args.store, name=args.name)
            print(result["raw"])
            return 0
        if args.cmd == "info":
            for key, value in client.info(args.nptp).items():
                print(f"{key:12} {value}")
            return 0
        if args.cmd == "fetch":
            return client.fetch(args.target, args.peer, peer_id=args.peer_id, out=args.out, store=args.store, fec=args.fec, tracker=args.tracker)
        if args.cmd == "serve":
            return client.serve(args.nptp, port=args.port, store=args.store)
    except Np2ptpError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
