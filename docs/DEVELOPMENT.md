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

Every `.rs` file is under the 700-line cap.

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

`kythe-patches/README.md` holds the per-patch detail. The summary of
the four-patch chain:

| # | file | change |
|---|---|---|
| 1 | `external.bzl` | bump `org.ow2.asm:asm:9.1` → `9.7.1` (Java 23 max class file). One line + `bazel run @unpinned_maven//:pin`. |
| 2 | `KytheClassVisitor.java` | `private static final int ASM_API_LEVEL = Opcodes.ASM9;` (was `ASM7`). ASM 9 understands records / sealed / pattern matching. |
| 3 | `ClassFileIndexer.java` | new `--default_corpus` flag on `jvm_indexer`. Stock VName corpus is `""` for raw `.jar`/`.class` inputs; `java_indexer`'s `named` edges target VNames with the corpus the build ships. Without the flag the two VNames don't match and write_tables can't merge them. |
| 4 | `CompilationUnitPathFileManager.java` | derive `StandardLocation.CLASS_PATH` from `!CLASS_PATH_JAR!`-prefixed `required_input` entries when `JavaDetails` is absent. **This is the load-bearing one** — it unblocks every services.core → framework.jar cross-CU edge. Empirically: 0 → 1209 `named` edges to `android.os.Binder.*` JVM FQNs after the patch. |

### Where the patches live

`kythe-patches/000{1,2,3,4}-*.patch` at the repo root. The
`kythe-patches/README.md` next to them documents each patch
individually.

### Building patched Kythe from source

```bash
git clone https://github.com/kythe/kythe ~/dev/kythe
cd ~/dev/kythe
git apply /path/to/scry2/kythe-patches/000{1,2,3,4}-*.patch
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

For a small loop, pick a single module's per-target kzip from
`out/soong/.intermediates/...` and feed it directly:

```bash
~/kythe/kythe-v0.0.75/indexers/cxx_indexer \
    one_module.kzip > /tmp/dev.entries
./target/release/scry2 index --entries /tmp/dev.entries -o /tmp/dev.s2db
./target/release/scry2 --index /tmp/dev.s2db stat
```

Iteration is fast: 30 MB cxx kzip → 489 MB entries → 30 MB `.s2db`
in 3 s.

### Reference run: full AOSP index

End-to-end command for an AOSP corpus after `build_kzip.bash` has
produced `out/dist/aosp.kzip`. Assumes the four Kythe patches are
applied (see Path B in [INSTALL.md](INSTALL.md)).

```bash
# Outputs:
#   /var/scry2/aosp.s2db                (~3-8 GB)
#   /var/scry2/aosp.s2db.tmp            (atomically renamed at the end)
# Logs:
#   stderr captured to ~/scry2-aosp.log
nohup scry2 from-kzip \
    --kzip /aosp/out/dist/aosp.kzip \
    --kythe-root ~/scry2-setup/kythe-v0.0.75 \
    --langs cxx,java,jvm \
    --jvm-heap 12g \
    -o /var/scry2/aosp.s2db \
    > ~/scry2-aosp.log 2>&1 &

# Watch progress:
tail -f ~/scry2-aosp.log
```

What this does:

1. Spawns `cxx_indexer aosp.kzip`, streams its stdout through
   ingest. ~2-3 hrs for the full AOSP corpus, ~150-250 M raw
   entries, ~80-120 M xref rows landing in the builder.
2. Then `java_indexer.jar` with `-Xmx12g --temp_directory`
   (the JDK system-modules unpack needs the temp dir). ~2-3 hrs.
3. Then `jvm_indexer.jar` with `-Xmx12g`. ~30-60 min.
4. Sorts every table, dedupes, atomic-renames the result into place.

End-to-end wall on a 36-vCPU host: 6-8 hours. Peak RSS during
ingest: 10-15 GB (mostly the in-flight xref/calls vectors before
sort). Disk peak: ~3-5 GB for the staged `.s2db.tmp`.

If you only want C++:

```bash
scry2 from-kzip ... --langs cxx -o aosp-cxx.s2db
```

If you only have a Java/JVM kzip:

```bash
scry2 from-kzip ... --langs java,jvm -o aosp-jvm.s2db
```

`scry2 from-kzip` is idempotent on `-o`: the output `.s2db.tmp` is
written first, fsynced, then renamed. A crash mid-build leaves the
old index untouched.

### Per-CU dispatch and CU-arg rewriting

`from-kzip` does NOT run each indexer once against the whole kzip.
Stock `cxx_indexer` segfaults mid-iteration on the first CU whose
argv contains a flag Clang's frontend rejects (e.g. AOSP's
soong-generated `-compiler ...`), and that crash takes the entire
batch down. Java/JVM indexers behave better but still drop coverage
when one CU fails — they emit zero entries for everything after the
failure and exit 0 silently.

Instead the orchestrator:

1. Decodes every unit out of the source kzip.
2. Filters by `--in` / `--not-in` against the unit's primary path
   (`source_file[0]` or `required_input[0].path`).
3. Routes each surviving CU by `v_name.language`:
   `c++→cxx_indexer`, `java→java_indexer.jar`,
   `jvm→jvm_indexer.jar`, `go→go_indexer`,
   `protobuf→proto_indexer`, `textproto→textproto_indexer`.
4. For each CU, calls `kzip::SubKzipWriter::extract` to build a
   tiny single-CU sub-kzip under `--staging` (`$SCRY_TMP_DIR` or
   `/mnt/agent/tmp` by default), then spawns the right indexer
   against it. One bad CU no longer kills the run; its failure
   stderr tail is captured in the per-language summary.
5. The driver streams the indexer's stdout through `ingest_tolerant`
   into a shared `IndexBuilder`, drains the child's stderr tail in
   a thread (avoiding pipe-fill blocks on chatty CUs), then deletes
   the sub-kzip + per-CU JVM temp dir.

### Indexer-specific argv requirements

The Kythe v0.0.75 indexers each have an idiosyncratic invocation
shape. Mismatched argv produces silent zero-entry runs more often
than hard errors, so the orchestrator handles each shape explicitly
(see `build_indexer_command` in `crates/scry2/src/main.rs`):

* **`cxx_indexer <kzip>`** — positional kzip; emits Entry protos
  to stdout. No flags needed when the kzip's `argument` is well-
  formed Clang. Stock AOSP kzips occasionally carry Soong-side
  flags Clang rejects; per-CU dispatch isolates those failures.
* **`java -Xmx<heap> -jar java_indexer.jar --ignore_empty_kzip
  --temp_directory <dir> <kzip>`** — the `--temp_directory` flag is
  mandatory whenever the CU's javac args carry
  `--system <jdk_image>` (every modern AOSP build does). Without it
  `CompilationUnitPathFileManager.setSystemOption` raises
  `IllegalArgumentException` and the indexer silently emits zero
  entries (exit 0). We allocate one temp dir per CU under
  `--staging` so cleanup is bounded.
* **`java -Xmx<heap> -jar jvm_indexer.jar --ignore_empty_kzip
  --temp_directory <dir> <kzip>`** — same argv shape as
  java_indexer; reads class-file CUs instead of source.
* **`go_indexer <kzip>`** — positional kzip.
* **`proto_indexer -index_file=<kzip>`** — single-dash gflags.
* **`textproto_indexer --index_file=<kzip>`** — double-dash flags.

`--jvm-heap` sizes the `-Xmx` for both java and jvm indexers. AOSP's
services.core / framework batches blow past 2g building javac line
maps; 8g handles every observed AOSP CU; 12-16g for pathological
template-heavy units.

### CU-arg injection (`--inject-cu-arg`)

Some kzip CUs need a compiler flag the extractor didn't capture —
the indexer otherwise silently emits zero entries or fails with a
javac error. Rather than hard-code corpus-specific knowledge in the
scry2 binary, `from-kzip` accepts a repeatable
`--inject-cu-arg PREFIX::ARG` flag: any CU whose primary source path
starts with `PREFIX` gets `ARG` prepended to its compiler argv. The
flag is generic; corpus-specific rules live in wrapper scripts.

The most common AOSP-specific need is the **libcore quirk**: files
under `libcore/ojluni/src/main/java/java/` are Android's
implementation of the JDK's `java.base` module. Soong builds these
with `--patch-module=java.base=libcore/ojluni/src/main/java` when
emitting real java.base targets, but the `core-all-system-modules`
build variant whose CUs ship in AOSP's `aosp.kzip` omits the flag.
Without `--patch-module` javac sees the `--system <jdk_image>` JDK
already declaring `java.lang.String` and friends and rejects each
AOSP source file as a redefinition (`CompletionFailure: class file
for java.lang.String not found`).

`scripts/aosp-from-kzip.sh` is the AOSP-shaped wrapper. It reads
`$ANDROID_BUILD_TOP` (the AOSP checkout root) from the environment
and emits the right `--inject-cu-arg` rules before forwarding the
rest of the argv to `scry2 from-kzip`:

```bash
export ANDROID_BUILD_TOP=/aosp
./scripts/aosp-from-kzip.sh /aosp/out/dist/aosp.kzip \
    --in frameworks/base,frameworks/native,system/,art/,libcore/ \
    -o /var/scry2/aosp-core.s2db
```

Equivalently, with explicit `--inject-cu-arg`:

```bash
scry2 from-kzip \
    --kzip /aosp/out/dist/aosp.kzip \
    --kythe-root ~/scry2-setup/kythe-v0.0.75 \
    --inject-cu-arg 'libcore/ojluni/src/main/java/::--patch-module=java.base=libcore/ojluni/src/main/java' \
    --in frameworks/base,frameworks/native,system/,art/,libcore/ \
    -o /var/scry2/aosp-core.s2db
```

Implementation note: when a rule matches, the orchestrator clones
the `CompilationUnit`, prepends the missing flags via
`SubKzipWriter::extract_with`, drops the raw-proto cache so changes
actually land, and re-encodes the pbunit. CUs with no matching rule
take the fast path — raw proto bytes pass through verbatim.

If you find another patched-module / extra-flag quirk (e.g.
`art/runtime/openjdkjvmti/` needing `--patch-module=jdk.internal.vm.compiler=…`),
add another `--inject-cu-arg` line to `scripts/aosp-from-kzip.sh` —
no scry2 code change required.

### Filtered ingest via cheap primary-path peek

A full AOSP `aosp.kzip` carries ~118 k compilation units. Decoding
every one to apply an `--in frameworks/base,…` filter is wasted work
— the kept set is usually a few thousand. `read_units_filtered` peeks
just the proto-3 / JSON `source_file` (or first `required_input`)
without paying the multi-MB full decode, then drops CUs the filter
would reject anyway. On a normalized AOSP kzip this turns the walk
phase from ~3 min into ~30 s. The fallback path is also correct: if
the peek can't locate a recognizable primary path (corrupted or
non-standard CU layout), the orchestrator full-decodes and re-checks
the filter, so no CU is silently dropped.

### Resume on kill: `--resume` + rolling snapshots

Long AOSP runs get killed (host reboot, OOM, operator). `from-kzip`
maintains a rolling builder snapshot so the next invocation picks up
where the last one stopped.

```bash
# First run — gets killed mid-way through 12 000 CUs.
scry2 from-kzip --kzip aosp.kzip --kythe-root … \
    --in frameworks/base,frameworks/native,system/,art/,libcore/ \
    --snapshot-every 2000 \
    -o /var/scry2/aosp.s2db

# Restart picks up from /var/scry2/aosp.s2db.partial.{s2db,shas}.
scry2 from-kzip --kzip aosp.kzip --kythe-root … \
    --in frameworks/base,frameworks/native,system/,art/,libcore/ \
    --resume \
    -o /var/scry2/aosp.s2db
```

What `--snapshot-every N` does, in detail:

1. After every `N` *successful* CUs (`ingest_tolerant` returned Ok
   AND child exited 0 AND the entry stream was non-empty) one
   worker locks `snap_state_mu`, locks `builder_mu`, calls
   `FileIdAllocator::push_to(&mut builder)` to flush deferred
   `(path, file_id)` mappings, clones the live builder, and releases
   `builder_mu`. The clone is the snapshot's data.
2. The shas that became successful since the last snapshot move
   from `pending` into `committed` under the same lock — so the
   snapshot's `<out>.partial.shas` reflects exactly what the clone
   captured.
3. `write_snapshot` does `clone.finish(<out>.partial.s2db.tmp)` →
   atomic rename → write `<out>.partial.shas.tmp` → atomic rename
   (in that order so a crash between the two renames leaves the
   newer `.s2db` partial paired with a strictly older `.shas`,
   which `--resume` rejects with a clear error rather than silently
   double-counting).
4. Empty / failed CUs DO NOT get a sha. On resume those CUs are
   re-run — safe, because a failed CU contributed zero rows the
   first time around.

What `--resume` does:

1. If `<out>.partial.s2db` and `<out>.partial.shas` are both
   present, open the s2db as an `Index`, then
   `IndexBuilder::populate_from_index(&ix)` replays every
   xref / sym / file / inherit / call / alias back into a fresh
   builder via the reader's `iter_*` methods.
2. The shas file populates a `HashSet<String>` skip set; the plan
   loop drops every CU whose sha is in that set.
3. Indexing continues from where the snapshot left off, taking
   further snapshots every `--snapshot-every` successes.
4. On final `builder.finish(<out>.s2db)` success, the `partial.*`
   files are removed.

Failure modes the design handles:
- **Mid-snapshot crash** between the `.s2db.tmp` rename and the
  `.shas.tmp` rename: the next `--resume` sees mismatched files
  (newer s2db, older shas) and bails with `partial state is
  incomplete` rather than double-counting.
- **Crash with no shas yet committed**: only one of the two files
  is present; same `--resume` failure path.
- **Both files absent under --resume**: starts fresh, prints a
  reassuring note.
- **Worker panic mid-CU**: `CleanupPath` Drop guards remove the
  sub-kzip / jvm_tmp paths from `--staging` even during unwind.
  External `kill -9` bypasses Drop, leaving a few files in
  staging which the next clean run wipes (`remove_dir_all(&staging)`
  at end of `cmd_from_kzip`).
- **`FileIdAllocator` state lost in snapshot**: handled by
  `push_to(&mut builder)` immediately before the clone — without
  this, mid-run snapshots would carry zero `files` rows and
  resumed `ref` queries couldn't resolve file paths.

Picking `--snapshot-every`: snapshot wall is dominated by the
builder clone (~1 s/GB of in-memory state) plus the `finish()`
write (~10 s/GB compressed serialization). The default `2000`
yields one snapshot per ~5 minutes of AOSP indexer wall — small
enough that a kill costs at most 5 min of redo work, large
enough that snapshots are <2 % overhead.

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

## Scope boundaries

scry2 intentionally stays narrow. It does **not** post-filter Kythe
output, does **not** parse the build graph for reachability, does
**not** wrap itself in MCP (REPL gives ~95% of MCP's value with ~5%
of the protocol surface — see [README](../README.md)). New
contributions that fit the lean-Kythe-edge story are welcome;
contributions that add a heuristic layer or a new query DSL are out
of scope for this repo.
