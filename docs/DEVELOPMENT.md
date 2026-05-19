# scry2 — development

Everything a contributor needs to set up a working dev environment,
build scry2 from source, run the test suite, and rebuild the Kythe
indexers (necessary for AOSP Java + Kotlin cross-CU coverage).

If you just want to *use* scry2, read [INSTALL.md](INSTALL.md) instead.

## Prereqs

| | min version | how |
|---|---|---|
| Rust toolchain | 1.75 (stable) | `rustup install stable` |
| Linux | any 5.x kernel | scry2 uses `posix_fadvise(POSIX_FADV_DONTNEED)` and `mmap(MAP_PRIVATE)` only |
| git | any | standard |
| Java | 21+ | only if you'll be running the Java/JVM indexers |
| Bazel | 6.x | only if you're rebuilding Kythe from source (see "Kythe patches" below) |
| `gh` (optional) | latest | for PRs, not required to build |

scry2 has five direct crate dependencies — `anyhow`, `clap`,
`memmap2`, `twox-hash`, `libc` — and nothing else. No build.rs, no
codegen, no C/C++ compilation. The release build takes 30 s clean.

## Build & test

```bash
git clone <repo>
cd scry2
cargo build --release -p scry2     # builds target/release/scry2
cargo test  --release -p scry2     # runs every unit test

# bench harness (optional; redb / rocksdb / mmap shoot-out)
cargo build --release -p scry2-bench
# add --features rocksdb-backend to include rocksdb (5-min C++ build)
```

Six tests cover:
* round-trip of every section (xrefs, syms, names, files, inhs, calls)
* FQN alias resolution via `add_alias`
* callgraph both directions + dedup
* substring name search
* hand-rolled Kythe Entry decode (proto wire format, no codegen)

If a test fails, the file at the top of the failure trace points at
the bug. Most bugs we've seen during dev fall in one of three buckets:

1. **Format-version drift** — change the wire layout, bump
   `format::VERSION` and `format::MAGIC`, regenerate test indexes.
2. **Sort-order divergence** — every table is binary-searched in the
   reader. If you add a new field to a row but don't update the writer's
   sort_unstable call, lookups become non-deterministic.
3. **Endianness mistakes** — every multi-byte key is BIG-endian on
   disk so memcmp gives lex order. New row types must follow this
   convention or the `lower_bound` helper will lie.

## Repository layout

```
scry2/
├── Cargo.toml                       # workspace
├── README.md
├── docs/
│   ├── INSTALL.md                   # for users
│   ├── USAGE.md                     # verb reference + examples
│   ├── DESIGN.md                    # architecture
│   ├── KYTHE.md                     # Kythe edge contract
│   ├── BENCH.md                     # backend tradeoff numbers
│   └── DEVELOPMENT.md               # this file
└── crates/
    ├── scry2/                       # main library + CLI
    │   ├── Cargo.toml
    │   └── src/
    │       ├── main.rs              # CLI dispatch
    │       ├── lib.rs               # public re-exports + tests
    │       ├── format.rs            # on-disk layout (header, row types, magic, sym_of)
    │       ├── reader.rs            # mmap + binary search
    │       ├── writer.rs            # IndexBuilder, atomic file rename
    │       └── kythe.rs             # Entry proto decoder + body-anchor extraction
    └── scry2-bench/                 # storage-backend benchmark
        ├── Cargo.toml
        └── src/{main, workload, stats, backend, be_mmap, be_redb, be_rocks}.rs
```

Every `.rs` file is under the 700-line cap (a scry-side convention
that survives here).

## Adding a new Kythe edge type

Two files to touch:

1. **`kythe.rs`** — add a predicate (`is_yourkind_edge`) and handle
   in `process_entry`. Patterns to copy:
   * Inheritance-shaped edges → `add_inherit(child, parent)` on the
     builder.
   * Symbol-meta edges (like `named`) → `add_alias(sym, alias)`.
   * Xref-shaped edges → return `Some(role_byte)` from `edge_to_role`,
     the existing anchor-flush path emits xref rows.
2. **`lib.rs`** — add a unit test that builds a hand-crafted Entry
   stream containing the new edge and asserts the right row count.
   The existing `decode_handcrafted_anchor` test is the template.

If your edge needs a new on-disk table (rare), three more files:

* **`format.rs`** — define the row struct + length constant + section
  offset in `Header` + bump `VERSION` and `MAGIC`.
* **`writer.rs`** — accumulate rows in `IndexBuilder`, sort + dedup
  in `finish`, page-align the section, write the bytes.
* **`reader.rs`** — slice accessor + binary-search lookup. Mirror
  `xrefs_slice` / `prefix_count` exactly so the cost stays O(log n).

## Kythe patches — required for AOSP Java + JVM cross-CU coverage

Public Kythe v0.0.75 ships indexer binaries that don't fully resolve
two AOSP-shaped scenarios:

* **Java 21 bytecode in framework.jar** — bundled ASM 9.1 can't
  read class file major version 65 (Java 21). `KytheClassVisitor`'s
  `ASM_API_LEVEL = ASM7` rejects records / sealed classes.
* **services.core → Binder cross-CU** — services.core CUs don't ship
  a `JavaDetails` proto, so javac falls into a "no classpath" state
  even though `required_input` carries every classpath jar's bytecode
  under the `!CLASS_PATH_JAR!` convention. With no resolved
  `MethodSymbol`, no `named` edge is emitted to the JVM FQN, and
  write_tables can't bridge the call.

scry's repo at `/mnt/agent/scry/docs/KYTHE_JVM_INDEXER_REBUILD.md`
holds the complete repro. The summary of the four-patch chain:

| # | file | change |
|---|---|---|
| 1 | `external.bzl` | bump `org.ow2.asm:asm:9.1` → `9.7.1` (Java 23 max class file). One line + `bazel run @unpinned_maven//:pin`. |
| 2 | `KytheClassVisitor.java` | `private static final int ASM_API_LEVEL = Opcodes.ASM9;` (was `ASM7`). ASM 9 understands records / sealed / pattern matching. |
| 3 | `ClassFileIndexer.java` | new `--default_corpus` flag on `jvm_indexer`. Stock VName corpus is `""` for raw `.jar`/`.class` inputs; `java_indexer`'s `named` edges target VNames with the corpus the build ships. Without the flag the two VNames don't match and write_tables can't merge them. |
| 4 | `CompilationUnitPathFileManager.java` | derive `StandardLocation.CLASS_PATH` from `!CLASS_PATH_JAR!`-prefixed `required_input` entries when `JavaDetails` is absent. **This is the load-bearing one** — it unblocks every services.core → framework.jar cross-CU edge. Empirically: 0 → 1209 `named` edges to `android.os.Binder.*` JVM FQNs after the patch. |

### Where the patches live

The canonical patch files are in **scry's** repo at
`kythe-patches/0001-asm.patch` through `0004-classpath.patch`. scry2
doesn't ship its own copies because the patches target the Kythe
codebase, not scry2 — they're a build-time prerequisite, not a
runtime one. We may mirror them into `scry2/kythe-patches/` once
v0.2 stabilises.

### Building patched Kythe from source

```bash
git clone https://github.com/kythe/kythe ~/dev/kythe
cd ~/dev/kythe
git apply /path/to/scry/kythe-patches/000{1,2,3,4}-*.patch
bazel run @unpinned_maven//:pin       # refresh maven_install.json after patch 1
bazel build //kythe/java/com/google/devtools/kythe/analyzers/java:indexer
bazel build //kythe/java/com/google/devtools/kythe/analyzers/jvm:indexer
# outputs land under bazel-bin/... — replace the jars in your Kythe
# release indexers/ dir.
```

For scry2 users **who only run cxx / Go / proto code**, the stock
v0.0.75 binaries work as-is. The patches are only needed for the
Java + JVM cross-CU story.

## Running on a real AOSP corpus

For development we use `aosp_cf_x86_64_phone.kzip` built by AOSP's
`build_kzip.bash`. Reference numbers (full corpus, 6 KB-72 KB CUs):

* cxx — ~250 k CUs, 12-18 hrs end-to-end with 36 workers
* java — ~50 k CUs, 4-6 hrs
* jvm — depends on classpath fan-out; typically 30-90 min

For a small loop, pick a single module's kzip from `/mnt/agent/aosp-out/soong/`
and feed it directly:

```bash
~/kythe/kythe-v0.0.75/indexers/cxx_indexer \
    one_module.kzip > /tmp/dev.entries
./target/release/scry2 index --entries /tmp/dev.entries -o /tmp/dev.s2db
./target/release/scry2 --index /tmp/dev.s2db stat
```

Iteration is fast: 30 MB cxx kzip → 489 MB entries → 30 MB `.s2db`
in 3 s.

## Code style — what the reviewer will flag

* Default to **no comments**. Add only when WHY isn't obvious — never
  WHAT.
* No `unwrap()` outside tests. Use `?` and the `anyhow::Context` shape
  the existing code uses.
* No `Vec<u8>` -> `String` conversions inside the hot path. The reader
  works on `&[u8]` slices and only utf8-decodes at the print boundary.
* Big endian on disk, big endian in transit. Little endian only in
  scratch in-memory state where ordering doesn't matter.
* Tests live alongside the code they cover. Format-level tests in
  `lib.rs`; verb-level tests in `main.rs` (none yet, follow-up).

## Filing PRs

1. Fork to your GH account.
2. Create a branch off `main`. Branch names: `kythe-<edge>`,
   `bench-<topic>`, `cli-<verb>`, `doc-<page>` — short, kebab-case.
3. Run `cargo test --release` and `cargo build --release` clean.
4. Open a PR with a clear "why" — what failure mode does this fix /
   what new edge does this expose. Include before/after numbers if
   the change touches the hot path or row format.
5. CI (when we add it) will rerun the bench gate at 10 M rows to
   confirm no warm-latency regression > 20%.

## Where scry and scry2 diverge

scry2 is built **alongside** scry — zero changes to scry's code. The
two share a Kythe install but otherwise are independent:

| | scry | scry2 |
|---|---|---|
| Storage | per-language sidecars + scry index dir | one `.s2db` mmap |
| Precision filter | yes (strict / lexical / clang-precise) | no |
| Build-graph reachability | yes (Soong / GN / kernel module graph) | no |
| Incremental updates | yes | no — full rebuild from kzip |
| Languages | cxx, java, jvm, go, proto, kotlin, rust (some via SCIP) | same six (Kythe-only) |
| LOC | ~25 000 across many crates | ~2 000 across two crates |
| Query verbs | def, ref, callers, callgraph, impact, prefix, fuzzy, grep, outline, coverage, stats, mcp, … | def, ref, callers, callgraph, super, sub, stat (+ build verbs) |

If your contribution wants something only scry has (precision
filter, module graph), send it to scry. If it's a new lean primitive
or a tighter Kythe-edge story, scry2 is the right home.
