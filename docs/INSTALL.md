# scry2 — install

scry2 is one Rust binary that shells out to Kythe's per-language
indexers. The install boils down to: get `scry2`, get a Kythe v0.0.75
release, point one at the other.

## One-liner install (AOSP target)

If the goal is **indexing AOSP**, paste this:

```bash
curl -fsSL https://raw.githubusercontent.com/fiveapplesonthetable/scry2/master/scripts/install-aosp.sh | bash
```

That script:

1. `apt-get`s the prereqs (git, JDK 21, Bazel via Bazelisk, Rust)
2. Clones Kythe v0.0.75 source, applies the four AOSP patches, builds
   `java_indexer.jar` and `jvm_indexer.jar` (~10 min)
3. Downloads the stock Kythe v0.0.75 release, overlays the patched jars
4. Builds scry2 from source, installs to `/usr/local/bin/scry2`
5. Prints the AOSP-side commands to run next (your tree, your kzip)

End state: `scry2` in `$PATH`, patched Kythe at `~/scry2-setup/kythe-v0.0.75/`,
ready to consume `aosp.kzip`. Read [`docs/AOSP.md`](AOSP.md) for the
full kzip-build + query recipe.

## Manual install — non-AOSP corpora (C/C++/Go/proto)

For non-AOSP corpora, **stock Kythe v0.0.75 is enough** (no Java patches
needed). Two curls:

```bash
# 1. scry2 binary
curl -fL https://github.com/fiveapplesonthetable/scry2/releases/latest/download/scry2-linux-x86_64 \
    -o /usr/local/bin/scry2 && chmod +x /usr/local/bin/scry2

# 2. Kythe v0.0.75 release tarball (~200 MB)
mkdir -p ~/kythe && cd ~/kythe
curl -fL https://github.com/kythe/kythe/releases/download/v0.0.75/kythe-v0.0.75.tar.gz | tar -xz

# Smoke test against any cxx kzip
scry2 from-kzip --kzip your.kzip --kythe-root ~/kythe/kythe-v0.0.75 -o your.s2db
scry2 --index your.s2db stat
```

Until v0.1.0 ships a prebuilt scry2 binary, build from source:

```bash
# Requirements: Rust 1.75 stable
curl -fsSL https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"

git clone https://github.com/fiveapplesonthetable/scry2
cd scry2 && cargo build --release -p scry2
sudo install -m 755 target/release/scry2 /usr/local/bin/
```

The release build takes ~30 s. Seven small crate deps
(`anyhow`, `clap`, `memmap2`, `serde`, `serde_json`, `twox-hash`,
`libc`); no `build.rs`, no codegen, no C/C++.

## What you get

```
scry2          # the CLI/library
~/kythe/...    # indexer binaries (cxx_indexer, java_indexer.jar, ...)
your.s2db      # one mmap'd index file per corpus
```

That's it. No daemons, no config, no per-user state.

## 30-second smoke test

```bash
~/kythe/kythe-v0.0.75/indexers/cxx_indexer some.kzip \
    | scry2 index --entries - -o /tmp/smoke.s2db
scry2 --index /tmp/smoke.s2db stat
scry2 --index /tmp/smoke.s2db def main --substr --limit 5
```

If you see row counts > 0 and at least one match, you're set.
See [USAGE.md](USAGE.md) for the full verb catalog.

## Uninstall

```
sudo rm /usr/local/bin/scry2
rm -rf ~/scry2-setup ~/kythe path/to/your.s2db
```

No package-manager state, no config dirs, no daemons to stop.
