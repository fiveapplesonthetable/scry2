# scry2 ↔ Kythe integration

This is the only file in scry2 that talks about specific Kythe edges,
proto field numbers, and indexer quirks. The rest of the code consumes
the contract this document describes.

## The contract

scry2 reads a stream of **delimited `kythe.proto.Entry` messages** on
stdin (or from a file). That stream is the canonical output of every
Kythe v0.0.75 indexer.

Each Entry is one of two shapes:

* **Node fact** — `(source = VName, fact_name = "/kythe/…", fact_value = bytes)`
* **Edge** — `(source = VName, edge_kind = "/kythe/edge/…", target = VName)`

scry2's `kythe.rs` hand-decodes both shapes from the proto wire format
(no protobuf-codegen dependency). Roughly 150 lines.

## VName → symbol identity

A `VName` has five fields: `signature`, `corpus`, `root`, `path`,
`language`. scry2 maps it to a stable `u64` sym via:

```
sym = xxHash64("kythe:<language>:<corpus>#<root>#<path>#<signature>")
```

Two VNames that differ in any field hash to a different sym. The
canonical string is what scry2 stores as the sym's primary name; FQN
aliases come from `/kythe/edge/named` (see below).

## Node facts scry2 consumes

| fact_name | purpose | what we do |
|---|---|---|
| `/kythe/node/kind` | tags a node as `anchor`, `function`, `record`, `variable`, `package`, `field` | marks anchor accumulators; sets `kind` on symbol metadata |
| `/kythe/loc/start` | anchor start byte offset (ASCII decimal) | populates `AnchorAccum.start` |
| `/kythe/loc/end` | anchor end byte offset | populates `AnchorAccum.end` — needed for body-anchor extents |

Everything else is ignored. We don't decode `/kythe/code` (the
MarkedSource pretty-name proto) — instead we get FQNs via the `named`
edge below.

## Edges scry2 consumes

| edge_kind | role | what we do |
|---|---|---|
| `/kythe/edge/defines/binding` | DECL | xref row at `(target_sym, DECL, file, anchor_start)` |
| `/kythe/edge/defines` | DEF | xref row at `(target_sym, DEF, …)` AND mark this anchor as a *body anchor* — its `start..end` is used for callgraph attribution |
| `/kythe/edge/ref` | REF | xref row + callgraph call-site queued |
| `/kythe/edge/ref/call` | CALL | xref row + callgraph call-site queued |
| `/kythe/edge/ref/writes` | REF | xref row only |
| `/kythe/edge/ref/imports` | REF | xref row only |
| `/kythe/edge/extends`, `extends/public`, `extends/protected`, `extends/private` | — | `inhs` row `(child = source, parent = target)` |
| `/kythe/edge/overrides`, `satisfies` | — | `inhs` row |
| `/kythe/edge/named` | — | register `target.signature` as a human-typeable alias for the source sym — that's how `scry2 def android.os.Binder.clearCallingIdentity` resolves without `--substr` |
| `/kythe/edge/completes`, `completes/uniquely` | — | DEFN→DECL bridge captured for cxx, see notes below |

scry2 strips `.N` ordinal suffixes on edge kinds (e.g.
`/kythe/edge/childof.42` is treated as `/kythe/edge/childof`) per the
Kythe convention for repeated edges.

## Edges scry2 intentionally ignores

* `/kythe/edge/childof` — cxx_indexer uses this to nest sym SCOPES
  (namespace → namespace, class → namespace). It does NOT connect
  anchors to their enclosing function, so chasing it returns zero
  matches for "what calls X". We use body-anchor offset containment
  instead.
* `/kythe/edge/childof/context` — same family, scope-only.
* `/kythe/edge/ref/expands`, `ref/expands/transitive`, `ref/implicit`
  — macro-expansion noise. Useful for some pipelines, not for the
  six LLM-shaped queries scry2 cares about
  (def / ref / callers / super / sub / callgraph).
* `/kythe/edge/typed`, `param`, `tparam`, `documents`, `aliases` —
  type-shape edges. Not needed for def/ref/callers/super/sub/callgraph.

## The body-anchor trick

Cxx_indexer (and Java's indexer) emit *two* anchors per function:

1. **Binding anchor** at the function name only — narrow, used for
   "jump to def".
2. **Body anchor** covering the entire function definition body —
   has a `/kythe/edge/defines` edge to the same sym.

scry2 stores `(file_id, start, end, sym)` for every body anchor. When a
call anchor at `(file, off)` lands, we binary-search the body-anchors
sorted by `(file_id, start)` for the smallest containing range — that's
the enclosing function. Nested lambdas / inner classes work
correctly because "smallest containing" wins.

74% of call/ref anchors in our cxx smoke test resolved to an enclosing
body. The unresolved 26% are mostly refs inside header includes and
macro expansions that aren't inside any function body — those still
appear in `xrefs` (so `callers NAME` finds them), they just don't
contribute to the `calls` table that `callgraph` walks.

## Per-language indexer notes

### `cxx_indexer` (C / C++ / ObjC)

* Reads the kzip, writes delimited `Entry` protos to stdout.
* Crashes if all CUs in the kzip are non-cxx (e.g. a Java-only kzip).
  scry2's `from-kzip` redirects stderr to `/dev/null` and tolerates
  truncated entry streams, so this doesn't sink the multi-language
  ingest.
* Does NOT emit `/kythe/edge/named` — for cxx, the VName signature
  string already encodes the canonical USR (`c:@N@android@C@Binder@F@…`).
  Result: substring search works; FQN aliases don't help much for cxx.
* DEFN ↔ DECL identity for cross-TU forward declarations is emitted as
  `/kythe/edge/completes` from the .cpp anchor to the header anchor.
  scry2 captures these but currently doesn't bridge — see "Known
  limitations" below.

### `java_indexer.jar`

* Run as `java -Xmx8g -jar java_indexer.jar --temp_directory /tmp …`.
  The `--temp_directory` flag is mandatory for AOSP-shaped CUs that
  carry `--system <jdk_image>` — without it the indexer silently
  emits zero entries for those CUs.
* Emits `/kythe/edge/named` heavily — the source VName of every Java
  method is the JVM signature (`android/os/Binder.clearCallingIdentity()(I)V`)
  and the target's signature is the human FQN. This is what makes
  `scry2 def android.os.Binder.clearCallingIdentity` work in one shot.
* Body anchors for methods are emitted via `/kythe/edge/defines`
  (covers the full method body), so callgraph extraction works.

### `jvm_indexer.jar`

* For `.class` / `.jar` post-compile inputs. Same invocation shape as
  java_indexer. Used for Kotlin (no source-level public indexer
  available) and for Java cross-CU bytecode references.

### `go_indexer`

* Same shape as cxx_indexer: positional kzip arg, delimited entries to
  stdout. Emits `named` edges for Go FQNs (`pkg.Type.Method`).

### `proto_indexer` / `textproto_indexer`

* Use single-dash (`-index_file=`) and double-dash (`--index_file=`)
  flags respectively. Quirk of two different flag-parser libraries
  used inside the Kythe codebase.

## Kythe patches — when stock v0.0.75 isn't enough

For pure cxx / Go / proto corpora the stock Kythe v0.0.75 indexers
work as-is. **For AOSP Java + JVM cross-CU coverage** there are four
patches scry2 expects against the Kythe codebase — without them,
`services.core → Binder.clearCallingIdentity` returns 0 hits because
the `/kythe/edge/named` bridge never fires.

| # | file | what changes |
|---|---|---|
| 1 | `external.bzl` | `org.ow2.asm:asm:9.1 → 9.7.1` (Java 21 class major version 65 support — stock ASM 9.1 maxes at Java 17). |
| 2 | `KytheClassVisitor.java` | `ASM_API_LEVEL = Opcodes.ASM9` (was ASM7). Records, sealed classes, pattern-matching for switch all rely on ASM ≥ 8. |
| 3 | `ClassFileIndexer.java` | new `--default_corpus` flag on `jvm_indexer`. Stock VName corpus on raw `.jar`/`.class` inputs is `""` while `java_indexer`'s `named`-edge targets carry the build's actual corpus — same signature, different corpus → different VName → `write_tables` can't merge them. |
| 4 | `CompilationUnitPathFileManager.java` | derive `StandardLocation.CLASS_PATH` from `!CLASS_PATH_JAR!`-prefixed `required_input` entries when `JavaDetails` is absent on the CU. **Load-bearing.** Empirically: 0 → 1209 `named` edges to `android.os.Binder.*` JVM FQNs after the patch. |

The patch files live in `kythe-patches/` at the repo root. The
complete build repro (Bazel commands + bazel-bin output paths + the
order to apply the patches) is in
[`docs/DEVELOPMENT.md`](DEVELOPMENT.md#kythe-patches-required-for-aosp-java-jvm-cross-cu-coverage).

## Known limitations (honest)

* **completes bridge not applied yet.** scry2 captures cxx's
  `completes` edges during ingest, but doesn't currently rewrite call-
  site VNames from the header DECL to the .cpp DEFN. That means
  cross-translation-unit C++ refs split between forward-decl call
  sites and definition-site refs. Proper fix is a per-CU bridge map
  applied at xref emission. Tracked as a v0.2 follow-up.
* **No MarkedSource decode.** Kythe's `/kythe/code` fact carries a
  proto-encoded pretty-printed name. For cxx symbols this is what
  would give us "clear demangled identifier" without falling back to
  USR-style names. Substring search papers over it for now.
* **Source-level Kotlin gaps.** Public Kythe v0.0.75 ships no
  source-level Kotlin indexer. JVM bytecode mostly fills the gap, but
  lambda bodies / inline functions don't survive the round-trip.
* **Function body coverage = 74 % of refs.** Refs in macros and
  headers that aren't inside any function body don't contribute to
  the callgraph. They are still in the `xrefs` table.

## If you want to extend scry2 to a new edge kind

The whole contract is in `crates/scry2/src/kythe.rs`. Add to the
`edge_to_role` match for xref-table edges, the `is_inherit_edge` /
`is_named_edge` predicates for typed edges, or split out a new builder
method for entirely new tables. Tests live in `crates/scry2/src/lib.rs`
and exercise a hand-rolled Entry stream — easy to extend with one new
test per new edge.
