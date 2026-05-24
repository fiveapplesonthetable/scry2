# scry2 — usage

A compact verb set over a single `.s2db` file — `def`, `ref`,
`callers`, `callgraph`, `super`, `sub`, `inheritance`, `type`, `sig`,
`members`, `names`, `stat`, plus the `repl` / `serve` long-lived modes.
Every example below was run on a real index built from a C++ test kzip
(`scry2-smoke.s2db`, 220 k xrefs, 128 k symbols, 64 k callgraph edges,
30 MB on disk).

## When to use scry2 — and when not to

scry2 answers questions about **symbols**, resolved from a Kythe graph: where a
thing is defined, who references or calls it, its type/signature, what it extends
or overrides, a type's members. It is semantic (not textual) and fast (single
mmap, sub-millisecond warm).

It is **not** a text search and **not** live:

- It does **not** index source-file content. A log string, a comment, or any
  arbitrary text will not be found. `--substr` matches the qualified **symbol
  ticket** (names + paths), never file contents — so a 0-result `--substr` means
  "no symbol matched", **not** "absent from the codebase".
- It is a **snapshot** built at index time, not your working tree — it will not
  reflect unsaved edits or changes made since the index was built.

Routing rule (especially for automated / LLM callers): **symbol lookup → scry2;
raw text, log strings, or current-file freshness → `grep`/`ripgrep` or read the
file.** Misrouting a text query to `--substr` and getting nothing is the most
common mistake — that is the tool working as designed, not the string missing.

## Build the index

### Option 1 — pipe one indexer's stdout

If you already know which Kythe indexer to run, or you want to do it
yourself, just pipe entries straight in:

```bash
~/kythe/kythe-v0.0.75/indexers/cxx_indexer your_corpus.kzip \
  | scry2 index --entries - -o your.s2db
```

`--entries -` reads delimited Kythe Entry protos from stdin. You can
pass multiple `--entries FILE` to mix per-language captures into one
ingest:

```bash
scry2 index \
    --entries cxx.entries \
    --entries java.entries \
    --entries go.entries  \
    -o aosp.s2db
```

### Option 2 — `from-kzip` orchestrator

For a multi-language kzip (the AOSP shape), let scry2 spawn every
indexer for you:

```bash
scry2 from-kzip \
    --kzip your.kzip \
    --kythe-root ~/kythe/kythe-v0.0.75 \
    -o your.s2db
```

Restrict to a subset of languages with `--langs cxx,java`. Bump the
JVM heap with `--jvm-heap 16g` if java_indexer OOMs on a fat CU.

Output during build (all on stderr) looks like:

```
[from-kzip] plan: 18342 CUs to index (5120 skipped: lang=…, path=…)
[from-kzip]       from-kzip       index   12000/18342 ( 65.4%)  +812.3s
[heartbeat] +840s done=12410/18342 (886.4/min) snap@=12000 partial=4.71G rss=18.2G indexers=22 delta_rows=83.5M
[from-kzip] snapshot @ 14000/18342: shard 0007 (14000 shas durable, sinks drained=24/24, busy=0)
[from-kzip] cxx: CUs=9211 (ok=9050 empty=120 failed=41) entries=… anchors=… xrefs=… inh=… alias=… calls=… types=…
[from-kzip] java: CUs=8800 (ok=8700 empty=80 failed=20) entries=… …
[from-kzip] jvm:  CUs=331  (ok=331 empty=0 failed=0) entries=… …
[from-kzip] final merge: remainder (xrefs=…) + base(yes) + 7 shard(s) — single k-way pass
[from-kzip] merge: xrefs 80123456 rows (1/13 sections, 1.36G)
[from-kzip] merge heartbeat: +20s, output 2.41G, rss 14.8G
[from-kzip] merge: names 110234567 rows (3/13 sections, 3.02G)
[from-kzip] merge: trigram dict=4812345 postings=0.71G (13/13 sections, 5.09G)
[from-kzip] done in 1043.27s → your.s2db (5.10 GB)
```

The per-CU **progress** line (`index N/TOTAL (P%)`) updates as workers
finish CUs. A periodic **`[heartbeat]`** line (every 30 s) reports
done/total, CU rate, the CU count of the last snapshot, the on-disk
partial size, process RSS, the count of live indexer subprocesses, and
the in-RAM `delta_rows`. **`snapshot @ N`** lines mark each delta drain
to a shard. After all CUs finish, one **per-language summary** line per
language (`cxx:`/`java:`/`jvm:`/…) reports `CUs=(ok empty failed)` and
aggregate counts, followed by **`final merge`** and **`done in Ns`**.

## `from-kzip` operational guide

`from-kzip` reads one (possibly multi-language) kzip, routes each CU to
the right Kythe indexer by its `v_name.language`, ingests every indexer's
entry stream, and writes a single `.s2db`. The flags:

| flag | what it does |
|---|---|
| `--kzip PATH` | the input kzip (required). |
| `--kythe-root DIR` | the Kythe release dir; indexers are resolved under `DIR/indexers/` (`cxx_indexer`, `go_indexer`, `proto_indexer`, `textproto_indexer`, and the `java_indexer`/`jvm_indexer` jars). Required. |
| `--langs cxx,java,jvm,go,proto,textproto` | restrict to a subset of languages. Routing is by the CU's language, not by file extension. |
| `--jvm-heap 8g` | `-Xmx` for the JVM-based indexers; bump it if `java_indexer` OOMs on a fat CU. |
| `--in SUBSTR` | scope to CUs whose primary source path contains ANY of these substrings (repeatable / comma-separated). `--not-in` is the inverse. |
| `--workers N` | CUs indexed concurrently. Default is `num_cpus/2` (the JVM indexers carry a 200–300 MB working set, so the default avoids OOM on big runs). |
| `--snapshot-every N` | drain the in-RAM delta to a shard after this many successful CUs. Default 2000; a coarse durability fallback. 0 disables. |
| `--snapshot-rows N` | drain whenever the in-RAM delta reaches this many rows (xrefs+inherits+calls+aliases+typed+childof+sig — every append-only delta table). **This is what bounds peak memory** — a large CU crosses the budget sooner and triggers an earlier drain, so the peak is deterministic regardless of how rows are distributed. Default 250M (≈ a 25 GB delta). 0 disables. Bound memory with this, not with worker count. |
| `--inject-cu-arg PREFIX::ARG` | prepend `ARG` to the indexer argv of any CU whose primary path starts with `PREFIX` (the `::` is the separator). Repeatable. Example: `'libcore/ojluni/src/main/java/::--patch-module=java.base=libcore/ojluni/src/main/java'` so AOSP libcore ojluni files index against `java.base`. Skipped if the CU's argv already has the arg. |
| `--resume` | continue a killed run from its on-disk partial state. |
| `--cache-dir PATH` | opt into INCREMENTAL rebuilds: each CU's delta shard is persisted in `PATH`, keyed by a content digest of exactly what its indexer consumes; a later rebuild pointed at the same `PATH` reuses unchanged CUs' shards (skipping the indexer subprocess **and** the ingest) and re-indexes only changed CUs. The merged output is byte-identical to a full build. Omit this flag (the default) for a plain full build. Ignored under `--resume`. At AOSP scale the cache is large — per-CU shards don't dedup shared blobs, so it can be ~20x the `.s2db` — so use it for a working set you iterate on, not a one-shot whole-tree build. |
| `--clean` | with `--cache-dir`: delete that cache, do a full from-scratch build, then repopulate it fresh. Use after a toolchain change or to clear a cache you no longer trust. Requires `--cache-dir`. |
| `-o, --out PATH` | output `.s2db` (default `scry2.s2db`). |

### Incremental rebuilds (the shard cache)

Incremental rebuilds are **opt-in** via `--cache-dir PATH`; the default is a
plain full build. With `--cache-dir`, `from-kzip` persists a per-CU shard in
`PATH`. The cache key is a content digest of everything the indexer reads for a
CU — its argument list, its required-input `(path, content-digest)` pairs, an
indexer-version tag, and scry2's ingest-schema version — so it changes exactly
when (and only when) re-indexing that CU would produce different output. On a
rebuild pointed at the same `PATH`:

* **unchanged CU** → its cached shard is reused: no indexer subprocess, no
  ingest, just a shard copy into the merge set;
* **changed / new CU** → re-indexed, its shard re-stored under the new digest;
* **dropped CU** → simply not merged; its orphan cache shard is pruned.

The rebuilt `.s2db` is **byte-for-byte identical** to a full build of the same
kzip. The enabler is deterministic file-ids: each shard stores its file
references in a local, membership-independent namespace (a path's rank among
only that CU's files), and the final merge re-assigns global ids = the path's
rank in the build's sorted seed set. So adding, deleting, or re-`--in`-slicing
CUs only re-sorts the union at merge time; it never shifts a cached shard's
stored ids, and every unchanged CU is reused. (A shard-format change is folded
into the digest via the ingest-schema version, so a stale shard is invalidated
on lookup, not by wiping the whole cache.)

**Disk cost.** Per-CU shards don't dedup blobs shared across CUs (C++ headers
especially), so at whole-AOSP scale the cache is roughly 20x the merged
`.s2db` — hundreds of GB. Enable it for an iterated working set, not a one-shot
whole-tree index.

`--clean` (with `--cache-dir`) forces a guaranteed-clean from-scratch rebuild
and refreshes the cache. `--resume` is unchanged and uses its own
partial-state mechanism, independent of this cache.

### On-disk artifacts during a run

A run is checkpointed beside `--out`:

* **`<out>.partial.shard.NNNN.s2db`** — delta shards. Each snapshot drains
  the workers' in-RAM delta to a fresh numbered shard, so a kill loses at
  most the rows ingested since the last drain.
* **`<out>.partial.shas`** — the list of CU shas already folded into the
  durable state, one per line. The invariant: a sha is never written
  until its CU's rows are durable, so the shas file never names a CU whose
  data is missing.

On a clean finish all of these (plus the staging dir) are removed and
only `<out>` remains.

### Resume

`--resume` loads the partial state, then re-runs only the CUs whose sha is
**not** already in `<out>.partial.shas` — failed and empty CUs carry no
sha, so they re-run too. The previously-written shards are re-merged in
the final pass, so no already-ingested work is repeated. Without
`--resume` an existing `--out` is rebuilt from scratch.

### Final merge

After every CU finishes, the run does **one k-way streaming pass** over
(the in-RAM remainder delta + the base partial + every shard). Each source
is mmap'd and read exactly once, so peak RAM is roughly one output blob
rather than the whole union.

The merge logs a `[from-kzip] merge: <section> <rows> (<k>/13 sections,
<G>)` line as each of the 13 declared sections lands (the 13th being the
trigram dict + postings, logged as `merge: trigram dict=… postings=…`),
plus a `[from-kzip] merge heartbeat: +Ns, output …G, rss …G` line every
~20 s so the long gather sub-phases between section lines don't look hung.

## Output mode and invocation

All examples assume `--index your.s2db`, which can also be the
default (`./scry2.s2db`).

**Output mode.** Every query verb accepts `--json` for machine-
readable output. The wire shape is identical to what `scry2 repl`
prints on stdout and what `scry2 serve` returns over a Unix socket
— so a tool that consumes `--json` works unchanged against any of
the three modes.

```bash
$ scry2 --json --index aosp.s2db stat
{"cmd":"stat","xrefs":215164,"syms":128628,"files":895,"inhs":1998,"calls":64093}
```

## Long-lived modes — REPL and serve

Each query verb costs ~10 ms of process startup + mmap before the
microsecond-scale lookup. For an LLM that runs hundreds of queries in
one session, that startup dominates. Two ways to amortize it.

### `repl` — stdin/stdout JSON loop (recommended)

```bash
$ scry2 --index aosp.s2db repl <<EOF
{"cmd":"stat"}
{"cmd":"def","name":"Binder","substr":true,"limit":3}
{"cmd":"callers","name":"clearCallingIdentity","substr":true,"max_hits":5}
EOF
{"cmd":"stat","xrefs":…}
{"cmd":"xrefs","groups":[…],"total":…}
{"cmd":"xrefs","groups":[…],"total":…}
```

One JSON request per line in, one JSON reply per line out. The
process opens the .s2db once and serves requests until stdin closes.
Subsequent queries cost ~5 µs of pipe overhead instead of ~10 ms of
fork+mmap. No socket, no daemon, no system state — when the parent
closes the pipe, scry2 exits.

### `serve` — daemon over a Unix socket (rare)

```bash
# Start the daemon once
scry2 --index aosp.s2db serve --socket /tmp/scry2.sock &

# Now any number of processes can query the warm Index:
scry2 --socket /tmp/scry2.sock --json def Binder --substr
scry2 --socket /tmp/scry2.sock --json callers clearCallingIdentity --substr
```

Pick this only when *N unrelated processes* need to share one warm
Index (e.g. multiple developers on a shared host). For one LLM, REPL
is leaner.

## Query verbs — by example

### `stat` — sanity check

```bash
$ scry2 --index scry2-smoke.s2db stat
xrefs:  215164
syms:   128628
files:  895
inhs:   1998
calls:  64093
```

### `def NAME` — definition site(s)

The `# header` line shows the symbol's human FQN (from `/kythe/edge/named`
or `/kythe/code` MarkedSource) rather than the raw VName ticket, so
`def`/`ref`/`callers`/`members`/`super`/`sub`/`inheritance` all render
readable names. A symbol with no FQN alias (e.g. a C++ builtin) still
shows its raw ticket — there is no human name to render.

Exact match on the Kythe VName-string (or any `/kythe/edge/named`
alias):

```bash
$ scry2 --index scry2-smoke.s2db def \
    'kythe:c++:android.googlesource.com/platform/superproject###__builtin_remainder#n#builtin'
# kythe:c++:…###__builtin_remainder#n#builtin  [?/cxx]
hits=0
```

(Builtins have no source-level decl, so 0 is correct.)

Substring search when you don't know the full name:

```bash
$ scry2 --index scry2-smoke.s2db def __builtin_memcpy --substr --limit 3
# kythe:c++:…###__builtin___memcpy_chk#n#builtin  [?/cxx]
# kythe:c++:…###__builtin_memcpy#n#builtin  [?/cxx]
hits=0
```

### `ref NAME` — every reference

```bash
$ scry2 --index scry2-smoke.s2db ref __builtin___memcpy --substr --limit 1 --max-hits 3
# kythe:c++:…###__builtin___memcpy_chk#n#builtin  [?/cxx]
  ref  prebuilts/.../usr/include/x86_64-linux-gnu/bits/string3.h@1588
  call prebuilts/.../usr/include/x86_64-linux-gnu/bits/string3.h@1588
hits=2
```

The `# header` line gives the symbol metadata `[kind/language]`. Each
body line is `  <role> <file>@<byte-offset>`.

### `callers NAME` — call sites only

Same as `ref --substr` but filters to `role=CALL`:

```bash
$ scry2 --index scry2-smoke.s2db callers __builtin___memcpy --substr --max-hits 3
# kythe:c++:…___memcpy_chk#n#builtin  [?/cxx]
  call prebuilts/.../bits/string3.h@1588
hits=1
```

### `--substr` matching — exact, case-sensitive, and `-i`

`def`, `ref`, and `callers` resolve a NAME two ways:

* **Default (no `--substr`)** — an exact-FQN binary search. It resolves
  **every** symbol of that exact name (all overloads, all per-jar copies,
  all language-pair variants), not a single one. This is the fast path —
  prefer it for hot queries; use `names NAME` to find the exact FQN.
* **`--substr`** — matches NAME against any substring of the qualified
  symbol ticket, backed by a **compressed trigram index** (the query
  intersects the needle's trigrams by galloping over the smallest posting
  list, then verifies each candidate). Sub-millisecond warm for typical
  needles, low-ms worst case. It is **case-SENSITIVE by default** (code
  identifiers are case-significant). Add `-i` / `--ignore-case` to fold
  ASCII case so the needle matches regardless of case (same speed — the
  index is a case-folded filter either way). A needle shorter than 3
  chars has no trigram and falls back to a parallel `memchr::memmem`
  linear scan.

```bash
# Case-sensitive (default): matches "Binder", "IBinder", "BinderProxy"
$ scry2 --index aosp.s2db def Binder --substr --limit 5

# Case-insensitive: also matches "binder", "BINDER", "rebinder"
$ scry2 --index aosp.s2db def binder --substr -i --limit 5
```

When a result hits the `--limit` cap the output prints a truncation
indicator on stderr — `(showing N; --limit cap reached, more exist —
raise --limit)` — so a capped count is never mistaken for the whole truth.

`ref`/`callers --substr` aggregate edges across all matching symbols,
capped at 64 by default (raise with `--limit`); a broader match returns a
fast partial flagged `truncated`. Prefer an exact FQN when you have it.

### `super NAME` — direct supertypes

`super android.os.Binder` returns the Java `IBinder` interface from
the `/kythe/edge/extends` edges. Each related symbol is resolved to its
FQN and its definition site (`path@offset`, preferring a DEF over a
DECL):

```bash
$ scry2 --index aosp.s2db super 'kythe:java:android##.../Binder.java#…'
android.os.IBinder  frameworks/base/core/java/android/os/IBinder.java@1234
hits=1
```

Per-jar duplicate copies of one logical supertype (same VName signature,
different build-variant path) are deduped to a single hit.

### `sub NAME` — direct subtypes

```bash
$ scry2 --index aosp.s2db sub 'kythe:java:android##.../IBinder.java#…'
android.os.Binder  frameworks/base/core/java/android/os/Binder.java@2048
android.os.IBinder.Stub  frameworks/base/core/java/android/os/IBinder.java@4096
android.os.IBinder.Stub.Proxy  frameworks/base/core/java/android/os/IBinder.java@8192
hits=3
```

An anonymous / local subtype with no name row renders as
`anon@<path>@<off>` rather than an FQN — a concrete locator instead of a
bare ticket. Per-jar duplicates are deduped.

### `callgraph NAME --direction up|down|both [--depth N]`

Transitive walk over the calls table. Defaults: `--direction up`,
`--depth 3`, `--max-syms 200`.

Output is a **BFS spanning tree** — every node carries an `id` and a
`parent` pointing at the node that discovered it. Walk `parent`
pointers back to the root to reconstruct exact discovery paths. The
root has `parent: null` (JSON) or `parent=-` (human). Each node also
reports its `kind` (`fn`, `type`, `var`, …) and its `def` location
(`path@off`, when the index has a DECL/DEF) — the same fields every
other verb reports. The `def` lets you byte-verify any node against
source; together with `kind` it makes a mis-attributed hop (a `type`
or `var` where you expect `fn`, or a node whose location lands in an
implausible file) visible instead of silent.

```bash
$ scry2 --index aosp.s2db callgraph \
    'kythe:java:android##.../Binder.java#clearCallingIdentity()' \
    --direction up --depth 2
  id=0   parent=-   hop=0 root  fn        clearCallingIdentity  .../IPCThreadState.cpp@17986
  id=1   parent=0   hop=1 up    fn        ActivityManagerService.startActivityAsUser  .../AMS.java@1694
  id=2   parent=0   hop=1 up    fn        BroadcastQueueImpl.deliverToReceiverLocked  .../BroadcastQueueImpl.java@2051
  id=3   parent=1   hop=2 up    fn        ActivityStarter.execute  .../ActivityStarter.java@2412
hits=3
```

Reading the tree:

* `id=1`, `id=2` are direct callers of `clearCallingIdentity` (`parent=0`).
* `id=3` is a transitive caller — reached via `id=1` (`ActivityStarter
  → ActivityManagerService → clearCallingIdentity`).
* If you see the same name appear under multiple parents in a `--both`
  walk, those are genuinely different discovery paths.

JSON shape (canonical):

```json
{
  "cmd": "callgraph",
  "nodes": [
    {"id": 0, "parent": null, "hop": 0, "dir": "root", "kind": "fn", "name": "clearCallingIdentity", "def": "frameworks/native/libs/binder/IPCThreadState.cpp@17986"},
    {"id": 1, "parent": 0,    "hop": 1, "dir": "up",   "kind": "fn", "name": "AMS.startActivityAsUser", "def": ".../AMS.java@1694"},
    {"id": 2, "parent": 0,    "hop": 1, "dir": "up",   "kind": "fn", "name": "BroadcastQueueImpl.deliverToReceiverLocked", "def": ".../BroadcastQueueImpl.java@2051"},
    {"id": 3, "parent": 1,    "hop": 2, "dir": "up",   "kind": "fn", "name": "ActivityStarter.execute", "def": ".../ActivityStarter.java@2412"}
  ],
  "total": 3,
  "truncated": false
}
```

Nodes are emitted in BFS order (parents always before children), so a
streaming consumer can build the tree on the fly.

`--direction down` shows what NAME calls. `--direction both` walks
both directions from the root — `dir: "up"` and `dir: "down"` mark
each edge. `--max-syms 200` caps total nodes so a hub function with
10 000+ callers doesn't run away.

AOSP compiles one logical function into many build-variant stub copies,
and the call edges aren't mirrored across them. `callgraph` **unions**
the call edges across all duplicate copies of each node, and **dedups**
duplicate roots (so `callgraph parseInt` doesn't flood ~18 identical
`parent=-` roots), keeping one node per logical function.

### `inheritance NAME --direction up|down|both [--depth N]`

The type-hierarchy mirror of `callgraph`, with the same id/parent/hop
forest output. `up` walks transitive supertypes (extends/implements),
`down` walks transitive subtypes, `both` does both. `--depth` and
`--max-syms` bound the walk like `callgraph`. Each node carries its FQN
and a `def` location (`path@offset`); anonymous/local types with no name
row render as `anon@<path>@<off>`.

```
# Whole ancestor chain of a type
$ scry2 --index aosp.s2db inheritance android.os.Bundle --direction up
# Everything that extends/implements an interface
$ scry2 --index aosp.s2db inheritance android.os.IBinder --direction down
```

AOSP compiles one logical type into many build-variant stub copies, and
the inheritance edges are not mirrored across them. `inheritance` (like
`callgraph`) **unions** the edges across all duplicate copies of a node,
so hub types like `Thread` / `HashMap` resolve correctly instead of
returning empty when the first duplicate happened to carry no edges.
Logical-duplicate nodes are themselves deduped to one node in the forest.

For ambiguous names prefer the exact FQN: `--substr` can also match
type-application syms (`const(T)`, `T&`) as roots.

### `type NAME` — resolved type of a symbol

The compiler-resolved type, including deduced `auto`/`var` and concrete
generic/template instantiations (not the syntactic token). C++ and Java.
Java fields carry their declared type here too.

```
$ scry2 --index aosp.s2db type some.Var      #=> java.util.List<java.lang.String>
$ scry2 --index aosp.s2db type someCxxVar    #=> const Box<int> &
$ scry2 --index aosp.s2db type android.os.Bundle.EMPTY  #=> android.os.Bundle
```

### `sig NAME` — full signature with parameter names

Function signature with rendered parameter types **and names**, for both
C++ and Java:

```
$ scry2 --index aosp.s2db sig setEnabled   #=> void setEnabled(boolean enabled)
$ scry2 --index aosp.s2db sig pick         #=> java.util.List<java.lang.String> pick(int idx, java.lang.String key)
$ scry2 --index aosp.s2db sig clearCallingIdentity  #=> long clearCallingIdentity()
```

A zero-parameter function renders as `<ret> name()` (it still needs both
a return type and a name; a synthetic zero-param node with neither — e.g.
the JVM `<clinit>` — gets no row). `def` also prints `sig:` inline when
present. Type-variable and array types render in the indexer's own form
(e.g. `array<int>`, `T.m.K`). Honest emptiness: a sym with no
parameter/return info renders no signature.

### `members NAME` — what a type declares

The methods and fields a type/record/interface declares (reverse of
"member is a child of its class"), each with its kind and — for methods —
signature. Fields carry their FQN name and type, listed with kind
`[field/...]`:

```
$ scry2 --index aosp.s2db members android.os.Bundle
# android.os.Bundle
  android.os.Bundle.EMPTY [field/java]  android.os.Bundle
  android.os.Bundle.putByteArray [fn/java]  void putByteArray(java.lang.String key, byte[] value)
  ...
```

Per-jar duplicate copies of one logical member (identical rendered
name + kind + signature) are deduped.

## Path filters — `--in`, `--not-in`, `--def-in`

All three are **substring matches**
against Kythe's path field. **No realpath, no `--source-root`, no
mixing of absolute/relative paths** — what the indexer stored is what
you match against.

| flag | applies to | filters out |
|---|---|---|
| `--in SUBSTR` | the call/ref site's file path | rows whose path doesn't contain SUBSTR |
| `--not-in SUBSTR` | the call/ref site's file path | rows whose path contains SUBSTR |
| `--def-in SUBSTR` | the target symbol's decl/def file path | symbols whose def isn't in SUBSTR |

Useful combinations:

```bash
# All call sites of methods defined in frameworks/base/services/core
scry2 callers clearCallingIdentity --substr --def-in services/core

# Same query, but show me callers in frameworks/base only — drop the
# rest of the corpus
scry2 callers clearCallingIdentity --substr --def-in services/core \
    --in frameworks/base

# All refs of a name except those in test files
scry2 ref ProcessRecord --substr --not-in /test/
```

## Recall vs precision — how to think about it

scry2 reports every edge Kythe emitted. There is no post-filtering
pass, no heuristic re-attribution. That means:

* **Recall is bounded by what Kythe indexes.** If a kzip is missing
  source files (a common AOSP build issue), the indexer never emits
  entries for those translation units and scry2 has nothing to
  return. The fix is at the kzip layer, not at the query layer.
* **Precision is bounded by Kythe's VName logic.** If cxx_indexer
  emits 199 anchors targeting `incStrong`, all 199 land in the
  `xrefs` table. A user asking "who calls incStrong" gets 199 hits.
  An LLM with surrounding code context can prune; a CI gate that
  needs zero false positives should add its own filter on top.

The output is structured (`--json`) so you can post-filter with `jq`
or any tool — e.g. drop call sites in test files, or restrict to
results whose target is defined in a specific module.

## Performance — what to expect on a real index

Numbers from `/usr/bin/time -v` on the 30 MB cxx smoke index, warm:

| op | wall | peak RSS | major page faults |
|---|---|---|---|
| `def NAME` (exact lookup) | < 1 ms | 20 MB | 0 |
| `def NAME --substr` (over 128 k syms) | 10–80 ms | 25 MB | 0 |
| `ref NAME --substr` with 500 hits | 20 ms | 26 MB | 0 |
| `callgraph NAME --depth 3` | 5–30 ms | 25 MB | 0 |

For a full AOSP index (~1 GB on disk, ~80 M xrefs) the bench at
`docs/BENCH.md` projects warm point lookups at 1.8 µs and warm prefix
scans at 3.7 µs — the queries themselves get dwarfed by process
startup. To eliminate startup overhead for an LLM running many
queries in one session, use `scry2 repl` (see above).
