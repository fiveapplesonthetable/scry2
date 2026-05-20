# scry2

Super-lean Kythe wrapper for AOSP-scale code walks. One Rust binary,
one mmap'd `.s2db` file, six LLM-shaped query verbs in single-digit
microseconds.

## What it does

Five verbs, no fluff:

```
scry2 def      NAME       # where is NAME defined?
scry2 ref      NAME       # every reference to NAME
scry2 callers  NAME       # call sites that target NAME
scry2 super    NAME       # direct supertypes of NAME (extends/overrides)
scry2 sub      NAME       # direct subtypes of NAME
scry2 callgraph NAME --direction up|down|both --depth N
```

Plus two build verbs:

```
scry2 index    --entries FILE -o out.s2db   # ingest pre-decoded Kythe entries
scry2 from-kzip --kzip K --kythe-root R -o out.s2db   # orchestrate indexers
```

## Why it exists

Existing Kythe-backed tools wrap the indexer output in a B+tree or
LSM database (LevelDB, RocksDB) plus a query daemon. That's the
right call for the cs.android.com use case — hundreds of concurrent
users hitting one warm server. For an LLM walking code in a single
session, it's overkill: the daemon's serialization, the database's
key encoding, and the bloom-filter-then-block-decompress chain
dominate the actual lookup.

scry2 strips it down to **one mmap'd packed-array file + binary
search**. Same upstream data, fewer layers between the query and
the bytes. The bench (`docs/BENCH.md`) shows this beats redb's
B+tree by 4× and rocksdb's LSM by 12× on warm point lookups —
because there's no allocator, no parser, no syscall on the hot
path. Every read is a memcmp on mmap'd bytes.

## Docs

* [`docs/INSTALL.md`](docs/INSTALL.md) — toolchain, Kythe release, smoke test
* [`docs/USAGE.md`](docs/USAGE.md) — verbs, examples, path filters, perf expectations
* [`docs/DESIGN.md`](docs/DESIGN.md) — storage layout, body-anchor callgraph, deliberate omissions
* [`docs/KYTHE.md`](docs/KYTHE.md) — exact Kythe edges + node facts consumed; per-indexer quirks
* [`docs/BENCH.md`](docs/BENCH.md) — three-way redb / mmap / rocksdb shoot-out, full numbers + decision

## Headline numbers

* Build: 80 M xrefs in **12 s** (projected from bench), 1 GB index.
* Warm query p50: **1.8 µs** point lookup / **3.7 µs** prefix scan.
* Disk: **991 MB** for 80 M xrefs (zero overhead vs the raw row bytes).
* Test coverage: unit tests across round-trip, FQN aliases,
  callgraph (both directions, dedup), name-substring, and the
  hand-rolled Kythe Entry decoder.
