# scry2 — install

scry2 is one Rust binary plus an external Kythe v0.0.75 release for the
indexers. No daemons, no databases, no system packages.

## Requirements

| | version |
|---|---|
| Rust toolchain | 1.75+ (stable). `rustup install stable` |
| OS | Linux (only Linux is tested) |
| Kernel | any 5.x or later — uses `posix_fadvise(POSIX_FADV_DONTNEED)` for cold-cache eviction |
| Filesystem | anything that supports `mmap` (ext4, xfs, btrfs, tmpfs) |
| Kythe release | v0.0.75 — only for `from-kzip` / ingest |
| Java (optional) | JDK 21+ if you'll feed it Java/JVM kzips |

scry2 itself depends on five crates: `anyhow`, `clap`, `memmap2`,
`twox-hash`, `libc`. No protobuf codegen, no C/C++ build steps.

## Build

```bash
git clone <repo>
cd scry2
cargo build --release -p scry2
# binary:   target/release/scry2     (~6 MB)
# strip if you care: strip --strip-debug target/release/scry2
```

The bench crate is optional and only useful if you want to repro the
backend tradeoff numbers — see `docs/BENCH.md`.

## Install Kythe v0.0.75

scry2 doesn't ship indexers — it shells out to the per-language
binaries Google publishes in the Kythe release. Download once and
point scry2 at the unpacked directory:

```bash
mkdir -p ~/kythe && cd ~/kythe
curl -fLO https://github.com/kythe/kythe/releases/download/v0.0.75/kythe-v0.0.75.tar.gz
tar -xzf kythe-v0.0.75.tar.gz
# Now ~/kythe/kythe-v0.0.75/indexers/ holds:
#   cxx_indexer  go_indexer  proto_indexer  textproto_indexer
#   java_indexer.jar  jvm_indexer.jar
```

scry2 only needs `indexers/` to exist; the `tools/`, `web/`, etc.
directories are unused.

## A 5-line smoke test

```bash
# 1. Capture entries from a small C++ kzip
~/kythe/kythe-v0.0.75/indexers/cxx_indexer \
    your_corpus.kzip > /tmp/scry2.entries

# 2. Build the index
./target/release/scry2 index --entries /tmp/scry2.entries -o /tmp/scry2.s2db

# 3. Query
./target/release/scry2 --index /tmp/scry2.s2db stat
./target/release/scry2 --index /tmp/scry2.s2db def main --substr
```

For a multi-language kzip, use `from-kzip` instead — see `docs/USAGE.md`.

## Uninstall

```
rm target/release/scry2 your_corpus.s2db
```

There is no persistent state outside the `.s2db` file you build.
scry2 reads `.s2db` with mmap, writes it once, and that's the entire
on-disk footprint.

## Cargo features

| feature | default | enables |
|---|---|---|
| (none) | yes | the `scry2` binary + library, mmap-only |
| `rocksdb-backend` (on the **bench** crate only) | no | the redb vs rocksdb vs mmap shoot-out at `docs/BENCH.md`. Drags `librocksdb-sys` (5-min C++ build first time). Not needed to use scry2. |
