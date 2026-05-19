#!/usr/bin/env bash
# scry2 AOSP install — paste one line and get going:
#
#   curl -fsSL https://raw.githubusercontent.com/fiveapplesonthetable/scry2/master/scripts/install-aosp.sh | bash
#
# What this script does, in order:
#   1. apt-installs prereqs (git, build tools, JDK 21, bazel)
#   2. clones Kythe v0.0.75 + applies the four AOSP patches
#   3. builds the patched java_indexer.jar + jvm_indexer.jar
#   4. downloads stock Kythe v0.0.75 release, overlays the patched jars
#   5. builds scry2 from source and installs it to /usr/local/bin/scry2
#   6. prints what to do next (run AOSP build_kzip.bash, then from-kzip)
#
# What it does NOT do:
#   * Doesn't run AOSP's build_kzip.bash — you point an existing AOSP
#     checkout at the resulting Kythe via $KYTHE_ROOT yourself.
#   * Doesn't write anywhere outside $HOME/scry2-setup and
#     /usr/local/bin/scry2.
#
# Idempotent: re-running is safe; existing intermediates are reused.

set -euo pipefail

SETUP_DIR="${SCRY2_SETUP_DIR:-$HOME/scry2-setup}"
KYTHE_REL="$SETUP_DIR/kythe-v0.0.75"
KYTHE_SRC="$SETUP_DIR/kythe-src"
SCRY_SRC="$SETUP_DIR/scry-src"
SCRY2_SRC="$SETUP_DIR/scry2-src"
BIN_DIR="${SCRY2_BIN_DIR:-/usr/local/bin}"
mkdir -p "$SETUP_DIR"

log() { printf '[scry2-install] %s\n' "$*" >&2; }

# ---------------------------------------------------------------- prereqs
log "1/5 installing host prereqs (sudo apt-get)"
if ! command -v bazel >/dev/null 2>&1; then
    sudo apt-get update
    sudo apt-get install -y \
        git build-essential curl ca-certificates \
        openjdk-21-jdk-headless python3 zip unzip
    curl -fLO https://github.com/bazelbuild/bazelisk/releases/latest/download/bazelisk-linux-amd64
    sudo install bazelisk-linux-amd64 /usr/local/bin/bazel
    rm bazelisk-linux-amd64
fi

# Rust — for building scry2 itself
if ! command -v cargo >/dev/null 2>&1; then
    log "    installing Rust via rustup (non-interactive)"
    curl -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi

# ---------------------------------------------------------------- patched Kythe
log "2/5 patching + building Kythe jars (~10 min first build)"
if [[ ! -d "$KYTHE_SRC" ]]; then
    git clone --depth=1 -b v0.0.75 https://github.com/kythe/kythe.git "$KYTHE_SRC"
fi
if [[ ! -d "$SCRY_SRC" ]]; then
    git clone --depth=1 https://github.com/fiveapplesonthetable/scry "$SCRY_SRC"
fi

cd "$KYTHE_SRC"
if [[ ! -f .scry2-patches-applied ]]; then
    git apply "$SCRY_SRC"/kythe-patches/000{1,2,3,4}-*.patch
    touch .scry2-patches-applied
fi
bazel run @unpinned_maven//:pin
bazel build \
    //kythe/java/com/google/devtools/kythe/analyzers/java:indexer \
    //kythe/java/com/google/devtools/kythe/analyzers/jvm:indexer

# ---------------------------------------------------------------- stock release + overlay
log "3/5 downloading stock Kythe v0.0.75 + overlaying patched jars"
if [[ ! -d "$KYTHE_REL" ]]; then
    cd "$SETUP_DIR"
    curl -fL https://github.com/kythe/kythe/releases/download/v0.0.75/kythe-v0.0.75.tar.gz \
        | tar -xz
fi
cp "$KYTHE_SRC"/bazel-bin/kythe/java/com/google/devtools/kythe/analyzers/java/indexer_deploy.jar \
   "$KYTHE_REL"/indexers/java_indexer.jar
cp "$KYTHE_SRC"/bazel-bin/kythe/java/com/google/devtools/kythe/analyzers/jvm/indexer_deploy.jar \
   "$KYTHE_REL"/indexers/jvm_indexer.jar
log "    Kythe ready at $KYTHE_REL"

# ---------------------------------------------------------------- scry2
log "4/5 building scry2"
if [[ ! -d "$SCRY2_SRC" ]]; then
    git clone https://github.com/fiveapplesonthetable/scry2 "$SCRY2_SRC"
fi
cd "$SCRY2_SRC"
git pull --ff-only
cargo build --release -p scry2
sudo install -m 755 target/release/scry2 "$BIN_DIR/scry2"
log "    scry2 installed: $($BIN_DIR/scry2 --version)"

# ---------------------------------------------------------------- next steps
log "5/5 done. Next, generate your AOSP kzip and index it:"
cat >&2 <<EOF

  # In your AOSP source tree (post-repo-sync, post-lunch target):
  export XREF_CORPUS=android.googlesource.com/platform/superproject
  export DIST_DIR=\$PWD/out/dist
  export KZIP_NAME=aosp
  export KYTHE_KZIP_ENCODING=proto
  build/soong/build_kzip.bash               # 2-4 hrs, produces out/dist/aosp.kzip

  # Then build the scry2 index (30-60 min for a full AOSP kzip):
  scry2 from-kzip \\
      --kzip \$DIST_DIR/aosp.kzip \\
      --kythe-root $KYTHE_REL \\
      -o ~/aosp.s2db --jvm-heap 8g

  # Query:
  scry2 --index ~/aosp.s2db def android.os.Binder.clearCallingIdentity

See docs/AOSP.md and docs/USAGE.md for the full recipe.
EOF
