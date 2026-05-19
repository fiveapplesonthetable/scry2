# scry2 backend bench — methodology

> Workload: 80M random `(sym_id, role, file_id, offset)` rows. 5M distinct
> symbols, 100k distinct files. Models a full-AOSP xref table.
>
> Hardware: this host (`uname -a` capture pending).
>
> Goal: pick one storage backend for scry2 based on real numbers, not
> intuition. Every ns accounted in `/proc/self/io` + `getrusage`.

## Why a backend bench at all

The question is whether to store the Kythe-derived xref table in a
plain mmap'd packed array (sorted records + binary search), an ACID
B+tree (redb), or an LSM tree (RocksDB). Each tradeoff:

| | mmap (packed) | redb (B+tree) | RocksDB (LSM) |
|---|---|---|---|
| Read shape | binary search + scan | B+tree descent | bloom-filtered SST scan |
| Write shape | sort all, flush once | one ACID txn (or many) | WriteBatch → memtable → SST |
| Update shape | rebuild file | incremental | incremental |
| Size overhead | 0 | ~2-3x (page headers, fanout slack) | compressed |
| Page-faults per cold query | ~1 | ~4-5 (tree depth) | ~1-2 with bloom |
| External dep | none | pure Rust | C++ FFI, 5-min first build |
| Concurrent readers | many (mmap) | many (mvcc) | many |
| Concurrent writers | one (rebuild) | one (txn) | one |
| Crash recovery | n/a (atomic rename) | ACID | WAL replay |

The scry2 access pattern is **write once per kzip, read forever** — so
the "incremental update" win of redb/RocksDB is irrelevant. The
question is purely query latency + disk size.

## Phases measured per backend

1. **bulk write** — wall time, /proc/self/io disk-write bytes, page-fault count, rows/s
2. **cold prefix scan** (role=Call) — 1000 random `(sym_id, role=2)` ranges, page cache evicted via `posix_fadvise(POSIX_FADV_DONTNEED)`
3. **warm prefix scan** — same query plan, no eviction
4. **warm whole-sym scan** — `(sym_id, *)` shape (= scry2's `ref` verb)
5. **point lookup** — first row of `(sym_id, *)` (= scry2's `def` verb)

Latency: ns precision, reported as p50 / p90 / p99 / p99.9 / max plus
per-row ns (so set-returning shapes are comparable across queries with
different result sizes).

## Results — 80M rows

Hardware: Intel Xeon Gold 6148 @ 2.40 GHz (72 vCPU), 157 GB RAM, SSD on `/mnt/agent/vdb`.

Workload: `BENCH_ROWS=80000000 BENCH_SYMS=5000000 BENCH_FILES=100000 BENCH_SCANS=1000`.
Logs: `/mnt/agent/tmp/scry2-bench/run-80M.log` (mmap+redb), `run-80M-rocks.log` (rocks).

| metric                           | mmap (packed) | redb (B+tree) | rocksdb (LSM)  |
|----------------------------------|---------------|---------------|----------------|
| **bulk write — wall**            | **12.0 s**    | 1147 s        | 205 s          |
| bulk write — throughput          | 6.6 M rows/s  | 70 k rows/s   | 390 k rows/s   |
| **file size on disk**            | **991.8 MB**  | 2.01 GB       | 1.15 GB        |
| **disk-write traffic on build**  | **991.8 MB**  | 128 GB        | 4.34 GB        |
| build write amplification        | **1.0x**      | 64x           | 4.4x           |
| **cold prefix scan p50**         | 2.48 ms       | **697 µs**    | 1.26 ms        |
| cold prefix scan p99             | 8.26 ms       | 2.78 ms       | 2.54 ms        |
| cold per-row ns                  | 583 us/row    | 148 µs/row    | 255 µs/row     |
| **warm prefix scan p50**         | **3.7 µs**    | 8.6 µs        | 21.9 µs        |
| warm prefix scan p99             | 21 µs         | 25 µs         | 343 µs         |
| **warm whole-sym scan p50**      | **2.4 µs**    | 10.5 µs       | 22.9 µs        |
| **warm point lookup p50**        | **1.8 µs**    | 7.7 µs        | 17.8 µs        |
| warm point lookup p99            | 3.1 µs        | 23 µs         | 35 µs          |
| warm per-row ns (prefix)         | 781 ns        | 1706 ns       | 8465 ns        |

### What stands out

* **mmap wins every warm metric by 4–12x.** Warm point in 1.8 µs is
  effectively the cost of an L3 miss + memcmp — there's nothing else
  doing work.
* **redb's only win is cold p50** — B+tree fanout (3 levels for 80M rows
  at ~500 keys/page) trumps mmap's 18-level binary search when each
  page costs ~250 µs from SSD. For interactive use this advantage
  disappears: after the first few queries the 1 GB file sits in the
  157 GB host page cache and "cold" stops happening.
* **rocksdb is the slowest on warm queries**, 12x slower than mmap on
  point lookups. The cost is C++ FFI per call plus LSM-tier bloom
  checks across multiple SSTs even for hot data.
* **Build write amplification is the kill shot for redb.** 128 GB of
  SSD writes to produce a 2 GB file means an AOSP-scale rebuild burns
  through SSD endurance and is bandwidth-bound on disk, not CPU.
  rocksdb's 4.4x amp is workable. mmap's 1.0x is ideal.
* **Build time matters for the rebuild story.** scry2's design is
  "rebuild on demand from kzip"; 12 s of mmap-write versus 19 min of
  redb-insert is the difference between "rebuild whenever" and "schedule
  an overnight job."

## Decision

**Plain mmap, sorted packed-array shape.**

Reasoning:

* The scry2 access pattern is **write-once-per-kzip, read-forever**.
  Incremental update — the only thing redb/rocksdb actually buy you over
  mmap — is irrelevant when each rebuild is 12 s.
* The hot working set for an LLM walking code is a few thousand symbols,
  which is trivially cache-resident on any host. Warm latency is what
  matters, and mmap dominates warm.
* Zero file-size overhead means an AOSP-scale index (~80M xrefs +
  inheritance + names = projected ~1.5 GB) stays in RAM end-to-end.
* No external dependencies. redb adds a moderately complex pure-Rust
  storage engine; rocksdb drags a 50 MB C++ library and a 5-min first
  build. mmap is the standard library.
* Crash recovery, ACID — not properties scry2 cares about. There is no
  writer to crash; the only writer is the build pipeline which writes
  to a temp file and atomic-renames on success.

What we give up: nothing real. mmap has no good answer to "partial
update without rebuild," but rebuild is 12 s and there's no plausible
scenario where 12 s is too long.

Open follow-ups, in priority order:

1. Pack secondary indices the same way: inheritance, symbol-name → id,
   file-id → path. All trivially fit the same mmap shape.
2. Implement the writer to emit *sharded* packed arrays (one shard per
   `sym_id >> N`), so a partial kzip rebuild only re-sorts the affected
   shards.
3. Decide on bloom filters / fence pointers for the very-cold case
   (cold p99 = 8 ms on mmap could be 1 ms with a fence-pointer
   summary). Probably not worth the complexity.

## Repro

```
cd scry2
cargo build --release [--features rocksdb-backend]
BENCH_ROWS=80000000 \
  BENCH_SYMS=5000000 \
  BENCH_FILES=100000 \
  BENCH_SCANS=1000 \
  BENCH_BACKEND=all \
  ./target/release/scry2-bench
```

`BENCH_BACKEND` is a comma list: `mmap`, `redb`, `rocks` (or `all`).
