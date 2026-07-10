#![no_main]

use libfuzzer_sys::fuzz_target;
use np2ptp_core::Manifest;

// Untrusted input surface: a .nptp file (shared file-to-file) or a manifest
// pulled from the network (`get_manifest`, validated against the requested
// root only *after* this parses). Panics/crashes are bugs; error returns
// are fine — this never trusts the result without also checking
// root_is_consistent()/chunk_hash_ok() first, but the parser itself must
// never be able to crash on adversarial bytes.
fuzz_target!(|data: &[u8]| {
    let _ = Manifest::from_nptp(data);
});
