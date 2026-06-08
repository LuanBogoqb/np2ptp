//! Runs every research scenario and prints a report.

use np2ptp_sim::{dedup, fec_cost, freeride, permanence};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    println!("NP2PTP research harness");
    println!("=======================\n");

    println!("[1] Dedup — store a file, then a lightly-edited v2");
    let d = dedup();
    println!("    chunks across both versions : {}", d.total_chunks);
    println!("    unique chunks stored        : {}", d.unique_chunks_stored);
    println!("    => dedup                    : {:.1}%\n", d.dedup_pct);

    println!("[2] Permanence — does content survive the seeder leaving?");
    let with = permanence(true).await;
    let without = permanence(false).await;
    println!("    with re-sharing : new peer completes after seed left = {}", with.completed_after_seed_left);
    println!("    no  re-sharing : new peer completes after seed left = {}", without.completed_after_seed_left);
    println!("    => content persists iff at least one peer re-shared it\n");

    println!("[3] Free-riding — does the reputation choke stop a leech?");
    let off = freeride(false).await;
    let on = freeride(true).await;
    println!("    choke OFF : leech completes = {}", off.leech_completed);
    println!("    choke ON  : leech completes = {}", on.leech_completed);
    println!("    => choke cuts the non-reciprocating peer off\n");

    println!("[4] FEC cost — chunk vs RaptorQ-symbol download (1 MB)");
    let f = fec_cost().await;
    println!("    chunk download : {} ms", f.chunk_ms);
    println!("    FEC   download : {} ms", f.fec_ms);
    println!("    => FEC trades extra round-trips for any-k-of-n resilience");
    println!("       (this prototype fetches one small symbol per request; batching would close the gap)");
}
