# scry2

Super-lean Kythe wrapper for AOSP-scale code walks. One Rust binary,
one mmap'd `.s2db` file, five LLM-shaped query verbs in single-digit
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

scry2 is built alongside `scry` as a clean experiment. Same upstream
data (Kythe v0.0.75), different shape: **mmap'd packed-array index,
no daemon, no precision filter, no incremental updates** — rebuild from
the kzip in 12 s (projected at AOSP scale) and trust whatever Kythe
emitted.

It is **not a replacement for scry** — scry has years of scope around
precision filtering, build-graph reachability, and code-walking
heuristics that scry2 deliberately doesn't carry. scry2 is the
sub-millisecond index an LLM uses to navigate, full stop.

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
* Test coverage: 6 unit tests across round-trip, FQN aliases,
  callgraph (both directions, dedup), name-substring, and the
  hand-rolled Kythe Entry decoder.
