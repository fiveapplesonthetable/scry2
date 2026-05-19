# scry2 — usage

scry2 is a super-lean Kythe wrapper. One binary, one index file, five verbs
that an LLM uses to walk code: `def`, `ref`, `callers`, `super`, `sub`.

## Build

```
cargo build --release -p scry2
```

The release binary lives at `target/release/scry2`.

## Index a kzip

scry2 v0 consumes a delimited Kythe `Entry` proto stream — the canonical
output of every Kythe v0.0.75 indexer. Run an indexer, pipe its stdout
into `scry2 index`.

```bash
# C++ — one .kzip, all CUs, single indexer run
~/kythe/cxx_indexer your_corpus.kzip \
  | scry2 index --entries - -o your_corpus.s2db

# Or pre-capture entries on disk first, then ingest separately
~/kythe/cxx_indexer your_corpus.kzip > corpus.entries
scry2 index --entries corpus.entries -o your_corpus.s2db

# Multiple streams in one ingest (e.g. cxx + java + go) share one file
# id allocator so xrefs across languages stay coherent.
scry2 index \
  --entries cxx.entries \
  --entries java.entries \
  --entries go.entries  \
  -o your_corpus.s2db
```

Reference numbers on this host (Xeon Gold 6148): a 6 MB cxx test kzip
produces 489 MB of entries → **2.94 s** ingest → 30 MB `.s2db` with
220k xrefs over 128k symbols and 1998 inheritance edges.

## Query verbs

Every verb takes `--index PATH.s2db` (defaults to `./scry2.s2db`).

```
scry2 stat                         # row counts and sanity
scry2 def NAME                     # decl/def sites
scry2 ref NAME                     # every reference
scry2 callers NAME                 # call sites only
scry2 super NAME                   # direct supertypes
scry2 sub NAME                     # direct subtypes
```

`NAME` is the canonical Kythe symbol string —
`kythe:<lang>:<corpus>#<root>#<path>#<signature>` — or, with `--substr`,
any substring of one.

```bash
# Find every callsite of clearCallingIdentity (Java)
scry2 --index aosp.s2db callers \
  kythe:java:android.googlesource.com/platform/superproject##frameworks/base/core/java/android/os/Binder.java#clearCallingIdentity\(\)

# Or the lazy way — substring match (slower but does what an LLM would
# actually type)
scry2 --index aosp.s2db callers clearCallingIdentity --substr --limit 4

# Decl sites of every symbol whose name contains "Binder"
scry2 --index aosp.s2db def Binder --substr --limit 32
```

Output format is one row per xref:

```
# kythe:java:...#android.os.Binder.clearCallingIdentity  [fn/java]
  call frameworks/base/services/core/java/com/android/server/am/ActivityManagerService.java@8001
  call frameworks/base/services/core/java/com/android/server/am/BroadcastQueueImpl.java@13044
  ...
hits=1212
```

`hits=` is emitted to stderr so a shell pipeline can `| head` the body.

## Latency expectations

On a 30 MB warm index (no page-fault disk reads), measured with
`/usr/bin/time -v`:

| op                            | wall   | peak RSS | major faults |
|-------------------------------|--------|----------|--------------|
| exact-name `def`/`callers`    | <1 ms  | ~20 MB   | 0            |
| `--substr` over 128 k syms    | 10–80 ms | ~25 MB | 0            |
| `ref` with 500 hits           | 20 ms  | ~26 MB   | 0            |

For full AOSP-scale (80 M xrefs ≈ 1 GB index), the underlying mmap
microbenchmark reports warm point p50 = **1.8 µs**, warm prefix p50 =
**3.7 µs** — i.e. the index lookups themselves are dwarfed by process
startup. See `docs/BENCH.md` for the three-way backend evaluation that
got us here.

## What scry2 does NOT do (yet)

* **Run the Kythe indexers itself.** `scry2 index` consumes entries. The
  scry repo's `scry-kzip` already orchestrates `cxx_indexer` /
  `java_indexer` / `jvm_indexer` per CU; scry2 will grow a `from-kzip`
  wrapper, but the leanest first step is for the user to pipe entries.
* **FQN normalisation.** Symbol names in v0 are the raw Kythe VName
  string. Most LLMs will use `--substr` and that's fine; an FQN bridge
  (Kythe `named` edges for Java + a USR demangler for C++) is next.
* **Cross-corpus updates.** Index files are immutable; a rebuild
  rewrites the file atomically. Cost is 3 s for a 6 MB kzip; projected
  ~30 s for the full AOSP cxx+java+go corpus.
