#!/usr/bin/env bash
#
# AOSP-shaped wrapper around `scry2 from-kzip`.
#
# scry2 itself has zero AOSP-specific knowledge: its `--inject-cu-arg`
# flag is a generic path-prefix → arg injector, and its `--in` /
# `--not-in` are generic substring filters. This script sets sensible
# AOSP defaults for all of them (scope, languages, JVM heap, CU-arg
# rules) so a fresh user can index a Soong-built kzip with just:
#
#     ./scripts/aosp-from-kzip.sh /aosp/out/dist/aosp.kzip -o aosp.s2db
#
# Once produced, the .s2db is fully self-contained — querying needs
# no wrapper:
#
#     scry2 --index aosp.s2db def android.os.Binder.clearCallingIdentity
#     scry2 --index aosp.s2db callers android::Parcel::writeStrongBinder
#     scry2 --index aosp.s2db ref clearCallingIdentity --in frameworks/base
#
# Required env:
#   ANDROID_BUILD_TOP  — AOSP checkout root (the dir holding
#                        frameworks/, libcore/, …). Used by some
#                        --inject-cu-arg rules (currently only as
#                        documentation; the rules below use AOSP-
#                        relative paths that work for any checkout).
#
# Optional env (each has a default, override to customise):
#   KYTHE_ROOT         — patched Kythe v0.0.75 release dir.
#                        Default: $HOME/scry2-setup/kythe-v0.0.75.
#   SCRY2              — scry2 binary path. Default: scry2 from PATH.
#   AOSP_IN            — comma-separated --in scope. Default narrows
#                        to the layers scry2 is typically asked about
#                        (frameworks/base + frameworks/native + system/
#                        + art/ + libcore/). Pass `AOSP_IN=` (empty)
#                        for the full corpus.
#   AOSP_LANGS         — comma-separated --langs. Default cxx,java,jvm
#                        (no Go/proto — they're sparse in AOSP and
#                        rarely queried).
#   AOSP_JVM_HEAP      — --jvm-heap. Default 12g (handles services.core
#                        and other heavy javac batches with margin).
#
# Usage:
#   aosp-from-kzip.sh <KZIP> [extra flags forwarded to `scry2 from-kzip`]

set -euo pipefail

if [[ $# -lt 1 ]]; then
    cat >&2 <<USAGE
usage: $0 <KZIP> [scry2 from-kzip flags ...]

Common overrides via env:
  AOSP_IN=...            comma-separated --in scope (default: frameworks/base,frameworks/native,system/,art/,libcore/)
  AOSP_LANGS=...         comma-separated --langs (default: cxx,java,jvm)
  AOSP_JVM_HEAP=...      --jvm-heap value (default: 12g)
  KYTHE_ROOT=...         patched Kythe release dir
USAGE
    exit 2
fi
KZIP="$1"; shift

: "${ANDROID_BUILD_TOP:?must be set to the AOSP checkout root}"
: "${KYTHE_ROOT:=$HOME/scry2-setup/kythe-v0.0.75}"
: "${SCRY2:=scry2}"
: "${AOSP_IN:=frameworks/base,frameworks/native,system/,art/,libcore/}"
: "${AOSP_LANGS:=cxx,java,jvm}"
: "${AOSP_JVM_HEAP:=12g}"

# Sanity: the patched indexer binaries are where we expect.
for bin in cxx_indexer java_indexer.jar jvm_indexer.jar; do
    if [[ ! -f "$KYTHE_ROOT/indexers/$bin" ]]; then
        echo "error: $KYTHE_ROOT/indexers/$bin missing" >&2
        echo "       set KYTHE_ROOT or run scripts/install-aosp.sh" >&2
        exit 1
    fi
done

# --- AOSP-specific CU-arg injection rules ------------------------------
#
# Each rule is `PREFIX::ARG`. When a CU's primary source path starts
# with PREFIX, ARG is prepended to its compiler argv before the
# indexer is invoked. Rules cascade — multiple rules can fire on the
# same CU. Already-present args are skipped (re-runs are idempotent).
#
# 1. libcore/ojluni — Android's java.base implementation.
#    Soong builds these with --patch-module=java.base=… when emitting
#    actual java.base targets; the core-all build path whose CUs end
#    up in the public aosp.kzip omits the flag. Without it javac sees
#    --system <jdk_image> already declaring java.lang.String and
#    friends and rejects each AOSP source file as a redefinition
#    (`CompletionFailure: class file for java.lang.String not found`).
#
RULES=(
    --inject-cu-arg "libcore/ojluni/src/main/java/::--patch-module=java.base=libcore/ojluni/src/main/java"
)

# Add new rules below as we encounter them. Examples (not yet active —
# fill in if the corresponding indexer failures show up in the
# per-language summary at the end of a run):
#
#   "art/runtime/openjdkjvmti/::--patch-module=jdk.internal.vm.compiler=art/runtime/openjdkjvmti"
#   "external/conscrypt/src/main/java/::--patch-module=jdk.crypto.cryptoki=external/conscrypt/src/main/java"

# Compose the final scry2 invocation. All defaults are overridable
# from the command line because user args follow `"${RULES[@]}"`.
exec "$SCRY2" from-kzip \
    --kzip "$KZIP" \
    --kythe-root "$KYTHE_ROOT" \
    --langs "$AOSP_LANGS" \
    --jvm-heap "$AOSP_JVM_HEAP" \
    ${AOSP_IN:+--in "$AOSP_IN"} \
    "${RULES[@]}" \
    "$@"
