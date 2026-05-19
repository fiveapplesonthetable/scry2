# scry2 — install

scry2 is **one Rust binary** plus a set of Kythe v0.0.75 indexer
binaries. No daemons, no databases, no system packages.

Two install paths, picked by what corpus you're indexing:

* **Pure C/C++/Go/proto corpora** → stock Kythe v0.0.75 is enough.
  Two curls. See [Path A](#path-a--stock-kythe-cgoproto) below.
* **AOSP Java + JVM cross-CU coverage** → you need patched Kythe
  (Java 21 class major version 65 + `--default_corpus` flag +
  classpath autoderivation). See [Path B](#path-b--patched-kythe-aosp-javajvm).

If you're not sure which you need, start with Path A and switch when
Java queries return 0 rows for symbols you know exist cross-CU.

## Path A — stock Kythe (C/C++/Go/proto)

```bash
# 1. Get scry2 itself (one binary, ~6 MB)
curl -fL https://github.com/fiveapplesonthetable/scry2/releases/latest/download/scry2-linux-x86_64 \
    -o /usr/local/bin/scry2
chmod +x /usr/local/bin/scry2

# 2. Get the stock Kythe v0.0.75 indexers (~200 MB)
mkdir -p ~/kythe && cd ~/kythe
curl -fL https://github.com/kythe/kythe/releases/download/v0.0.75/kythe-v0.0.75.tar.gz \
    | tar -xz

# 3. Done — smoke test
scry2 --version
scry2 from-kzip --kzip your.kzip --kythe-root ~/kythe/kythe-v0.0.75 -o your.s2db
scry2 --index your.s2db stat
```

That's it. Nothing else to configure.

## Path B — patched Kythe (AOSP Java/JVM)

Stock `jvm_indexer.jar` (a) can't read `framework.jar`'s Java 21 class
files and (b) emits no `/kythe/edge/named` bridge for cross-CU
classpath-only inputs. The four-patch chain that fixes both is documented
in [DEVELOPMENT.md → Kythe patches](DEVELOPMENT.md#kythe-patches-required-for-aosp-java-jvm-cross-cu-coverage).

> **Bundle planned for v0.1.0.** We intend to ship a pre-built
> `scry2-patched-kythe-jars-v0.0.75.tar.gz` (~60 MB, holds the
> patched `java_indexer.jar` and `jvm_indexer.jar` plus a `PATCHES.md`
> describing what changed and pointing at the Kythe LICENSE) as a
> companion GitHub Release asset. Install becomes:
>
> ```bash
> curl -fL .../scry2-linux-x86_64       -o /usr/local/bin/scry2 && chmod +x $_
> curl -fL .../kythe-v0.0.75.tar.gz     | tar -xz -C ~/kythe
> curl -fL .../scry2-patched-kythe-jars-v0.0.75.tar.gz \
>     | tar -xz -C ~/kythe/kythe-v0.0.75/indexers   # overlays the two jars
> ```
>
> Until we cut v0.1.0, build the patched jars yourself per the
> instructions below.

### Build the patched jars yourself (current path)

Prereqs: Bazel 6.x, Java 21+, ~10 GB free disk.

```bash
# 1. Stock Kythe first
mkdir -p ~/kythe && cd ~/kythe
curl -fL https://github.com/kythe/kythe/releases/download/v0.0.75/kythe-v0.0.75.tar.gz | tar -xz

# 2. Clone Kythe source + apply scry's four patches
git clone https://github.com/kythe/kythe ~/dev/kythe
cd ~/dev/kythe
git apply /path/to/scry/kythe-patches/000{1,2,3,4}-*.patch
bazel run @unpinned_maven//:pin                # refresh maven_install.json after Patch 1

# 3. Build the two patched jars
bazel build //kythe/java/com/google/devtools/kythe/analyzers/java:indexer
bazel build //kythe/java/com/google/devtools/kythe/analyzers/jvm:indexer

# 4. Overlay onto the stock release
cp bazel-bin/kythe/java/com/google/devtools/kythe/analyzers/java/indexer_deploy.jar \
   ~/kythe/kythe-v0.0.75/indexers/java_indexer.jar
cp bazel-bin/kythe/java/com/google/devtools/kythe/analyzers/jvm/indexer_deploy.jar \
   ~/kythe/kythe-v0.0.75/indexers/jvm_indexer.jar

# 5. scry2 install + smoke test
curl -fL https://github.com/fiveapplesonthetable/scry2/releases/latest/download/scry2-linux-x86_64 \
    -o /usr/local/bin/scry2 && chmod +x /usr/local/bin/scry2
scry2 from-kzip --kzip your-aosp.kzip --kythe-root ~/kythe/kythe-v0.0.75 -o aosp.s2db
scry2 --index aosp.s2db callers Binder.clearCallingIdentity --substr
```

Bazel build of the two jars takes ~10 min on this host.

## Build scry2 from source

If you're on an unsupported platform, want to track `main`, or are
contributing:

```bash
# Requirements: Rust 1.75 stable, Linux 5.x, git.
git clone https://github.com/fiveapplesonthetable/scry2
cd scry2
cargo build --release -p scry2          # → target/release/scry2 (~6 MB)
sudo cp target/release/scry2 /usr/local/bin/
```

The release build takes ~30 s clean. Five tiny deps (`anyhow`, `clap`,
`memmap2`, `twox-hash`, `libc`, `serde`, `serde_json`), no build.rs,
no codegen, no C/C++.

## 30-second smoke test

```bash
~/kythe/kythe-v0.0.75/indexers/cxx_indexer some.kzip \
    | scry2 index --entries - -o /tmp/smoke.s2db
scry2 --index /tmp/smoke.s2db stat
scry2 --index /tmp/smoke.s2db def main --substr --limit 5
```

If you see row counts > 0 and at least one match, you're set. Read
[USAGE.md](USAGE.md) for the full verb catalog.

## Uninstall

```
rm /usr/local/bin/scry2
rm -rf ~/kythe/kythe-v0.0.75 ~/dev/kythe
rm path/to/your.s2db
```

No package manager state, no config dirs, no daemons to stop.

## Cargo features

| feature | default | enables |
|---|---|---|
| (none) | yes | the `scry2` binary + library, mmap-only |
| `rocksdb-backend` (on the **bench** crate only) | no | the redb vs rocksdb vs mmap shoot-out documented in [BENCH.md](BENCH.md). Drags `librocksdb-sys` + a ~5-min C++ build the first time. Not needed to use scry2. |
