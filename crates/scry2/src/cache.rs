//! Per-CU shard cache for `from-kzip` incremental rebuilds.
//!
//! A `from-kzip` build indexes every compilation unit (CU) in a kzip with
//! an external indexer subprocess (clang/javac/…), ingests the indexer's
//! Kythe stream into an in-memory delta, and merges every CU's delta into
//! one `.s2db`. Re-running the same kzip re-does all of that even when
//! nothing changed. This cache persists each CU's produced delta — a
//! standalone `.s2db` shard — keyed by a CONTENT digest of exactly what
//! the indexer consumes, so an unchanged CU is reused (skipping both the
//! indexer subprocess and the ingest) and only changed/new CUs are
//! re-indexed. The final k-way merge then folds the reused + freshly built
//! shards into the same output.
//!
//! ## Why this is byte-identical to a full build (and reuses across add/delete)
//!
//! The merged `.s2db` is a deterministic function of (a) the set of delta
//! rows across all CUs and (b) the path → file-id map. A per-CU shard stores
//! its file references in a LOCAL, membership-independent namespace: each
//! path the CU touches gets its dense rank among only that CU's files
//! ([`crate::writer::IndexBuilder::localize_file_ids`]). That makes a shard a
//! pure function of the CU's CONTENT — built now or in a prior run, with any
//! other CU added or removed, it is byte-identical — so a digest hit reuses
//! it directly.
//!
//! The final merge then re-assigns GLOBAL file-ids = each path's rank in the
//! build's sorted seed set (the exact rule a clean build uses), remapping
//! every shard's local ids while folding them
//! ([`crate::writer::coalesce_shards_remapped`]). Because the global ids it
//! assigns are identical to a clean build's, and the per-table merge is the
//! same sorted-deduped union, the merged bytes equal a full build's. A
//! membership change only re-sorts the union at merge time; it never shifts a
//! cached shard's stored ids — so adding/deleting a CU reuses every unchanged
//! shard with no cache wipe. See the gate in the feature commit for the
//! byte-for-byte `cmp` evidence.
//!
//! ## Digest key
//!
//! [`cu_content_digest`] hashes a canonical encoding of everything the
//! indexer reads for one CU: its argument list, its required-input
//! (path, content-digest) pairs (sorted), plus an indexer-version tag and
//! the scry2 ingest-schema-version constant. The kzip CU proto already
//! carries the required-input digests and args, so this is complete by
//! construction and avoids the sub-kzip zip SHA (which can carry packaging
//! nondeterminism). Two builds that would feed the indexer identical input
//! and ingest it with the same scry2 produce the same digest.
//!
//! ## Scale
//!
//! In cache mode every CU contributes ONE per-CU shard (a cache hit copies
//! its shard in; a miss writes + stores one). Feeding tens of thousands of
//! per-CU shards directly into one k-way merge would mmap them all at once
//! and fan in over a very wide front. Instead the final merge is COALESCED:
//! the per-CU shards are partitioned into fixed-size groups, each group is
//! merged (with the local→global file-id remap) into one intermediate shard
//! in parallel, and the ~tens of intermediates are merged into the final
//! `.s2db`. The merge is partition-invariant (a commutative, associative
//! sorted-deduped union with deterministic tie-breaks and clean-build-
//! identical global ids), so grouping cannot change the result — the gate
//! proves it. The group size is the memory/fan-in knob.

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Bumped whenever scry2's ingest (`kythe::ingest_tolerant`) or the `.s2db`
/// row semantics — including the per-shard file-id ENCODING — change in a
/// way that would make an OLD cached shard inconsistent with a freshly built
/// one. Folded into every CU digest so a scry2 upgrade transparently
/// invalidates every stale cache entry (the digest no longer matches, so the
/// shard is a miss and gets rebuilt) rather than silently merging
/// incompatible shards. Keep this in lockstep with any change to what rows a
/// CU's ingest emits or how a shard stores them.
///
/// v2: per-CU shards store LOCAL file ids ([`crate::writer::IndexBuilder::
/// localize_file_ids`]) remapped to global at merge time, instead of the v1
/// scheme that baked the build's global seed ranks into each shard. The
/// bump invalidates any v1 (global-id) shard automatically — its rows would
/// be misattributed if reused under the local-id merge — so no separate
/// cache wipe is needed across the upgrade.
pub const INGEST_SCHEMA_VERSION: u32 = 2;

/// A tag identifying the indexer toolchain that produced (and would
/// reproduce) a CU's shard. Folded into the digest so swapping the indexer
/// binaries (a new Kythe release) invalidates cached shards built by the
/// old ones. Derived from the kythe-release dir's indexer binary sizes +
/// mtimes — cheap, and changes whenever an indexer is replaced. Computed
/// once per build.
pub fn indexer_version_tag(kythe_root: &Path) -> String {
    // The six indexer artifacts from-kzip can dispatch to. A missing one is
    // simply skipped (some releases ship a subset); its absence is itself a
    // stable signal. Sorted, so the tag is order-independent.
    const INDEXERS: &[&str] = &[
        "indexers/cxx_indexer",
        "indexers/go_indexer",
        "indexers/java_indexer.jar",
        "indexers/jvm_indexer.jar",
        "indexers/proto_indexer",
        "indexers/textproto_indexer",
    ];
    let mut h = Hasher::new();
    for rel in INDEXERS {
        let p = kythe_root.join(rel);
        h.add_str(rel);
        match std::fs::metadata(&p) {
            Ok(m) => {
                h.add_u64(m.len());
                // mtime as nanos since epoch; 0 if unavailable. Replacing a
                // binary changes its mtime even if the size matches.
                let mtime = m.modified().ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                h.add_u64(mtime);
            }
            Err(_) => { h.add_u64(u64::MAX); } // sentinel "absent"
        }
    }
    format!("{:032x}", h.finish())
}

/// Compute a CU's content digest from the kzip proto fields the indexer
/// consumes. `indexer_tag` is [`indexer_version_tag`] for this build.
///
/// Canonical encoding (length-prefixed so no field boundary is ambiguous):
///   schema-version, indexer-tag, the CU language, then the argument list
///   IN ORDER (argv order is semantically significant to a compiler), then
///   the required-input (path, digest) pairs SORTED (input order is not
///   significant and some extractors don't fix it).
pub fn cu_content_digest(cu: &crate::kzip::CompilationUnit, indexer_tag: &str) -> String {
    let mut h = Hasher::new();
    h.add_u64(INGEST_SCHEMA_VERSION as u64);
    h.add_str(indexer_tag);
    // Language drives indexer routing; a CU re-tagged to a different
    // language would index differently, so it must change the digest.
    h.add_str(&cu.v_name.language);
    // Arguments in order.
    h.add_u64(cu.argument.len() as u64);
    for a in &cu.argument { h.add_str(a); }
    // Required inputs, sorted+deduped by (path, digest) for
    // order-independence (extractors don't fix required-input order).
    let inputs: BTreeSet<(&str, &str)> = cu.required_input.iter()
        .map(|ri| (ri.v_name.path.as_str(), ri.info.digest.as_str()))
        .collect();
    h.add_u64(inputs.len() as u64);
    for (path, digest) in &inputs {
        h.add_str(path);
        h.add_str(digest);
    }
    format!("{:032x}", h.finish())
}

/// Streaming 128-bit xxh3 hasher over length-prefixed fields. A 128-bit
/// digest is collision-safe at corpus scale (millions of CUs) and needs no
/// extra dependency (twox-hash is already in the tree). Length prefixes
/// make the field stream unambiguous so two distinct field layouts can't
/// alias to the same byte sequence.
struct Hasher {
    buf: Vec<u8>,
}

impl Hasher {
    fn new() -> Self { Self { buf: Vec::with_capacity(4096) } }
    fn add_u64(&mut self, v: u64) { self.buf.extend_from_slice(&v.to_le_bytes()); }
    fn add_str(&mut self, s: &str) {
        self.buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
        self.buf.extend_from_slice(s.as_bytes());
    }
    fn finish(&self) -> u128 {
        twox_hash::xxh3::hash128(&self.buf)
    }
}

/// On-disk cache layout, rooted at a directory (default `<out>.cache/`).
///
/// ```text
///   <root>/
///     manifest.tsv          digest \t shard-filename, one CU per line
///     shards/<digest>.s2db  one CU's delta shard, named by its digest
/// ```
///
/// The shard filename IS the digest, so the manifest is a convenience index
/// (and a place to record which digests this cache holds) rather than the
/// source of truth — a shard is present iff its file exists. Lookups stat
/// the shard path directly, so a torn manifest never causes a wrong reuse.
pub struct ShardCache {
    root:    PathBuf,
    shards:  PathBuf,
}

impl ShardCache {

    /// Open (creating if absent) a cache at `root`.
    pub fn open(root: &Path) -> Result<Self> {
        let shards = root.join("shards");
        std::fs::create_dir_all(&shards)
            .with_context(|| format!("mkdir cache shards {}", shards.display()))?;
        Ok(Self { root: root.to_path_buf(), shards })
    }

    /// Delete the entire cache directory (used by `--clean`). Safe if absent.
    pub fn clear(root: &Path) -> Result<()> {
        if root.exists() {
            std::fs::remove_dir_all(root)
                .with_context(|| format!("rm cache {}", root.display()))?;
        }
        Ok(())
    }

    /// Path of the shard for `digest` (whether or not it exists).
    pub fn shard_path(&self, digest: &str) -> PathBuf {
        self.shards.join(format!("{digest}.s2db"))
    }

    /// Is a shard for `digest` present (a cache hit)? A present-but-corrupt
    /// shard is treated as a hit at lookup time; the merge open would catch
    /// a corrupt file, so the conservative choice is to verify openability
    /// here so a corrupt cache entry degrades to a miss rather than aborting
    /// the build.
    pub fn contains(&self, digest: &str) -> bool {
        let p = self.shard_path(digest);
        if !p.exists() { return false; }
        crate::reader::Index::open(&p).is_ok()
    }

    /// Store `src` (a freshly built CU shard) under `digest`, atomically
    /// (copy to a temp then rename). Overwrites any prior shard for the
    /// digest — a re-index of the same content reproduces the same bytes,
    /// so a stale partial is harmlessly replaced. Idempotent.
    pub fn store(&self, digest: &str, src: &Path) -> Result<()> {
        let dst = self.shard_path(digest);
        let tmp = self.shards.join(format!(".{digest}.{}.tmp", std::process::id()));
        std::fs::copy(src, &tmp)
            .with_context(|| format!("copy {} → {}", src.display(), tmp.display()))?;
        std::fs::rename(&tmp, &dst)
            .with_context(|| format!("rename {} → {}", tmp.display(), dst.display()))?;
        Ok(())
    }

    /// Write the manifest (digest → shard filename) listing the digests
    /// `kept` (the current build's CU set). Prunes shard files no longer in
    /// `kept` so a cache for a kzip that dropped CUs doesn't grow without
    /// bound. Atomic write of the manifest; best-effort prune of orphans.
    pub fn finalize(&self, kept: &[String]) -> Result<()> {
        use std::io::Write;
        let keep: std::collections::HashSet<&str> =
            kept.iter().map(|s| s.as_str()).collect();
        // Prune orphan shards (present on disk but not in this build's set).
        if let Ok(rd) = std::fs::read_dir(&self.shards) {
            for e in rd.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                let Some(dg) = name.strip_suffix(".s2db") else { continue };
                if !keep.contains(dg) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        // Write the manifest atomically.
        let mut lines: Vec<String> = kept.iter()
            .map(|d| format!("{d}\t{d}.s2db"))
            .collect();
        lines.sort();
        let manifest = self.root.join("manifest.tsv");
        let tmp = self.root.join(format!(".manifest.{}.tmp", std::process::id()));
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        for l in &lines { writeln!(f, "{l}")?; }
        f.flush()?;
        f.sync_all().ok();
        drop(f);
        std::fs::rename(&tmp, &manifest)
            .with_context(|| format!("rename manifest {}", manifest.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kzip::{CompilationUnit, FileInfo, FileInput, VName};

    fn cu_with(args: Vec<&str>, inputs: Vec<(&str, &str)>) -> CompilationUnit {
        CompilationUnit {
            v_name: VName { language: "c++".into(), ..Default::default() },
            argument: args.into_iter().map(|s| s.to_string()).collect(),
            required_input: inputs.into_iter().map(|(p, d)| FileInput {
                v_name: VName { path: p.into(), ..Default::default() },
                info: FileInfo { path: p.into(), digest: d.into() },
            }).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn digest_stable_for_identical_input() {
        let a = cu_with(vec!["clang++", "-c", "a.cpp"], vec![("a.cpp", "d1"), ("a.h", "d2")]);
        let b = cu_with(vec!["clang++", "-c", "a.cpp"], vec![("a.cpp", "d1"), ("a.h", "d2")]);
        assert_eq!(cu_content_digest(&a, "tag"), cu_content_digest(&b, "tag"));
    }

    #[test]
    fn digest_independent_of_required_input_order() {
        // required_input order is not semantically significant; the digest
        // must be the same whichever order the extractor emitted them in.
        let a = cu_with(vec!["clang++"], vec![("a.cpp", "d1"), ("a.h", "d2")]);
        let b = cu_with(vec!["clang++"], vec![("a.h", "d2"), ("a.cpp", "d1")]);
        assert_eq!(cu_content_digest(&a, "tag"), cu_content_digest(&b, "tag"));
    }

    #[test]
    fn digest_changes_on_input_digest() {
        // A changed file content (different blob digest) is the core "this
        // CU changed" signal — it MUST change the key.
        let a = cu_with(vec!["clang++"], vec![("a.cpp", "d1")]);
        let b = cu_with(vec!["clang++"], vec![("a.cpp", "d1_CHANGED")]);
        assert_ne!(cu_content_digest(&a, "tag"), cu_content_digest(&b, "tag"));
    }

    #[test]
    fn digest_changes_on_args() {
        let a = cu_with(vec!["clang++", "-O0"], vec![("a.cpp", "d1")]);
        let b = cu_with(vec!["clang++", "-O2"], vec![("a.cpp", "d1")]);
        assert_ne!(cu_content_digest(&a, "tag"), cu_content_digest(&b, "tag"));
    }

    #[test]
    fn digest_sensitive_to_arg_order() {
        // argv order IS significant to a compiler; reordering must change
        // the key (a flag's position can change its meaning).
        let a = cu_with(vec!["clang++", "-Ifoo", "-Ibar"], vec![("a.cpp", "d1")]);
        let b = cu_with(vec!["clang++", "-Ibar", "-Ifoo"], vec![("a.cpp", "d1")]);
        assert_ne!(cu_content_digest(&a, "tag"), cu_content_digest(&b, "tag"));
    }

    #[test]
    fn digest_changes_on_indexer_tag() {
        let a = cu_with(vec!["clang++"], vec![("a.cpp", "d1")]);
        assert_ne!(cu_content_digest(&a, "tag1"), cu_content_digest(&a, "tag2"));
    }

    #[test]
    fn digest_changes_on_language() {
        let mut a = cu_with(vec!["x"], vec![("a", "d1")]);
        let mut b = a.clone();
        a.v_name.language = "c++".into();
        b.v_name.language = "java".into();
        assert_ne!(cu_content_digest(&a, "t"), cu_content_digest(&b, "t"));
    }

    #[test]
    fn cache_store_hit_miss_roundtrip() {
        let dir = std::env::temp_dir().join(format!("scry2-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = ShardCache::open(&dir).unwrap();
        let digest = "deadbeef";
        assert!(!cache.contains(digest), "fresh cache: miss");

        // Build a tiny real shard to store.
        let src = dir.join("src.s2db");
        let mut b = crate::writer::IndexBuilder::new();
        b.upsert_sym(crate::format::sym_of("kythe:x"), crate::format::kind::FUNCTION,
                     crate::format::lang::CXX, "x");
        b.upsert_file(0, "a.cpp");
        b.add_xref(crate::format::sym_of("kythe:x"), crate::format::role::DEF, 0, 1);
        b.finish(&src).unwrap();

        cache.store(digest, &src).unwrap();
        assert!(cache.contains(digest), "after store: hit");

        // The stored shard reopens and carries the row.
        let ix = crate::reader::Index::open(&cache.shard_path(digest)).unwrap();
        assert_eq!(ix.iter_files().count(), 1);

        // Finalize keeps the digest's shard; an orphan is pruned.
        let orphan = cache.shard_path("cafef00d");
        std::fs::copy(&src, &orphan).unwrap();
        cache.finalize(&[digest.to_string()]).unwrap();
        assert!(cache.contains(digest), "kept digest survives finalize");
        assert!(!orphan.exists(), "orphan shard pruned by finalize");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clean_removes_cache() {
        let dir = std::env::temp_dir().join(format!("scry2-cache-clean-{}", std::process::id()));
        let cache = ShardCache::open(&dir).unwrap();
        std::fs::write(cache.shard_path("aa"), b"x").unwrap();
        assert!(dir.exists());
        ShardCache::clear(&dir).unwrap();
        assert!(!dir.exists(), "clear removes the cache dir");
        // Idempotent on an absent cache.
        ShardCache::clear(&dir).unwrap();
    }

}
