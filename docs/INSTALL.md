# scry2 — install

scry2 is **one Rust binary** plus the Kythe v0.0.75 release tarball
that hosts the indexer binaries. No daemons, no databases, no system
packages, no scripts to source. The shortest viable install is two
`curl` commands.

## Quickest path — prebuilt binary (recommended)

> Pinned release builds for x86_64-linux land on GitHub Releases when
> we cut tags. Until v0.1.0 ships there, build from source — see the
> next section.

```bash
# 1. Get scry2 itself (one binary, ~6 MB)
curl -fL https://github.com/fiveapplesonthetable/scry2/releases/latest/download/scry2-linux-x86_64 \
    -o /usr/local/bin/scry2
chmod +x /usr/local/bin/scry2

# 2. Get the Kythe v0.0.75 indexers (~200 MB)
mkdir -p ~/kythe && cd ~/kythe
curl -fL https://github.com/kythe/kythe/releases/download/v0.0.75/kythe-v0.0.75.tar.gz \
    | tar -xz

# 3. Done. Smoke test:
scry2 --version
scry2 from-kzip --kzip your.kzip \
    --kythe-root ~/kythe/kythe-v0.0.75 \
    -o your.s2db
scry2 --index your.s2db stat
```

That's the install. There is no `scry2 init`, no config file, no
state outside the `.s2db` you create.

## Build from source

If you're on an unsupported platform, want to track `main`, or are
contributing:

### Requirements

| | min version | install |
|---|---|---|
| Rust toolchain | 1.75 stable | `curl https://sh.rustup.rs -sSf \| sh` |
| Linux | any 5.x kernel | scry2 uses `posix_fadvise` + `mmap` only |
| git | any | standard |

### Build

```bash
git clone https://github.com/fiveapplesonthetable/scry2
cd scry2
cargo build --release -p scry2          # → target/release/scry2 (~6 MB)
sudo cp target/release/scry2 /usr/local/bin/  # optional
```

The release build takes ~30 s clean. Five tiny deps (`anyhow`, `clap`,
`memmap2`, `twox-hash`, `libc`), no build.rs, no codegen, no C/C++.

## Get the Kythe indexers

```bash
mkdir -p ~/kythe && cd ~/kythe
curl -fL https://github.com/kythe/kythe/releases/download/v0.0.75/kythe-v0.0.75.tar.gz \
    | tar -xz
# → ~/kythe/kythe-v0.0.75/indexers/{cxx,go,proto,textproto}_indexer
#   ~/kythe/kythe-v0.0.75/indexers/{java,jvm}_indexer.jar
```

scry2 only needs `kythe-v0.0.75/indexers/`. The `tools/`, `web/`,
etc. directories are unused.

### Optional: patched Kythe for AOSP Java + JVM cross-CU

Public v0.0.75 indexers fall short for AOSP Java 21 bytecode in
`framework.jar` and for services.core → Binder cross-CU edges.
If those scenarios matter for your corpus, you'll need to rebuild
Kythe with the four scry-developed patches. Full repro lives in
[`docs/DEVELOPMENT.md`](DEVELOPMENT.md#kythe-patches-required-for-aosp-java-jvm-cross-cu-coverage).

For pure cxx / Go / proto corpora the stock binaries work as-is.

## 30-second smoke test

```bash
# Pick any small C++ kzip — Kythe's own tests ship one, or AOSP's
# per-module kzips work.
~/kythe/kythe-v0.0.75/indexers/cxx_indexer some.kzip \
    | scry2 index --entries - -o /tmp/smoke.s2db
scry2 --index /tmp/smoke.s2db stat
# xrefs:  …
# syms:   …
# files:  …
# calls:  …
scry2 --index /tmp/smoke.s2db def main --substr --limit 5
```

If you see row counts > 0 and at least one match, you're set. Reach
for [USAGE.md](USAGE.md) for the full verb catalog.

## Uninstall

```
rm /usr/local/bin/scry2
rm -rf ~/kythe/kythe-v0.0.75
rm path/to/your.s2db
```

No package manager state, no config dirs, no daemons to stop.

## Cargo features

| feature | default | enables |
|---|---|---|
| (none) | yes | the `scry2` binary + library, mmap-only |
| `rocksdb-backend` (on the **bench** crate only) | no | the redb vs rocksdb vs mmap shoot-out documented in [BENCH.md](BENCH.md). Drags `librocksdb-sys` and a ~5-min C++ build the first time. Not needed to use scry2. |
