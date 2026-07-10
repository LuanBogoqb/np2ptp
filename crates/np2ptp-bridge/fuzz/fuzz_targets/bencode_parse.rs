#![no_main]

use libfuzzer_sys::fuzz_target;

// Untrusted input surface: a .torrent file from an arbitrary source (disk or
// network). parse_torrent_file already rejects negative/oversized lengths,
// overly deep nesting, and path traversal — this just keeps that honest as
// the parser evolves. Panics/crashes are bugs; error returns are fine.
fuzz_target!(|data: &[u8]| {
    let _ = np2ptp_bridge::parse_torrent_file(data);
});
