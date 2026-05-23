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
//! ## Why this is byte-identical to a full build
//!
//! The merged `.s2db` is a deterministic function of (a) the set of delta
//! rows across all CUs and (b) the path → file-id map. A cached CU's shard
//! carries byte-identical rows to a freshly built one BECAUSE file-ids are
//! pre-seeded deterministically from the plan's path set
//! ([`crate::kythe::FileIdAllocator::seed_paths`]) before any ingest — so a
//! CU's shard is the same whether built now or built in a prior run. The
//! k-way merge output is invariant to how CUs are partitioned into shards
//! (it is the sorted-deduped union), so reusing per-CU shards yields the
//! identical merged bytes a full build produces. See `DESIGN`/the gate in
//! the feature commit for the byte-for-byte `cmp` evidence.
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
//! In cache mode every CU contributes ONE per-CU shard to the final k-way
//! merge (a cache hit copies its shard in; a miss writes + stores one). The
//! merge is partition-invariant, so this is correct at any count, and at the
//! gate's scale (a scoped `--in` slice, hundreds of CUs) it is also cheap.
//! For a whole-AOSP run (tens of thousands of CUs) the merge would mmap that
//! many shards at once — well within this host's fd limit but heavier than
//! the legacy snapshot path's coarse batching. Coalescing cache shards into
//! batches before the merge would restore that, and is the natural follow-up
//! if the cache is pointed at a corpus-scale build; it does not affect the
//! per-CU keying or byte-identity established here.

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Bumped whenever scry2's ingest (`kythe::ingest_tolerant`) or the `.s2db`
/// row semantics change in a way that would make an OLD cached shard
/// inconsistent with a freshly built one. Folded into every CU digest so a
/// scry2 upgrade transparently invalidates every stale cache entry rather
/// than silently merging incompatible shards. Keep this in lockstep with
/// any change to what rows a CU's ingest emits.
pub const INGEST_SCHEMA_VERSION: u32 = 1;

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
    /// Default cache dir for an output path: a sibling `<out>.cache/`.
    pub fn default_dir(out: &Path) -> PathBuf {
        let mut p = out.to_path_buf();
        let stem = p.file_name().map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        p.set_file_name(format!("{stem}.cache"));
        p
    }

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

    /// The stored seed-basis token, if any. The basis is a hash of the
    /// build's deterministic file-id seed (the sorted plan path set). A
    /// cached shard's file-ids are RANKS in that seed, so reusing a shard is
    /// only valid when the current build's seed basis matches the one the
    /// shard was built under. A mismatch (e.g. a different `--in` filter that
    /// changes the path set, shifting every rank) means every cached shard's
    /// ids are stale — the cache must be discarded, not silently merged.
    pub fn stored_basis(&self) -> Option<String> {
        std::fs::read_to_string(self.root.join("basis"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Record the current build's seed-basis token (atomic write).
    pub fn write_basis(&self, basis: &str) -> Result<()> {
        let tmp = self.root.join(format!(".basis.{}.tmp", std::process::id()));
        std::fs::write(&tmp, basis.as_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, self.root.join("basis"))
            .with_context(|| "rename basis")?;
        Ok(())
    }

    /// Compute the seed-basis token from the sorted seed path list. A 128-bit
    /// xxh3 over the length-prefixed paths — order-sensitive (rank order is
    /// what matters) and collision-safe.
    pub fn seed_basis<'a, I: IntoIterator<Item = &'a str>>(sorted_paths: I) -> String {
        let mut h = Hasher::new();
        for p in sorted_paths { h.add_str(p); }
        format!("{:032x}", h.finish())
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

    #[test]
    fn seed_basis_order_sensitive() {
        // Rank order is what file-ids encode, so a reordered path set is a
        // DIFFERENT basis (its ranks differ), and the same set is the same
        // basis.
        let a = ShardCache::seed_basis(["", "a", "b", "c"]);
        let same = ShardCache::seed_basis(["", "a", "b", "c"]);
        let reordered = ShardCache::seed_basis(["", "b", "a", "c"]);
        let extra = ShardCache::seed_basis(["", "a", "b", "c", "d"]);
        assert_eq!(a, same, "identical path set → identical basis");
        assert_ne!(a, reordered, "reordered set → different basis");
        assert_ne!(a, extra, "added path → different basis");
    }

    #[test]
    fn basis_roundtrip_and_absent() {
        let dir = std::env::temp_dir().join(format!("scry2-cache-basis-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = ShardCache::open(&dir).unwrap();
        assert!(cache.stored_basis().is_none(), "fresh cache: no basis");
        cache.write_basis("abc123").unwrap();
        assert_eq!(cache.stored_basis().as_deref(), Some("abc123"));
        cache.write_basis("def456").unwrap();
        assert_eq!(cache.stored_basis().as_deref(), Some("def456"), "basis overwrites");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
