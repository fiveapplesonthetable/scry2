//! Shared workload generation. Every backend bench eats the same Vec<XKey>,
//! and the same query plan, so cross-backend numbers are comparable.
//!
//! Keys are 13 bytes: (sym_id u32 BE, role u8, file_id u32 BE, offset u32 BE).
//! Big-endian so prefix scans by (sym_id,) or (sym_id, role) are
//! lexicographically ordered the way redb / mmap / rocksdb all need.

use rand::{Rng, SeedableRng};

pub type XKey = [u8; 13];
pub const KEY_LEN: usize = 13;

#[derive(Clone, Copy, Debug)]
pub struct Args {
    pub n_rows: u64,
    pub n_symbols: u64,
    pub n_files: u64,
    pub n_scans: u64,
}

impl Args {
    pub fn from_env() -> Self {
        let env = |k: &str, def: u64| -> u64 {
            std::env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(def)
        };
        Self {
            n_rows:    env("BENCH_ROWS",  80_000_000),
            n_symbols: env("BENCH_SYMS",   5_000_000),
            n_files:   env("BENCH_FILES",    100_000),
            n_scans:   env("BENCH_SCANS",      1_000),
        }
    }
}

pub fn pack_key(sym_id: u32, role: u8, file_id: u32, offset: u32) -> XKey {
    let mut k = [0u8; 13];
    k[0..4].copy_from_slice(&sym_id.to_be_bytes());
    k[4] = role;
    k[5..9].copy_from_slice(&file_id.to_be_bytes());
    k[9..13].copy_from_slice(&offset.to_be_bytes());
    k
}

/// Generate the full insert stream deterministically. Returns the keys in the
/// random order a real Kythe driver would emit them — backends that need
/// sorted input (mmap) sort their own copy.
pub fn generate(args: &Args) -> Vec<XKey> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(0xDEADBEEF);
    let mut v = Vec::with_capacity(args.n_rows as usize);
    for _ in 0..args.n_rows {
        let sym_id: u32 = rng.gen_range(0..args.n_symbols as u32);
        let role:   u8  = rng.gen_range(0..3);          // Decl=0, Ref=1, Call=2
        let file_id:u32 = rng.gen_range(0..args.n_files as u32);
        let offset: u32 = rng.gen();
        v.push(pack_key(sym_id, role, file_id, offset));
    }
    v
}

/// Pre-generate scan keys so each backend evaluates the same queries in the
/// same order. Two flavors: (sym_id, role=Call) prefix scan = `callers`,
/// (sym_id,) prefix scan = `ref`.
pub struct QueryPlan {
    pub prefix_role_call: Vec<(XKey, XKey)>,  // (start, end_exclusive) for role=Call
    pub prefix_any_role:  Vec<(XKey, XKey)>,  // (start, end_exclusive) for whole sym_id
}

pub fn build_query_plan(args: &Args) -> QueryPlan {
    let mut rng = rand::rngs::StdRng::seed_from_u64(0xCAFEBABE);
    let mut role_call = Vec::with_capacity(args.n_scans as usize);
    let mut any_role  = Vec::with_capacity(args.n_scans as usize);
    for _ in 0..args.n_scans {
        let sym_id: u32 = rng.gen_range(0..args.n_symbols as u32);
        // role=Call range
        let mut start = [0u8; 13];
        start[0..4].copy_from_slice(&sym_id.to_be_bytes());
        start[4] = 2;
        let mut end = start;
        end[4] = 3;
        role_call.push((start, end));
        // whole sym_id range
        let mut s = [0u8; 13];
        s[0..4].copy_from_slice(&sym_id.to_be_bytes());
        let mut e = [0u8; 13];
        let next = sym_id.wrapping_add(1);
        e[0..4].copy_from_slice(&next.to_be_bytes());
        any_role.push((s, e));
    }
    QueryPlan { prefix_role_call: role_call, prefix_any_role: any_role }
}
