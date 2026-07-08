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

The workspace currently carries roughly 85 unit and integration tests, including
real `libp2p` nodes exchanging content over QUIC (chunk-by-chunk and via RaptorQ
symbols), DHT peer discovery, reputation-based choking, and a behind-NAT node
obtaining a relay reservation. One relayed-transfer test is `#[ignore]`d — it is
flaky on loopback and is instead validated against a real NAT by hand; see
[Relay Setup](RELAY.md) for that story.

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

## Continuous Integration

Every push tagged `v*` (or a manual `workflow_dispatch`) triggers
[`.github/workflows/release.yml`](../.github/workflows/release.yml), which builds
release binaries for Linux and Windows and attaches them to a
[GitHub Release](https://github.com/LuanBogoqb/np2ptp/releases/latest).
