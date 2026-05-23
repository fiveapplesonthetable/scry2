# scry2 — design

> Goal: AOSP-scale Kythe wrapper that answers def/ref/callers/callgraph/
> super/sub in single-digit µs warm. One Rust binary, one mmap'd file,
> no daemon, no embedded database.

## What this is and isn't

scry2 is a **read-only mmap'd index** that an LLM (or a human) queries
to walk a code graph. scry2 doesn't second-guess Kythe — there's
no post-filtering pass that drops "ambiguous" rows, no parsing engine,
no language server layer. The truth is whatever the Kythe indexers
emit; scry2 stores and serves it.

If a Kythe indexer doesn't emit data for a translation unit (e.g.
because the kzip is missing source files), scry2 silently has nothing
to return. There is no fallback path. The reasoning: an LLM walking
code wants **honest emptiness** much more than it wants imputed
guesses — imputed cross-references are a known LLM hallucination
attractor. See `docs/KYTHE.md` for what each indexer emits and what
it doesn't.

## Architecture in two diagrams

```
   .kzip                                            .s2db
  +-----+    +-------------+    +-----------+    +------+
  | cxx |--->|cxx_indexer  |--->|           |    |xrefs |
  +-----+    +-------------+    | scry2     |    +------+
  | java|--->|java_indexer |--->| ingest    |--->|syms  |
  +-----+    +-------------+    | (in-mem)  |    +------+
  | go  |--->|go_indexer   |--->| sort      |    |names |
  +-----+    +-------------+    | flush     |    +------+
                                +-----------+    |files |
                                                 +------+
                                                 |inhs  |
                                                 +------+
                                                 |calls |
                                                 +------+

   (build pipeline — runs once per kzip)
```

```
   .s2db                                          query verbs
  +-------+                                       +-------+
  | mmap  |<------- read-only --------- scry2 <---| def   |
  +-------+                                       | ref   |
  Header   (offsets + counts, 256 B)              | callers
  xrefs    (sym, role, file, off) — 17 B/row      | super |
  syms     (sym → kind, lang, name)               | sub   |
  names    (alpha index) → sym                    | call- |
  files    (file_id → path)                       |  graph|
  inhs     (child, parent)                        | stat  |
  calls    sorted by caller                       +-------+
  calls    sorted by callee
  blob     (UTF-8 strings, no separators)
  trigram  dict + block-skip postings (--substr)
```

## Storage layout

One `.s2db` file. Every section is **page-aligned** so the kernel can
mmap each independently and the bench's `posix_fadvise(DONTNEED)`
cleanly evicts a single section's pages. Row keys are **big-endian**
packed so `memcmp` on a row's prefix gives lex order on
`(primary_key, …)` — that lets every query reduce to a binary search
on a slice plus a linear walk. The 256-byte Header is the only
host-endian structure. The blob offsets carried in the `syms`,
`names`, and `files` rows are `u64`, since the blob can exceed 4 GiB
on an AOSP-scale index.

| section | row format | sort order | purpose |
|---|---|---|---|
| `xrefs` | `(sym u64, role u8, file u32, off u32)` = 17 B | `(sym, role, file, off)` | core: every anchor → sym attribution |
| `syms` | `(sym u64, kind u8, lang u8, name_off u64, name_len u16)` = 20 B | by sym | sym → (name, kind, language) |
| `names` | `(name_off u64, name_len u16, sym u64)` = 18 B | by name bytes | alpha-sorted name → sym lookup, **including aliases** from `/kythe/edge/named` |
| `files` | `(file u32, path_off u64, path_len u16)` = 14 B | by file | file id → path |
| `inhs` | `(child u64, parent u64)` = 16 B | by (child, parent) | inheritance edges (extends, overrides, satisfies); `super`/`inheritance --up` |
| `inhrev` | same rows | by (parent, child) | `sub`/`inheritance --down` — `inherited_by(X)` is one binary search, no linear scan |
| `calls` | `(caller u64, callee u64, role u8)` = 17 B | by caller | callgraph DOWN — `calls_from(X)` is one binary search |
| `crev`  | same rows | by callee | callgraph UP — `called_by(X)` is one binary search; no linear scan |
| `typed` | `(sym u64, str_off u64, str_len u16)` = 18 B | by sym | resolved type of a sym (deduced `auto`/`var`, concrete generics), pre-rendered to a blob string |
| `childrev` | `(parent u64, child u64)` = 16 B | by (parent, child) | `members` — a type's methods/fields (reverse `childof`) |
| `sig` | `(sym u64, str_off u64, str_len u16)` = 18 B | by sym | full function signature with param names (C++/Java), pre-rendered |
| `blob`  | concat UTF-8, no separators | n/a | all names, paths, types, and signatures, referenced by `(off u64, len u16)` slots |
| `trigram_dict` | `(trigram [u8;3], _pad u8, post_off u64, post_len u32, count u32)` = 20 B | by the 3-byte trigram | substring index: each lowercased trigram → its postings region + posting count |
| `trigram_post` | varint blob (block-skip / gap-delta) | n/a | per-trigram ascending name-row-ids, block-skip compressed; backs `--substr` |

The `typed`/`childrev`/`inhrev`/`sig` sections are the comprehension
layer and the `trigram_dict`/`trigram_post` sections are the substring
index; their header fields are carved from the header's reserved bytes.
The on-disk format is **v6** and the reader is **strict v6-only**:
`Index::open` accepts exactly version 6 (`VERSION == MIN_VERSION == 6`
in `format.rs`) and bails on anything else. This is dev mode — there is
no backward compatibility, so an index built by an older binary must be
rebuilt from the kzip. See "Format / version compatibility" below.

## Format / version compatibility

The `.s2db` on-disk format is **v6**, and the reader is **strict v6-only**.
`format.rs` pins `VERSION = 6` and `MIN_VERSION = 6`, and `Index::open`
rejects any file whose header version is not exactly 6. There is no older
fallback path: this is dev mode, not a stable file format. An index
produced by an older build is not upgraded in place — it is rebuilt from
the source kzip with `from-kzip`.

The header is a fixed 256-byte struct (offset 0) holding a `(byte offset,
row count)` pair per section. Every section is page-aligned (4 KB) so the
kernel faults each in independently. The sections, in file order:

| section | purpose |
|---|---|
| `xrefs` | every anchor → sym attribution `(sym, role, file, offset)` — backs `def`/`ref`/`callers` |
| `syms` | sym → `(kind, lang, name)` |
| `names` | alpha-sorted name → sym, including `/kythe/edge/named` and C++ MarkedSource aliases |
| `files` | file id → path |
| `inh` | inheritance edges `(child, parent)` — `super`/`inheritance --up` |
| `calls` | callgraph edges `(caller, callee, role)` sorted by caller — `calls_from` |
| `crev` | the same call rows sorted by callee — `called_by` in one binary search |
| `typed` | sym → resolved type string (deduced `auto`/`var`, concrete generics) |
| `childrev` | reverse `childof` `(parent, child)` — a type's `members` |
| `inhrev` | reverse inherits `(parent, child)` — `sub`/`inheritance --down` |
| `sig` | sym → full rendered signature with parameter names |
| `blob` | all UTF-8 strings (names, paths, types, signatures), concatenated, no separators |
| `trigram_dict` | each lowercased 3-byte trigram → `(post_off, post_len, count)`, sorted by the trigram — binary-searchable substring index |
| `trigram_post` | block-skip / gap-delta varint postings: each trigram's ascending name-row-ids — backs `--substr` |

There are **14 sections** in total (the 12 above plus the two trigram
sections, which are built once post-merge over the final alpha-sorted
names table and appended after the blob). All row **keys** are big-endian-packed, so a raw `memcmp` over a row's
key prefix equals the logical sort order — every lookup is a plain binary
search over a cast byte slice with no parsing and no comparator callback.
Blob offsets carried in the `syms`, `names`, `files`, `typed`, and `sig`
rows are **u64**, so the names+paths+types blob can exceed 4 GiB on a
full corpus.

Roughly:

* 1 xref row per Kythe `defines/binding | defines | ref | ref/call`
  edge that landed at a known anchor offset.
* 1 calls row per ref/call anchor whose enclosing function body could
  be identified (~74 % of cxx call sites in practice — the unresolved
  remainder are refs inside headers / macros that don't fall inside
  any function-body anchor).
* 1 name row per (raw VName-string AND every `/kythe/edge/named`
  alias) of each sym. Multiple names map to the same sym, all
  resolvable via `sym_for_name`.

## Symbol identity

A symbol is `xxHash64(VName-string)` where the VName string is

```
kythe:<lang>:<corpus>#<root>#<path>#<signature>
```

— the same canonical form Kythe uses internally. xxHash64 gives ~3 GB/s
on this CPU and a collision rate of `5e6 / 2^64 ≈ 2.7e-13` at our
scale (5 M symbols), which is effectively zero. Symbols cross
language boundaries cleanly: the JVM signature of `clearCallingIdentity`
hashes differently from the Java signature, but a `/kythe/edge/named`
edge from each to the FQN string registers them both as aliases of
the human name.

## Body-anchor callgraph extraction

This is where scry2 has done some real engineering — cxx_indexer's
`/kythe/edge/childof` connects sym scopes (namespace / class nesting),
NOT anchors to their enclosing function. So the naive "chase childof
from a call anchor to its parent function anchor" approach returns
zero matches.

The Kythe-correct alternative is body anchors. `/kythe/edge/defines`
(NOT `defines/binding`) marks an anchor whose `start..end` covers the
**entire** function definition body, not just the name. For each call/
ref anchor at `(file, off)`, the innermost body anchor with
`(same file, start ≤ off < end)` is the enclosing function.

Implementation at ingest:

1. Stream entries, accumulating per-anchor state: `is_anchor`,
   `start`, `end`, pending out-edges, optional `body_def_sym`.
2. When an anchor flushes complete (kind + start + end + edges seen),
   for each `defines` edge record `(file, start, end, target_sym)`
   into a body-anchors list. For each `ref` / `ref/call` edge record
   `(file, start, target_sym, role)` into a call-sites list.
3. After the stream, sort body-anchors by `(file, start)` and for each
   call site binary-search for the smallest containing range. Emit
   `(enclosing_sym, target_sym, role)` to the `calls` table.

Innermost wins so a call inside a lambda is attributed to the lambda,
not the surrounding method. Per-CU performance is `O((bodies +
call_sites) log bodies)` which on the smoke test runs at 1.88M
entries/3 s including the sort.

## Substring search — compressed galloping trigram index

`--substr` is backed by a trigram index built once post-merge over the
final alpha-sorted names table (two appended sections, `trigram_dict` +
`trigram_post`). It is not a linear scan.

* **Dict.** Each distinct lowercased 3-byte trigram present in any name
  is one `trigram_dict` row, sorted by the trigram, carrying its
  postings region `(off, len)` and a posting `count`. The dict is
  case-folded so one index backs both the case-sensitive default and
  `-i`.
* **Postings.** Each trigram's postings are the ascending name-row-ids
  that contain it, **block-skip compressed**: split into fixed blocks of
  `TRIGRAM_BLOCK` (128) ids, each block stored as LEB128 gap-delta
  varints (dense runs collapse to ~1 byte per id), with a per-trigram
  skip-table of one `(max_id, block_offset)` entry per block.
* **Query.** Look up each distinct needle trigram's descriptor with no
  decode; the smallest `count` is the **driver**. Fully decode only the
  driver (small). For each driver candidate in ascending order, probe
  every other trigram for membership via its skip-table — galloping /
  binary search to the one block that could hold the candidate, decode
  just that block. A per-list forward cursor means ascending candidates
  never re-scan earlier blocks. So a query costs the driver list's size
  plus a per-candidate `O(log n_blocks + TRIGRAM_BLOCK)` probe, not the
  sum of all list sizes.
* **Verify.** The trigram intersection is necessary-but-not-sufficient
  (`abcZZbcd` holds `abc` and `bcd` but not `abcd`), so each candidate
  that survives all lists is verified with a real substring check —
  case-sensitive on the raw bytes by default, case-folded under `-i`.
* **Fallback.** A needle shorter than 3 chars has no trigram, and an
  empty names table has no dict; both fall back to the parallel
  `memchr::memmem` linear scan.

Performance: sub-millisecond warm for typical needles; the worst case is
a needle whose every trigram is near-universal (e.g. on AOSP the
canonical `kythe:...` VName prefix), which stays in the low-ms range
because the driver is still the most-selective list. The per-call
`--limit` cap bounds the work that survives into `ref`/`callers --substr`.

## What's intentionally absent

* **No second-pass attribution heuristic.** Some tools run a
  post-filter that drops call-site rows whose target sym can't be
  uniquely attributed to a single def — useful when zero false
  positives are required (CI gating). Wrong for an LLM walking code:
  the LLM has surrounding context to prune candidates, and a silent
  drop is worse than an ambiguous row. scry2 reports every Kythe
  edge as-is. When cxx_indexer emits 199 anchors targeting
  `incStrong` across the kzip, scry2 returns all 199.
* **No daemon by default.** Each `scry2 NAME` call opens the mmap,
  does the lookup, and exits. Startup is ~10 ms (process spawn +
  mmap), the lookup itself is microseconds. For sessions where
  startup dominates (an LLM doing 100s of queries), `scry2 repl`
  amortizes startup across one stdin/stdout JSON loop; `scry2 serve`
  exposes the same protocol over a Unix socket. None of these are
  daemons in the system-service sense — they live and die with the
  parent process.
* **No incremental updates.** A kzip rebuild costs ~3 s for a 30 MB
  index (small cxx) and ~12 s projected for a 1 GB index (AOSP-scale,
  per the bench). At those costs, "rewrite the whole file atomically"
  beats every incremental-update design the bench tried. The format
  is read-only on purpose — every section is sorted at build time,
  so the reader is just `mmap` + `memcmp`.
* **No precision sidecars.** A single `.s2db` is the complete index.
  FQN-alias resolution (the path that lets `scry2 def
  android.os.Binder.clearCallingIdentity` work without `--substr`)
  is rolled directly into the name table — no separate
  `clang_usrs.bin` / `scip_index.bin` / `build-resolutions` pass.

## Related docs

* [LIMITS.md](LIMITS.md) — known correctness limits: call attribution,
  the ingested edge subset, hash identity, substring semantics, the
  `--inject-cu-arg` re-encode invariant, and the trusted-input boundary.
* [VS_KYTHE.md](VS_KYTHE.md) — when to use scry2's `.s2db` vs Kythe's own
  serving tables.

## Why this is faster than LevelDB / RocksDB / redb

The bench at [`BENCH.md`](BENCH.md) is the source of truth — three
backends, identical workload, full numbers. Headline at 80 M xrefs:

| | scry2 (mmap) | redb (B+tree) | rocksdb (LSM) |
|---|---|---|---|
| warm point lookup p50 | **1.8 µs** | 7.7 µs | 17.8 µs |
| warm prefix scan p50 | **3.7 µs** | 8.6 µs | 21.9 µs |
| build wall (80 M rows) | **12 s** | 19 min | 3 min 25 s |
| disk written during build | **991 MB** | 128 GB | 4.3 GB |

The gap isn't because scry2 is doing less work — it's doing
*structurally* less work per query. Step through what a warm point
lookup costs in each backend:

### scry2 (one binary search on a flat packed array)

1. `mmap()` of the index file done once at process start (~10 ms,
   not in the hot path).
2. Compute a 4 KB-page offset from the binary-search midpoint.
3. **One `memcmp` of 16 bytes** against the candidate row.
4. Adjust `lo`/`hi`, repeat ~22 times (log₂ of 5 M syms).
5. Total: 22 memcmps + ~22 cache line reads from the hot region of
   the sym table. **No allocator, no parser, no syscall.**

### redb (B+tree)

1. mmap of the redb file.
2. **Walk the B+tree**: 4 levels at AOSP scale, each level reads a
   4 KB page, deserializes the page header, runs a comparator
   callback (vptr indirection) for each key in the page.
3. Each comparator call decodes the stored key bytes back into
   typed Rust values, then runs the user-defined `Ord` impl.
4. After landing on a leaf, deserialize the value bytes (we store
   `()` so this is cheap, but the bookkeeping isn't free).
5. Total: 4 page reads, 4 deserialization passes per page, ~16
   comparator callbacks. **~4× scry2 in CPU time even fully warm.**

### rocksdb (LSM)

1. mmap or pread of the SST files (multiple — one per LSM level).
2. **Bloom filter check** per SST to decide which levels to probe.
3. **Block decompression** if the target block is lz4-compressed.
4. Binary search within the decompressed block.
5. FFI: every iteration crosses a Rust→C++ boundary (function call
   + ownership marshaling).
6. Block cache lookup + atomic refcount on hit / miss.
7. Total: ~50 µs warm just for the FFI + decompression + block
   cache machinery, before the actual search starts. **~10× scry2.**

### LevelDB is the same shape as rocksdb but worse

LevelDB doesn't ship with bloom filters out of the box for prefix
keys, so range scans against a cold prefix touch every SST until
the right block is found. Plus LevelDB's single-threaded compaction
means the SST count can spike — making the search fan-out bigger.
Same per-query overhead as rocksdb (FFI, decompression, block cache);
worse worst-case behaviour under write pressure.

### The cheat code: read-only mmap + sorted packed rows

What lets scry2 collapse all of that to *one binary search*:

* **Read-only by construction.** Index files are written once and
  never mutated. No journal, no WAL, no MVCC, no locking on the
  read path. Other databases pay for write-tolerance even when
  reading.
* **Packed fixed-width rows.** Each row is `[u8; 17]` (xrefs) or
  `[u8; 20]` (syms). No length prefix, no header, no padding. A
  10 GB file = a 10 GB sorted array; no other overhead.
* **Big-endian keys = free comparator.** Multi-byte fields are
  packed BE, so `memcmp` on the row bytes IS the lexicographic
  compare we want. No user-defined comparator, no callback, no
  deserialization in the inner loop. The CPU's hardware compare
  instructions are the comparator.
* **The OS page cache is our block cache.** No app-level cache, no
  LRU, no double-buffering. The kernel does it for free, scaled to
  whatever RAM the host has.
* **One file = one VMA = full sequential prefetch.** When the
  binary search transitions to a linear walk (for prefix scans),
  the kernel prefetcher kicks in immediately. Database engines
  with multiple SST files don't get that.

### What we give up

* No durable writes — but we don't have writes anyway, just bulk
  rebuilds.
* No range deletion / overwrite — same.
* No queryable snapshots across writes — same.

So scry2 isn't faster because we optimised harder. We're faster
because **the workload is single-writer-bulk, multi-reader-immutable**,
and a database's flexibility is paid-for overhead at every read.

## Cold reads are bounded by SSD, not the storage engine

Warm numbers are the steady state once the hot pages of the file
land in the OS cache. Cold reads (first time a sym's page is
touched) are bounded by SSD random-read latency — measured 2.48 ms
p50 against the bench's 80 M-row index. Every storage engine pays
that same cost. scry2 just happens to need fewer page faults per
cold query than the alternatives (~1 vs ~4 for B+tree vs ~5 for LSM
+ bloom miss), so its cold tail is also smallest in absolute terms.

For interactive use on the 1 GB AOSP-scale index, the file is fully
cache-resident after the first few hundred queries on any modern
host. Everything moves to the warm 1–4 µs regime and stays there.

## FAQ — common questions about the design

* **xxHash64, not SHA-1, for sym identity.** Collisions are not
  security-sensitive; at 5 M syms / 2^64 keyspace the probability
  is 2.7e-13. SHA-1 would cost ~30× more CPU per insert.
* **Packed `[u8; n]` rows, not `bincode` / `flatbuffers`.** Those
  formats can't be `memcmp`'d in sort order, which breaks binary
  search. Packed BE bytes get the ordering for free.
* **Substring name search is a compressed trigram index.** `--substr`
  intersects the needle's distinct (lowercased) trigrams against the
  `trigram_dict`/`trigram_post` sections: it galloping-probes the
  smallest (most-selective) posting list and verifies each surviving
  candidate with a real substring check. Postings are block-skip
  compressed (per-trigram skip-table over fixed blocks of gap-delta
  varints), so a probe touches one block, not the whole list. It is
  case-sensitive by default (verify on the raw bytes); `-i` /
  `--ignore-case` verifies case-folded (needle and candidate lowercased).
  A needle shorter than 3 chars (no trigram) falls back to a parallel
  `memchr::memmem` linear scan. Each call is bounded by its per-call cap
  (`--limit`), which also caps the symbols that flow into the `ref` /
  `callers --substr` aggregation. See [LIMITS.md](LIMITS.md) for the
  exact semantics.
* **Kotlin source-level coverage is partial.** Public Kythe v0.0.75
  ships no source-level Kotlin indexer. The JVM bytecode indexer
  handles `.class` files but not source. Kotlin call sites that
  resolve through compiled `.class` files land in the index; pure
  source-only paths don't.
