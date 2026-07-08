# Basic Usage

A quick guide to sharing and downloading files with NP2PTP — the essentials
only. For how it works internally, see the [README](../README.md). For more
elaborate scenarios (real network transfer, non-interactive usage, the Rust
API), see [Usage Examples](EXAMPLES.md).

## 1. Get the Binary

Download a prebuilt binary from the
[latest release](https://github.com/LuanBogoqb/np2ptp/releases/latest) —
`np2ptp-windows-x86_64.exe` (Windows) or `np2ptp-linux-x86_64` (Linux). No
installation required; it is a single executable. To build from source instead,
see [Building from Source](BUILDING.md).

## 2. Create a `.nptp` File (Link What You Want to Share)

Works with **a single file or an entire folder** (subfolder structure is
preserved):

```sh
np2ptp pack myfile.zip --out myfile.nptp

# or an entire folder:
np2ptp pack ./my-folder --out my-folder.nptp
```

The generated `.nptp` file is small — it only holds metadata (hashes), not the
content itself. This is the file you send to whoever will download it (email,
Discord, etc.); the actual content stays on your machine, in a store directory
(`.np2ptp-store` by default).

## 3. Make It Available on the Network

The `.nptp` file alone is not enough — someone also needs to be able to connect
to you. Run:

```sh
np2ptp serve myfile.nptp
```

and leave that running for as long as you want to share (like seeding a
torrent). This works even behind CGNAT or without an open router port — the
program detects that on its own and falls back to a public relay automatically,
with no configuration needed.

## 4. Download a `.nptp` File

```sh
np2ptp fetch myfile.nptp --out ./downloaded
```

If you only have the link (`np2ptp:abc123...`) rather than the `.nptp` file
itself, this works the same way:

```sh
np2ptp fetch np2ptp:abc123... --out ./downloaded
```

NP2PTP finds whoever is serving that content on its own, downloads it piece by
piece, and verifies the integrity of every piece before writing it — corrupted
or tampered content cannot arrive undetected.

## Folders vs. a Single File

- Single file → `--out` is the path of the restored file.
- Folder → `--out` is the destination directory; the subfolder structure is
  recreated inside it.
- Repeated files within a folder (or across different packages) are only
  transferred once — deduplication is automatic.

## Quick Extras

- `np2ptp info myfile.nptp` — lists what a `.nptp` file contains, without
  downloading anything.
- `np2ptp fetch ... --fec` — downloads using erasure coding (RaptorQ) instead of
  chunk by chunk; useful when seeders come and go.

See [Usage Examples](EXAMPLES.md) for real network transfers between two
machines, non-interactive (`--json`) usage for scripting or embedding NP2PTP in
another application, and the Rust API.
