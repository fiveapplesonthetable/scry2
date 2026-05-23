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
canonical VName string is what scry2 stores as the sym's name in the
discovery index; FQN aliases come from `/kythe/edge/named` (Java/JVM/Go)
or `/kythe/code` MarkedSource (C++) — see below. At CU finalize, scry2
picks the cleanest human FQN among a sym's aliases and sets it as the
sym's **display name**, so `def`/`ref`/`callers`/`members`/`super`/`sub`/
`inheritance` render a readable FQN (`android.os.Binder.clearCallingIdentity`)
instead of the raw `kythe:...#<hash>` ticket. A sym with no FQN alias
(e.g. a C++ builtin) keeps the ticket as its display name — there is no
human name to show.

## Node facts scry2 consumes

| fact_name | purpose | what we do |
|---|---|---|
| `/kythe/node/kind` | tags a node as `anchor`, `function`, `record`, `variable`, `package`, `field` | marks anchor accumulators; sets `kind` on symbol metadata |
| `/kythe/subkind` | refines a node's kind | `field` / `constant` subkind promotes a `variable` node to the FIELD kind (java_indexer emits class fields as `variable` + `subkind=field`), so Java fields surface as `[field/...]` |
| `/kythe/loc/start` | anchor start byte offset (ASCII decimal) | populates `AnchorAccum.start` |
| `/kythe/loc/end` | anchor end byte offset | populates `AnchorAccum.end` — needed for body-anchor extents |

We also decode `/kythe/code` (the MarkedSource pretty-name proto):
`parse_marked_source_fqn` renders it into a flat FQN like
`android::Parcel::writeStrongBinder` and registers that as a sym
alias — this is the cxx path to FQN lookup, since cxx_indexer emits
no `named` edge. For Java/JVM/Go we get FQNs via the `named` edge
below. Everything else is ignored.

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
| `/kythe/edge/named` | — | register `target.signature` as a human-typeable alias for the source sym — that's how `scry2 def android.os.Binder.clearCallingIdentity` resolves without `--substr`, and the cleanest such alias becomes the sym's FQN display name |
| `/kythe/edge/completes`, `completes/uniquely` | — | C++ DEFN→DECL VName bridge: at CU finalize, every sym-keyed row of the `.cpp` definition VName is remapped to the `.h` declaration VName so the two unify on the queryable decl sym (see notes below) |
| `/kythe/edge/typed` | — | source sym → its type node; the type is rendered to a string (MarkedSource for named/Java-generic nodes, recursive `tapp` walk for C++ composites) and stored in the `typed` section |
| `/kythe/edge/childof` | — | member → enclosing parent; stored reversed as `childrev` `(parent, child)` for `members` |
| `/kythe/edge/param`, `param.N` | — | ordinal-ordered parameters of a function, used (with `typed`) to render the `sig` section — full signatures with param names |

The type nodes reached by `typed` carry compiler-resolved types — a
deduced `auto`/`var` resolves to its concrete type and a template/generic
to its concrete instantiation, because the indexers read the
post-resolution AST (Clang `QualType` / javac `Type`). `sig` renders
parameter names for **both C++ and Java** — each parameter node carries
its name and a `typed` edge to its type; the method's own `typed` edge
gives the return type.

scry2 strips `.N` ordinal suffixes on edge kinds (e.g.
`/kythe/edge/childof.42` is treated as `/kythe/edge/childof`) per the
Kythe convention for repeated edges.

## `childof` is consumed for `members`, NOT for callgraph

`/kythe/edge/childof` connects a child node to its enclosing parent (a
field/method childof its class, a class childof its package). scry2
records **every** childof edge into the reverse `childrev` table, then
`members NAME` filters at query time by the parent sym's kind so only a
real container (type / record / interface / package) lists members and
function-local children (a param childof its function) never leak.

What childof is **not** used for is callgraph attribution: cxx_indexer's
childof nests sym SCOPES (namespace → namespace, class → namespace), not
anchors to their enclosing function, so chasing it returns zero matches
for "what calls X". Call containment is reconstructed from body-anchor
offset containment instead (see "The body-anchor trick").

## Edges scry2 intentionally ignores

* `/kythe/edge/childof/context` — scope-only, not a membership edge.
* `/kythe/edge/ref/expands`, `ref/expands/transitive`, `ref/implicit`
  — macro-expansion noise. Useful for some pipelines, not for the
  navigation + comprehension queries scry2 serves (def / ref / callers /
  callgraph / super / sub / inheritance / type / sig / members).
* `/kythe/edge/tparam`, `documents`, `aliases`, `instantiates`,
  `specializes`, `influences` — type-shape and dataflow edges scry2 does
  not surface. (`typed` and `param` ARE consumed — they back the `type`
  and `sig` verbs; see the consume table above.)

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
  scry2's `from-kzip` captures the child's stderr tail through a pipe
  (`drain_tail`) for failure diagnosis and tolerates truncated entry
  streams, so this doesn't sink the multi-language ingest.
* Does NOT emit `/kythe/edge/named` — for cxx, the VName signature
  string already encodes the canonical USR (`c:@N@android@C@Binder@F@…`).
  Result: substring search works; FQN aliases don't help much for cxx.
* DEFN ↔ DECL identity for cross-TU forward declarations is emitted as
  `/kythe/edge/completes` from the .cpp definition node to the header
  declaration node. scry2 applies this bridge at CU finalize: every
  sym-keyed row buffered for the CU (xrefs, aliases, inherits, childof,
  calls, types, sigs) is remapped from the def VName's sym to the decl
  VName's sym, so `def <method FQN>` — which resolves through the
  declaration's FQN alias — finds the `.cpp` body location too, not just
  the `.h` declaration. Per-CU and `O(rows in this CU)`, since a
  definition VName is unique to the CU that contains it.

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

* **completes bridge is per-CU.** scry2 applies the cxx `completes`
  bridge at CU finalize, remapping the `.cpp` definition VName's rows to
  the `.h` declaration VName so def/ref over a method's FQN cover both
  sites. The bridge is scoped to the CU that carries the `completes`
  edge (a definition VName is unique to that CU). Refs from other TUs
  that see only the forward declaration already key off the decl VName,
  which is the canonical sym, so they unify too — but a cross-TU ref
  whose target VName matches neither the decl nor any bridged def in its
  own CU is not retro-bridged.
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
