# scry2 — usage

Five verbs over a single `.s2db` file. Every example below was run on
a real index built from a C++ test kzip (`scry2-smoke.s2db`,
220 k xrefs, 128 k symbols, 64 k callgraph edges, 30 MB on disk).

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

Output during build looks like:

```
[from-kzip] running cxx
[from-kzip]   cxx: entries=1880842 anchors=276617 xrefs=220593 inherits=1998 aliases=0 calls=95884 (wall=3.0s, exit=Some(0))
[from-kzip] running java
[from-kzip]   java: entries=…
[from-kzip] writing — xrefs=… syms=… files=… inhs=… calls=…
[from-kzip] done in 8.42s → your.s2db (0.07 GB)
```

## Query verbs — by example

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

## Path filters — `--in`, `--not-in`, `--def-in`

These match scry's flag shape. All three are **substring matches**
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

scry2 reports every edge Kythe emitted. There is no precision filter,
no heuristic resolution. That means:

* **Recall is bounded by what Kythe indexes.** If a kzip is missing
  source files (a common AOSP build issue — see the kzip-build doc),
  the indexer never emits entries for those translation units and
  scry2 has nothing to return. The fix is at the kzip layer, not at
  the query layer.
* **Precision is bounded by Kythe's VName logic.** If cxx_indexer
  emits 199 anchors targeting `incStrong`, all 199 land in the
  `xrefs` table. A user asking "who calls incStrong" gets 199 hits.
  An LLM with surrounding code context can prune; a CI gate that
  needs zero false positives should add its own filter on top.

When you want to measure recall against a ground truth (e.g. scry's
strict output), the canonical comparison is:

```bash
scry        callers FOO --strict --index AOSP_SCRY  > scry.txt
scry2       callers FOO          --index AOSP_S2DB  | sort > scry2.txt
comm -12 scry.txt scry2.txt | wc -l   # intersection = true positives
comm -23 scry.txt scry2.txt | wc -l   # scry only    = scry2 missed
comm -13 scry.txt scry2.txt | wc -l   # scry2 only   = scry2 over-reported
```

Expect scry2 to over-report relative to scry's `--strict` (because
scry2 doesn't filter unresolved refs) and to never miss anything scry
finds (because scry2 indexes the same Kythe entries).

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
queries in one session, build scry2 as a stdin-driven REPL is a
v0.2 idea, currently not in scope.
