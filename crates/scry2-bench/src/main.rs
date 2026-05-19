//! scry2 backend bench — redb vs plain-mmap vs rocksdb on the scry2
//! xref workload.
//!
//! Phases per backend:
//!   1. bulk-write 80M random rows (one big transaction / sort / batch)
//!   2. close + reopen read-only, drop OS page cache
//!   3. COLD prefix scans  — 1k random `(sym_id, role=Call)` ranges
//!   4. WARM prefix scans  — same query plan, no cache drop
//!   5. WARM whole-sym scans  — `(sym_id, *)` (= `ref` shape)
//!   6. POINT lookups — first row of `(sym_id, *)` (= `def` shape)
//!
//! Reports per-phase: wall time, /proc/self/io disk-read/write delta,
//! page-fault delta, latency p50/p90/p99/p99.9/max in ns, and
//! per-result-row ns (the actually-meaningful number for set-returning
//! ops, since prefix scan time is dominated by result size).

mod backend;
mod be_mmap;
mod be_redb;
#[cfg(feature = "rocksdb-backend")] mod be_rocks;
mod stats;
mod workload;

use anyhow::Result;
use backend::{Backend, drop_caches, du};
use stats::{IoSnapshot, LatencyReport, fmt_bytes, print_io_delta};
use std::path::PathBuf;
use std::time::Instant;
use workload::{Args, QueryPlan, XKey, build_query_plan, generate};

const ROOT: &str = "/mnt/agent/tmp/scry2-bench";

fn main() -> Result<()> {
    let args = Args::from_env();
    eprintln!("[bench] config: {} rows over {} symbols × {} files, {} scans",
        args.n_rows, args.n_symbols, args.n_files, args.n_scans);
    std::fs::create_dir_all(ROOT)?;

    let pick = std::env::var("BENCH_BACKEND").unwrap_or_else(|_| "all".into());
    let pick: Vec<&str> = pick.split(',').collect();
    let want = |s: &str| pick.iter().any(|x| *x == "all" || *x == s);

    // ---- 1. Generate the shared workload (random insert order). ----
    let t = Instant::now();
    let rows = generate(&args);
    eprintln!("[bench] generated {} keys in {:.1}s", rows.len(), t.elapsed().as_secs_f64());
    let plan = build_query_plan(&args);
    eprintln!("[bench] generated {} queries", plan.prefix_role_call.len());

    // ---- 2. Run each backend through identical phases. ----
    if want("mmap") {
        let be = be_mmap::MmapBackend::create(PathBuf::from(format!("{ROOT}/mmap.bin")))?;
        run_backend(Box::new(be), &rows, &plan)?;
    }
    if want("redb") {
        let be = be_redb::RedbBackend::create(PathBuf::from(format!("{ROOT}/redb.kdb")))?;
        run_backend(Box::new(be), &rows, &plan)?;
    }
    #[cfg(feature = "rocksdb-backend")]
    if want("rocks") {
        let be = be_rocks::RocksBackend::create(PathBuf::from(format!("{ROOT}/rocks")))?;
        run_backend(Box::new(be), &rows, &plan)?;
    }
    #[cfg(not(feature = "rocksdb-backend"))]
    if want("rocks") {
        eprintln!("[bench] rocks backend not compiled (rebuild with --features rocksdb-backend)");
    }
    Ok(())
}

fn run_backend(mut be: Box<dyn Backend>, rows: &[XKey], plan: &QueryPlan) -> Result<()> {
    println!();
    println!("===== {} =====", be.name());

    // -- Bulk write --
    let io0 = IoSnapshot::now();
    let t = Instant::now();
    be.bulk_write(rows)?;
    let wall = t.elapsed().as_secs_f64();
    let io1 = IoSnapshot::now();
    let size = du(&be.paths());
    println!(
        "  [write]          rows={}  wall={:.2}s  {:.0}k rows/s  file={}",
        rows.len(), wall, (rows.len() as f64) / wall / 1000.0, fmt_bytes(size),
    );
    print_io_delta("write-io", &io1.delta(&io0), wall);

    // -- Reopen + drop caches: cold phase needs the OS page cache empty --
    be.reopen_readonly()?;
    drop_caches();

    // -- COLD prefix (role=Call) --
    let mut lats = Vec::with_capacity(plan.prefix_role_call.len());
    let mut n_rows = 0u64;
    let io_pre = IoSnapshot::now();
    let phase_t = Instant::now();
    for (s, e) in &plan.prefix_role_call {
        let t0 = Instant::now();
        let n = be.prefix_count(s, e)?;
        lats.push(t0.elapsed().as_nanos() as u64);
        n_rows += n as u64;
    }
    let wall = phase_t.elapsed().as_secs_f64();
    let io_d = IoSnapshot::now().delta(&io_pre);
    print_io_delta("cold-prefix", &io_d, wall);
    LatencyReport { label: "cold-prefix",  nanos: lats, n_rows_total: n_rows }.print();

    // -- WARM prefix (role=Call), same plan --
    let mut lats = Vec::with_capacity(plan.prefix_role_call.len());
    let mut n_rows = 0u64;
    let io_pre = IoSnapshot::now();
    let phase_t = Instant::now();
    for (s, e) in &plan.prefix_role_call {
        let t0 = Instant::now();
        let n = be.prefix_count(s, e)?;
        lats.push(t0.elapsed().as_nanos() as u64);
        n_rows += n as u64;
    }
    let wall = phase_t.elapsed().as_secs_f64();
    let io_d = IoSnapshot::now().delta(&io_pre);
    print_io_delta("warm-prefix", &io_d, wall);
    LatencyReport { label: "warm-prefix",  nanos: lats, n_rows_total: n_rows }.print();

    // -- WARM whole-sym (ref shape) --
    let mut lats = Vec::with_capacity(plan.prefix_any_role.len());
    let mut n_rows = 0u64;
    let phase_t = Instant::now();
    for (s, e) in &plan.prefix_any_role {
        let t0 = Instant::now();
        let n = be.prefix_count(s, e)?;
        lats.push(t0.elapsed().as_nanos() as u64);
        n_rows += n as u64;
    }
    let wall = phase_t.elapsed().as_secs_f64();
    print_io_delta("warm-anyrole", &IoSnapshot::default(), wall);
    LatencyReport { label: "warm-anyrole", nanos: lats, n_rows_total: n_rows }.print();

    // -- POINT lookup (def shape) --
    let mut lats = Vec::with_capacity(plan.prefix_any_role.len());
    let mut hits = 0u64;
    let phase_t = Instant::now();
    for (s, e) in &plan.prefix_any_role {
        let t0 = Instant::now();
        let got = be.point_first(s, e)?;
        lats.push(t0.elapsed().as_nanos() as u64);
        if got.is_some() { hits += 1; }
    }
    let wall = phase_t.elapsed().as_secs_f64();
    print_io_delta("point", &IoSnapshot::default(), wall);
    LatencyReport { label: "point",        nanos: lats, n_rows_total: hits }.print();
    Ok(())
}
