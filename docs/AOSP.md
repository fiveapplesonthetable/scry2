# scry2 on AOSP — start-to-finish

End-to-end recipe for indexing an AOSP tree with scry2:
**checkout → kzip → patched Kythe → `.s2db` → queries**. No
sandbox-specific paths; assumes a clean AOSP checkout and a clean
`~/scry2-setup/` workspace.

## Prereqs (one-time host setup)

```bash
sudo apt-get update && sudo apt-get install -y \
    git build-essential curl ca-certificates \
    openjdk-21-jdk-headless \
    python3 zip unzip

# Bazel — needed once to build the patched Kythe jars.
curl -fLO https://github.com/bazelbuild/bazelisk/releases/latest/download/bazelisk-linux-amd64
sudo install bazelisk-linux-amd64 /usr/local/bin/bazel
```

Disk budget: AOSP checkout ~250 GB, AOSP build artifacts ~150 GB,
`all.kzip` ~40 GB, scry2 index ~3-5 GB. Plan for ~500 GB free on the
build volume.

## Step 1 — generate `all.kzip` from AOSP

AOSP ships its own kzip builder at `build/soong/build_kzip.bash`.
After a normal `repo sync`, from the source tree root:

```bash
# Standard AOSP env
source build/envsetup.sh
lunch aosp_cf_x86_64_phone-trunk_staging-userdebug   # or any target

# Tell the extractor which corpus to stamp on every VName.
export XREF_CORPUS=android.googlesource.com/platform/superproject
export DIST_DIR=$PWD/out/dist
export KZIP_NAME=aosp
export KYTHE_KZIP_ENCODING=proto

build/soong/build_kzip.bash
# → out/dist/aosp.kzip   (typically 30-50 GB for a full corpus)
```

The script `m`s every C++/Java/Kotlin/Go/proto target's
xref-extractor variant (`xref_cxx`, `xref_java`, `xref_kotlin`,
`xref_rust`, plus per-Go-module `go_extractor` runs), then
`merge_zips` glues every per-module kzip into one `all.kzip`. It
checks that at least 100 000 sub-kzips landed; below that it aborts
with `ERROR: Too few kzip files were generated`.

First run takes 2-4 hours on a 16-core host. Incremental rebuilds
after small source changes are minutes. The output is one fat zip
that holds every CU + every required-input source byte each indexer
needs.

## Step 2 — set up patched Kythe v0.0.75

scry2 needs four patches against stock Kythe to handle AOSP Java 21
bytecode in `framework.jar` and to wire cross-CU classpath
resolution. The patches live in this repo at `kythe-patches/`.

```bash
mkdir -p ~/scry2-setup && cd ~/scry2-setup

# Stock Kythe v0.0.75 release — gives us cxx/go/proto/textproto
# indexers + the merge_zips tool. We'll overlay our patched jars.
curl -fL https://github.com/kythe/kythe/releases/download/v0.0.75/kythe-v0.0.75.tar.gz | tar -xz

# Patched jars — clone Kythe at the v0.0.75 tag, apply the four
# patches, build with Bazel.
git clone --depth=1 -b v0.0.75 https://github.com/kythe/kythe.git kythe-src
cd kythe-src
git clone --depth=1 https://github.com/fiveapplesonthetable/scry2 ../scry2-patches
git apply ../scry2-patches/kythe-patches/000{1,2,3,4}-*.patch
bazel run @unpinned_maven//:pin                       # refresh maven_install after Patch 1
bazel build \
    //kythe/java/com/google/devtools/kythe/analyzers/java:indexer \
    //kythe/java/com/google/devtools/kythe/analyzers/jvm:indexer

# Overlay the patched jars on top of the stock release.
cp bazel-bin/kythe/java/com/google/devtools/kythe/analyzers/java/indexer_deploy.jar \
   ../kythe-v0.0.75/indexers/java_indexer.jar
cp bazel-bin/kythe/java/com/google/devtools/kythe/analyzers/jvm/indexer_deploy.jar \
   ../kythe-v0.0.75/indexers/jvm_indexer.jar
cd ..
```

Bazel first-build is ~10 min on this host. Subsequent rebuilds (e.g.
after patching) are seconds.

`docs/DEVELOPMENT.md` documents each patch in detail. Short form:

1. `external.bzl`: ASM `9.1 → 9.7.1` (Java 21 class file support)
2. `KytheClassVisitor.java`: `ASM_API_LEVEL = ASM9`
3. `ClassFileIndexer.java`: new `--default_corpus` flag
4. `CompilationUnitPathFileManager.java`: derive `CLASS_PATH` from
   `!CLASS_PATH_JAR!` `required_input` entries — load-bearing.

## Step 3 — install scry2

```bash
# Prebuilt (when v0.1.0 ships):
curl -fL https://github.com/fiveapplesonthetable/scry2/releases/latest/download/scry2-linux-x86_64 \
    -o /usr/local/bin/scry2 && chmod +x /usr/local/bin/scry2

# Or build from source:
git clone https://github.com/fiveapplesonthetable/scry2
cd scry2 && cargo build --release -p scry2
sudo cp target/release/scry2 /usr/local/bin/
```

## Step 4 — normalize the kzip (if mixed-encoding)

AOSP's `build_kzip.bash` emits a kzip with BOTH proto-encoded units
(`root/pbunits/`) and JSON-encoded units (`root/units/`), depending
on which extractor wrote which CU. Stock Kythe v0.0.75 indexers
crash hard on mixed-encoding kzips with `Malformed kzip: multiple
unit encodings but different entries`. Check with:

```bash
~/scry2-setup/kythe-v0.0.75/tools/kzip info \
    --input ~/aosp/out/dist/aosp.kzip
```

If it errors "both proto and JSON units found but are not identical",
normalize the kzip first:

```bash
scry2 normalize-kzip \
    --in  ~/aosp/out/dist/aosp.kzip \
    --out ~/aosp/out/dist/aosp-norm.kzip
```

scry2 walks every `root/pbunits/<sha>` and `root/units/<sha>`,
parses each (proto wire or proto3-JSON, AOSP emits both), re-encodes
every unit as proto, and writes a fresh single-encoding kzip with
every required-input file blob preserved. ~5-15 min for a full AOSP
kzip, output ~same size as input.

## Step 5 — build the `.s2db` index

The recommended path for a real AOSP slice (scope narrowed to the
layers worth querying, durable across a kill):

```bash
export ANDROID_BUILD_TOP=~/aosp
./scripts/aosp-from-kzip.sh ~/aosp/out/dist/aosp-norm.kzip \
    --in frameworks/base,frameworks/native,system/,art/,libcore/ \
    --snapshot-every 2000 \
    -o ~/scry2-setup/aosp.s2db
```

The wrapper bakes in the AOSP defaults — `KYTHE_ROOT`, language set
(cxx, java, jvm), JVM heap (12 g), the libcore `--patch-module`
quirk — and forwards everything else to `scry2 from-kzip`.

If the run gets killed (OOM, reboot, operator), re-run with
`--resume`:

```bash
./scripts/aosp-from-kzip.sh ~/aosp/out/dist/aosp-norm.kzip \
    --in frameworks/base,frameworks/native,system/,art/,libcore/ \
    --snapshot-every 2000 --resume \
    -o ~/scry2-setup/aosp.s2db
```

`--resume` opens `~/scry2-setup/aosp.s2db.partial.s2db` (a queryable
mid-run snapshot taken every `--snapshot-every` successful CUs)
plus `~/scry2-setup/aosp.s2db.partial.shas` (the list of CUs already
folded in) and continues with only the CUs not yet processed. The
partial files are removed once the final s2db is written.

Equivalent without the wrapper:

```bash
scry2 from-kzip \
    --kzip ~/aosp/out/dist/aosp-norm.kzip \
    --kythe-root ~/scry2-setup/kythe-v0.0.75 \
    --in frameworks/base,frameworks/native,system/,art/,libcore/ \
    --inject-cu-arg 'libcore/ojluni/src/main/java/::--patch-module=java.base=libcore/ojluni/src/main/java' \
    --jvm-heap 12g --workers $(nproc) \
    --snapshot-every 2000 \
    -o ~/scry2-setup/aosp.s2db
```

Per-CU dispatch: each compilation unit is extracted to a tiny
sub-kzip and fed to the matching indexer (`cxx_indexer`,
`java_indexer.jar`, `jvm_indexer.jar`) in its own subprocess.
One bad CU no longer takes the batch down; its stderr tail lands in
the per-language failure summary. Workers run in parallel and
serialize only on the in-memory builder.

Expect ~30 min wall on the AOSP slice above; ~1-2 h on a full
unfiltered AOSP kzip — long pole is java_indexer chewing through
the Java CUs.

## Step 5 — query

```bash
# Where is android.os.Binder.clearCallingIdentity defined?
scry2 --index ~/scry2-setup/aosp.s2db def \
    android.os.Binder.clearCallingIdentity

# Who calls it from services.core?
scry2 --index ~/scry2-setup/aosp.s2db callers \
    android.os.Binder.clearCallingIdentity \
    --in frameworks/base/services/core

# Transitive callers up to depth 3
scry2 --index ~/scry2-setup/aosp.s2db callgraph \
    android.os.Binder.clearCallingIdentity \
    --direction up --depth 3
```

For an LLM session, spawn one `repl` and pipe many requests:

```bash
scry2 --index ~/scry2-setup/aosp.s2db repl
```

## Common gotchas

* **`build_kzip.bash` exits with "Too few kzip files"** — your AOSP
  checkout didn't fully build. Run `m -j` first to make sure the
  tree is clean, then re-run `build_kzip.bash`.
* **java/jvm queries return 0 cross-CU.** You're on stock Kythe.
  Re-do Step 2 to overlay the patched jars.
* **`Unsupported class file major version 65`** in jvm_indexer logs
  — same root cause: stock ASM 9.1 can't read Java 21. Patched jar
  fixes it.
* **`from-kzip` fails with "no indexer binaries found"** — check
  `~/scry2-setup/kythe-v0.0.75/indexers/` actually has the six
  binaries (`cxx_indexer`, `java_indexer.jar`, `jvm_indexer.jar`,
  `go_indexer`, `proto_indexer`, `textproto_indexer`). If a path is
  wrong, pass `--langs cxx` to skip the missing ones.
