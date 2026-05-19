# scry2 — design

> Goal: AOSP-scale Kythe wrapper that answers def/ref/callers/callgraph/
> super/sub in single-digit µs warm. Built standalone alongside scry —
> zero changes to scry's code.

## What this is and isn't

scry2 is a **read-only mmap'd index** that an LLM (or a human) queries
to walk a code graph. It is **not** a precision filter, **not** a
parsing engine, **not** a language server. The truth is whatever Kythe
indexers emit; scry2 stores and serves it.

If a Kythe indexer doesn't emit data for a translation unit (e.g.
because the kzip is missing source files), scry2 silently has nothing
to return. There is no fallback path. The reasoning: an LLM walking
code wants **honest emptiness** much more than it wants imputed
guesses — imputed cross-references are a known LLM hallucination
attractor. See `docs/KYTHE.md` for what cxx_indexer / java_indexer
emit and what they don't.

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
```

## Storage layout

One `.s2db` file. Every section is **page-aligned** so the kernel can
mmap each independently and the bench's `posix_fadvise(DONTNEED)`
cleanly evicts a single section's pages. All multi-byte integers are
**big-endian** packed so `memcmp` on a row's prefix gives lex order
on `(primary_key, …)` — that lets every query reduce to a binary
search on a slice plus a linear walk.

| section | row format | sort order | purpose |
|---|---|---|---|
| `xrefs` | `(sym u64, role u8, file u32, off u32)` = 17 B | `(sym, role, file, off)` | core: every anchor → sym attribution |
| `syms` | `(sym u64, kind u8, lang u8, name_off u32, name_len u16)` = 16 B | by sym | sym → (name, kind, language) |
| `names` | `(name_off u32, name_len u16, _pad u16, sym u64)` = 16 B | by name bytes | alpha-sorted name → sym lookup, **including aliases** from `/kythe/edge/named` |
| `files` | `(file u32, path_off u32, path_len u16)` = 10 B | by file | file id → path |
| `inhs` | `(child u64, parent u64)` = 16 B | by (child, parent) | inheritance edges (extends, overrides, satisfies) |
| `calls` | `(caller u64, callee u64, role u8)` = 17 B | by caller | callgraph DOWN — `calls_from(X)` is one binary search |
| `crev`  | same rows | by callee | callgraph UP — `called_by(X)` is one binary search; no linear scan |
| `blob`  | concat UTF-8 | n/a | all names and paths, referenced by `(off, len)` slots |

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

## What's intentionally absent

* **No precision filter.** scry's strict mode drops refs that can't be
  attributed to a single def site. That's the right call for some
  workloads (you want zero false positives in a callgraph) but not for
  others (an LLM wants every plausible candidate so it can prune by
  context). scry2 reports what Kythe says, no more, no less. We are
  honest about the cost: when cxx_indexer emits 199 unresolved refs to
  the same name, scry2 returns 199 rows. The user / LLM decides how to
  rank.
* **No daemon.** Every scry2 command opens the mmap, does the lookup,
  and exits. Startup is ~10 ms (process spawn + mmap), query is
  microseconds. There is nothing to keep running.
* **No incremental updates.** A kzip rebuild costs ~3 s for a 30 MB
  index (small cxx) and ~12 s projected for a 1 GB index (AOSP-scale
  per the bench). At those costs, "rewrite the whole file atomically"
  beats every incremental-update design we benched.
* **No precision-sidecar (the scry.bin / clang_usrs.bin shape).** scry2
  rolls the FQN-alias trick from scry's Phase-5 work directly into the
  name index, so a single `.s2db` is the complete index — no sidecars,
  no `build-resolutions` pass.

## Why this is fast (every ns accounted)

The bench at `docs/BENCH.md` is the source of truth. Headline numbers
at 80 M xrefs:

| | scry2 (mmap) | redb | rocksdb |
|---|---|---|---|
| warm point lookup p50 | **1.8 µs** | 7.7 µs | 17.8 µs |
| warm prefix scan p50 | **3.7 µs** | 8.6 µs | 21.9 µs |
| build wall (80 M rows) | **12 s** | 19 min | 3 min 25 s |
| disk written during build | **991 MB** | 128 GB | 4.3 GB |

The 1.8 µs warm-point cost is: one mmap'd page fault to the sym
section, one binary search across the sym table (log₂ 5 M ≈ 22
comparisons), one indirection into the blob section. Each step is
sequential and cache-friendly. There is no allocator, no parser, no
syscall on the hot path. Cold queries (first time a sym's page is
touched) cost about one extra page fault — measured 2.48 ms p50 on
the bench, which is the SSD random-read floor and not something scry2
can improve on.

For interactive use the file is fully cache-resident after the first
~100 queries on this 157 GB-RAM host, and everything moves to the warm
1–4 µs regime.

## What an "L7 SWE" critique would flag, and our answers

* *"Why xxHash64 not SHA-1?"* — Speed. Collisions are not security-
  sensitive (we're not gating access), and 2^64 is enough room at our
  scale. SHA-1 would cost 30× more CPU per row.
* *"Why packed `[u8; n]` records, not bincode?"* — `bincode` is fine
  but it can't be `memcmp`'d in sort order. The whole binary-search
  story falls apart without that. Packed BE bytes give us free
  ordering.
* *"Why no precision filter — is that not just a worse scry?"* — No.
  See "What's intentionally absent". scry2's audience is an LLM that
  prunes by context, not a CI that fails on a false positive.
* *"What about Kotlin?"* — public Kythe v0.0.75 has no source-level
  Kotlin indexer; the JVM bytecode indexer handles `.class` files
  but not source. Same constraint as scry. Kotlin call sites that go
  through a `.class` file land in the index; sources do not.
