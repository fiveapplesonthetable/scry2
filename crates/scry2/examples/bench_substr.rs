//! Micro-benchmark for the block-skip / galloping trigram substring index.
//!
//! Times ONLY `Index::syms_matching_substring` (the trigram candidate
//! intersection + verify that produces the matching syms) — NOT the
//! downstream xref aggregation that `def --substr` layers on top. This
//! isolates the substring-index latency.
//!
//! Two needle groups:
//!   * TYPICAL — the identifiers `def --substr` is normally given. These
//!     have at least one selective trigram, so galloping drives off it.
//!   * WORST — short, all-common needles whose every trigram is
//!     near-universal. Galloping cannot dodge these: the driver list is
//!     itself huge. They quantify the case skip-lists can't fully fix.
//!
//! Usage: cargo run --release --example bench_substr -- <index.s2db> [iters]

use std::path::Path;
use std::time::Instant;
use scry2::Index;

fn bench(ix: &Index, label: &str, needles: &[&str], iters: usize) {
    println!("\n== {label} ==");
    println!("{:<18} {:>10} {:>12} {:>12}", "needle", "hits", "avg_us", "p50_us");
    for n in needles {
        // Warm the trigram + names + blob pages for this needle.
        let _ = ix.syms_matching_substring(n, 64);
        let mut samples: Vec<u128> = Vec::with_capacity(iters);
        let mut hits = 0usize;
        for _ in 0..iters {
            let t = Instant::now();
            let r = ix.syms_matching_substring(n, 64);
            samples.push(t.elapsed().as_nanos());
            hits = r.len();
        }
        samples.sort_unstable();
        let avg = samples.iter().sum::<u128>() as f64 / iters as f64 / 1000.0;
        let p50 = samples[iters / 2] as f64 / 1000.0;
        println!("{n:<18} {hits:>10} {avg:>12.2} {p50:>12.2}");
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: bench_substr <index.s2db> [iters]");
    let iters: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(2000);

    let ix = Index::open(Path::new(&path)).expect("open index");

    let typical = [
        "Calling", "writeStrong", "HashMap", "Iterator", "getInstance",
        "Charset", "Buffer", "Exception", "toString", "compareTo",
        "getDeclaredField",
    ];
    // All-common-trigram needles: every trigram is near-universal, so the
    // driver list is huge and galloping can't shrink the work.
    let worst = ["get", "set", "Str", "ate", "ing", "tion", "ava"];

    bench(&ix, "TYPICAL (selective driver)", &typical, iters);
    bench(&ix, "WORST (all-common trigrams)", &worst, iters);
}
