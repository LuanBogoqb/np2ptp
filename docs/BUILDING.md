# Building from Source

NP2PTP is a Rust workspace. Building from source is only necessary if you want to
modify the code, run the test suite, or target a platform without a prebuilt
binary — see the [latest release](https://github.com/LuanBogoqb/np2ptp/releases/latest)
for the common case.

## Requirements

- The Rust toolchain: https://rustup.rs
- On Windows: the MSVC toolchain (see [Windows Setup](#windows-setup) below).

## Build

```sh
git clone https://github.com/LuanBogoqb/np2ptp.git
cd np2ptp
cargo build --release -p np2ptp-node
```

The resulting binary is at `target/release/np2ptp` (or `np2ptp.exe` on Windows).

## Test

```sh
cargo test --workspace          # every unit and integration test in the workspace
cargo test -p np2ptp-core       # a single crate
cargo clippy --workspace --all-targets   # lints; keep at 0 warnings
```

The workspace currently carries roughly 110 unit and integration tests, including
real `libp2p` nodes exchanging content over QUIC (chunk-by-chunk and via RaptorQ
symbols), DHT peer discovery, reputation-based choking, a behind-NAT node
obtaining a relay reservation, and a full download through that relay. One
mDNS test is `#[ignore]`d — this dev sandbox doesn't deliver UDP multicast
between two local processes, so it needs a real network to confirm by hand;
see [Relay Setup](RELAY.md) for the relay story (no longer ignored — see
that doc for what was actually wrong).

## Windows Setup

Builds with the **MSVC** toolchain:

```sh
winget install Rustlang.Rustup
winget install Microsoft.VisualStudio.2022.BuildTools --override "--add Microsoft.VisualStudio.Workload.VCTools"
rustup default stable-x86_64-pc-windows-msvc
```

The Visual Studio Build Tools install provides `link.exe`, which `rustc`/`cc`
auto-detect. If a fresh shell can't find `link.exe`, build from an "x64 Native
Tools Command Prompt" (or run `vcvars64.bat` first) — this is usually unnecessary
once `rustc`'s own auto-detection kicks in.

## Fuzzing

The two parsers that touch adversarial bytes before anything is verified —
`.torrent` files and `.nptp` manifests — have
[`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) targets:

```sh
cargo install cargo-fuzz
rustup toolchain install nightly   # cargo-fuzz requires nightly

cd crates/np2ptp-bridge && cargo +nightly fuzz run bencode_parse
cd crates/np2ptp-core   && cargo +nightly fuzz run manifest_from_nptp
```

Add `-- -max_total_time=60` (seconds) to bound a run instead of letting it go
indefinitely. On Windows/MSVC, cargo-fuzz currently doesn't work in this
project: with AddressSanitizer, the runtime DLL isn't part of a standard
rustup nightly install; without it (`--sanitizer none`), nothing provides the
coverage instrumentation's `__sancov_*` symbols at link time either way, and
`bencode_parse` additionally fails to *build* under sancov because
`np2ptp-bridge` transitively pulls in all of `np2ptp-net` (libp2p, DHT, QUIC)
just to fuzz a parser with no networking of its own — a dependency in that
graph (`if-watch`) doesn't link under Windows+sancov. Run these on
Linux/macOS/WSL instead.

## Continuous Integration

Every push tagged `v*` (or a manual `workflow_dispatch`) triggers
[`.github/workflows/release.yml`](../.github/workflows/release.yml), which builds
release binaries for Linux and Windows and attaches them to a
[GitHub Release](https://github.com/LuanBogoqb/np2ptp/releases/latest).
