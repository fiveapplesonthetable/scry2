# scry2 — usage

A compact verb set over a single `.s2db` file — `def`, `ref`,
`callers`, `callgraph`, `super`, `sub`, `inheritance`, `type`, `sig`,
`members`, `names`, `stat`, plus the `repl` / `serve` long-lived modes.
Every example below was run on a real index built from a C++ test kzip
(`scry2-smoke.s2db`, 220 k xrefs, 128 k symbols, 64 k callgraph edges,
30 MB on disk).

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
| `-o, --out PATH` | output `.s2db` (default `scry2.s2db`). |

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
* **`--substr`** — a parallel linear scan (`memchr::memmem`) over the
  whole names table, slower but tolerant of partial names. It is
  **case-SENSITIVE by default** (code identifiers are case-significant).
  Add `-i` / `--ignore-case` to fold ASCII case so the needle matches
  regardless of case.

```bash
# Case-sensitive (default): matches "Binder", "IBinder", "BinderProxy"
$ scry2 --index aosp.s2db def Binder --substr --limit 5

# Case-insensitive: also matches "binder", "BINDER", "rebinder"
$ scry2 --index aosp.s2db def binder --substr -i --limit 5
```

`ref`/`callers --substr` aggregate edges across all matching symbols,
capped at 64 by default (raise with `--limit`); a broader match returns a
fast partial flagged `truncated`. Prefer an exact FQN when you have it.

### `super NAME` — direct supertypes

`super android.os.Binder` returns the Java `IBinder` interface from
the `/kythe/edge/extends` edges:

```bash
$ scry2 --index aosp.s2db super 'kythe:java:android##.../Binder.java#…'
android.os.IBinder
hits=1
```

### `sub NAME` — direct subtypes

```bash
$ scry2 --index aosp.s2db sub 'kythe:java:android##.../IBinder.java#…'
android.os.Binder
android.os.IBinder.Stub
android.os.IBinder.Stub.Proxy
hits=3
```

### `callgraph NAME --direction up|down|both [--depth N]`

Transitive walk over the calls table. Defaults: `--direction up`,
`--depth 3`, `--max-syms 200`.

Output is a **BFS spanning tree** — every node carries an `id` and a
`parent` pointing at the node that discovered it. Walk `parent`
pointers back to the root to reconstruct exact discovery paths. The
root has `parent: null` (JSON) or `parent=-` (human).

```bash
$ scry2 --index aosp.s2db callgraph \
    'kythe:java:android##.../Binder.java#clearCallingIdentity()' \
    --direction up --depth 2
  id=0   parent=-   hop=0 root  clearCallingIdentity
  id=1   parent=0   hop=1 up    ActivityManagerService.startActivityAsUser
  id=2   parent=0   hop=1 up    BroadcastQueueImpl.deliverToReceiverLocked
  id=3   parent=1   hop=2 up    ActivityStarter.execute
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
    {"id": 0, "parent": null, "hop": 0, "dir": "root", "name": "clearCallingIdentity"},
    {"id": 1, "parent": 0,    "hop": 1, "dir": "up",   "name": "AMS.startActivityAsUser"},
    {"id": 2, "parent": 0,    "hop": 1, "dir": "up",   "name": "BroadcastQueueImpl.deliverToReceiverLocked"},
    {"id": 3, "parent": 1,    "hop": 2, "dir": "up",   "name": "ActivityStarter.execute"}
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

### `inheritance NAME --direction up|down|both [--depth N]`

The type-hierarchy mirror of `callgraph`, with the same id/parent/hop
forest output. `up` walks transitive supertypes (extends/implements),
`down` walks transitive subtypes, `both` does both. `--depth` and
`--max-syms` bound the walk like `callgraph`.

```
# Whole ancestor chain of a type
$ scry2 --index aosp.s2db inheritance android.os.Bundle --direction up
# Everything that extends/implements an interface
$ scry2 --index aosp.s2db inheritance android.os.IBinder --direction down
```

For ambiguous names prefer the exact FQN: `--substr` can also match
type-application syms (`const(T)`, `T&`) as roots.

### `type NAME` — resolved type of a symbol

The compiler-resolved type, including deduced `auto`/`var` and concrete
generic/template instantiations (not the syntactic token). C++ and Java.

```
$ scry2 --index aosp.s2db type some.Var      #=> java.util.List<java.lang.String>
$ scry2 --index aosp.s2db type someCxxVar    #=> const Box<int> &
```

### `sig NAME` — full signature with parameter names

Function signature with rendered parameter types **and names**, for both
C++ and Java:

```
$ scry2 --index aosp.s2db sig setEnabled   #=> void setEnabled(boolean enabled)
$ scry2 --index aosp.s2db sig pick         #=> java.util.List<java.lang.String> pick(int idx, java.lang.String key)
```

`def` also prints `sig:` inline when present. Type-variable and array
types render in the indexer's own form (e.g. `array<int>`, `T.m.K`).
Honest emptiness: a sym with no parameter/return info renders no signature.

### `members NAME` — what a type declares

The methods and fields a type/record/interface declares (reverse of
"member is a child of its class"), each with its kind and signature.

```
$ scry2 --index aosp.s2db members android.os.Bundle
```

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
