//! .s2db writer. Accumulates rows in memory, sorts each table, and dumps
//! one page-aligned mmap-ready file.
//!
//! Writes go to a tempfile in the same directory, then atomically
//! rename. A crashed build leaves a `.tmp` behind, never a torn index.

use crate::format::{*, kind};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Default, Clone)]
pub struct IndexBuilder {
    xrefs:    Vec<(u64, u8, u32, u32)>,
    syms:     HashMap<u64, (u8, u8, String)>,   // sym → (kind, lang, name)
    files:    HashMap<u32, String>,             // file_id → path
    inherits: Vec<(u64, u64)>,                  // (child, parent)
    aliases:  Vec<(u64, String)>,               // sym → human-typeable name
    /// Call-graph edges. (caller_sym, callee_sym, role). `role` is
    /// `role::CALL` for a direct call, `role::REF` for any other
    /// reference inside the caller's body — the LLM can ask
    /// "what does X touch" not just "what does X call".
    calls:    Vec<(u64, u64, u8)>,
}

impl IndexBuilder {
    pub fn new() -> Self { Self::default() }

    /// Record one (sym, role, file, offset) row. Caller is responsible
    /// for converting Kythe edge-kinds into our compact role byte.
    pub fn add_xref(&mut self, sym: u64, role: u8, file: u32, offset: u32) {
        self.xrefs.push((sym, role, file, offset));
    }

    /// Record a symbol's metadata. Later calls override earlier ones —
    /// the indexer may see a symbol via a `defines/binding` edge before
    /// the node-kind fact arrives, so the kind/lang refines over time.
    pub fn upsert_sym(&mut self, sym: u64, kind: u8, lang: u8, name: &str) {
        self.syms.entry(sym)
            .and_modify(|e| {
                if e.0 == kind::UNK { e.0 = kind; }
                if e.1 == lang::UNK { e.1 = lang; }
                if e.2.is_empty()   { e.2 = name.to_string(); }
            })
            .or_insert_with(|| (kind, lang, name.to_string()));
    }

    /// Record file_id → absolute path mapping. Caller picks the file_id
    /// (typically a small monotonically-increasing u32 keyed off the
    /// path's first-occurrence order).
    pub fn upsert_file(&mut self, file: u32, path: &str) {
        self.files.entry(file).or_insert_with(|| path.to_string());
    }

    pub fn add_inherit(&mut self, child: u64, parent: u64) {
        self.inherits.push((child, parent));
    }

    /// Register a human-typeable alias for `sym` — e.g. the FQN
    /// "android.os.Binder.clearCallingIdentity" learned from a
    /// `/kythe/edge/named` edge. The alphabetical name index will
    /// contain a row per (alias, sym) pair, so both the raw VName
    /// string and every alias resolve via `sym_for_name`.
    pub fn add_alias(&mut self, sym: u64, alias: &str) {
        if alias.is_empty() { return; }
        self.aliases.push((sym, alias.to_string()));
    }

    /// Record a call-graph edge: function `caller` directly references
    /// (`role::CALL`) or refers to (`role::REF`) function/type `callee`.
    /// Used by `scry2 callgraph NAME --direction down` for O(log n)
    /// traversal of "what does X reach?".
    pub fn add_call(&mut self, caller: u64, callee: u64, role: u8) {
        self.calls.push((caller, callee, role));
    }

    pub fn n_xrefs(&self) -> usize { self.xrefs.len() }
    pub fn n_syms(&self)  -> usize { self.syms.len() }
    pub fn n_files(&self) -> usize { self.files.len() }
    pub fn n_inh(&self)   -> usize { self.inherits.len() }
    pub fn n_aliases(&self) -> usize { self.aliases.len() }
    pub fn n_calls(&self) -> usize { self.calls.len() }

    /// Move every row from `other` into `self`, leaving `other`
    /// empty. The mirror of [`populate_from_index`] but in-memory
    /// and zero-copy on the per-row vectors. Used by `from-kzip`
    /// to merge per-worker `IndexBuilder`s into the accumulator
    /// — workers ingest into their own builder (no contention),
    /// the snapshotter drains them into the accumulator under a
    /// short-held lock.
    ///
    /// Merge semantics match the single-builder ingest:
    /// - Append-only tables (`xrefs`, `inherits`, `aliases`,
    ///   `calls`): extend; the final `sort_unstable + dedup`
    ///   inside `finish` handles ordering and duplicates.
    /// - First-wins maps (`syms`, `files`): use
    ///   `entry(...).or_insert(...)` so an earlier worker's
    ///   kind/lang/name and file path win over a later one's,
    ///   the same convention `upsert_sym` / `upsert_file` enforce
    ///   in-builder.
    pub fn merge_from(&mut self, mut other: Self) {
        self.xrefs.append(&mut other.xrefs);
        for (k, v) in other.syms {
            self.syms.entry(k).or_insert(v);
        }
        for (k, v) in other.files {
            self.files.entry(k).or_insert(v);
        }
        self.inherits.append(&mut other.inherits);
        self.aliases.append(&mut other.aliases);
        self.calls.append(&mut other.calls);
    }

    /// Collapse exact-duplicate rows in the append-only tables
    /// (`xrefs`, `inherits`, `calls`, `aliases`). The maps (`syms`,
    /// `files`) are already keyed and need no dedup.
    ///
    /// A freshly-ingested per-CU builder carries heavy intra-CU
    /// redundancy: the indexer emits the same `/kythe/edge/named`
    /// alias on every node that references a symbol (≈30× per CU,
    /// documented in [`finish`]), and repeats xref rows. `finish` /
    /// `write_merged_snapshot` dedup at write time, but if a per-CU
    /// builder is *accumulated* into a long-lived sink first, that
    /// redundancy sits in RAM until the next snapshot — the dominant
    /// driver of from-kzip's in-memory delta. Calling this on each CU
    /// before merging keeps the sink proportional to distinct facts.
    ///
    /// Returns the number of rows remaining after dedup (xrefs +
    /// inherits + calls + aliases), for buffer accounting.
    pub fn dedup_tables(&mut self) -> usize {
        self.xrefs.sort_unstable();
        self.xrefs.dedup();
        self.inherits.sort_unstable();
        self.inherits.dedup();
        self.calls.sort_unstable();
        self.calls.dedup();
        self.aliases.sort_unstable();
        self.aliases.dedup();
        self.xrefs.len() + self.inherits.len() + self.calls.len() + self.aliases.len()
    }

    /// Snapshot the current accumulated state to `path` without
    /// consuming `self`. Used by `from-kzip` to write a partial
    /// `.s2db` every N CUs so a killed run can resume via
    /// [`populate_from_index`].
    ///
    /// Implementation: clone the in-memory tables, then call
    /// [`finish`]. Clone cost on a fully-loaded AOSP builder is
    /// dominated by memcpy of the xref/calls vectors (~8 GB at
    /// ~10 GB/s ≈ 1 s) — short enough to take under the builder
    /// mutex without stalling workers visibly. The cloned copy is
    /// dropped when `finish` returns; the original `self` continues
    /// accumulating new CUs.
    pub fn snapshot(&self, path: &Path) -> Result<u64> {
        self.clone().finish(path)
    }

    /// Replay every row from a saved `.s2db` into `self`. After this
    /// call, calling [`finish`] reproduces a superset of the same
    /// `.s2db` — superset because callers usually keep ingesting
    /// more CUs after a resume.
    pub fn populate_from_index(&mut self, ix: &crate::reader::Index) -> Result<()> {
        for (sym, role, file, offset) in ix.iter_xrefs() {
            self.add_xref(sym, role, file, offset);
        }
        for (sym, kind, lang, name) in ix.iter_syms() {
            self.upsert_sym(sym, kind, lang, name);
        }
        for (file, path) in ix.iter_files() {
            self.upsert_file(file, path);
        }
        for (child, parent) in ix.iter_inherits() {
            self.add_inherit(child, parent);
        }
        for (caller, callee, role) in ix.iter_calls() {
            self.add_call(caller, callee, role);
        }
        for (sym, alias) in ix.iter_aliases() {
            self.add_alias(sym, alias);
        }
        Ok(())
    }

    /// Streaming 2-way merge of `self` (an in-memory delta) with an
    /// optional `prior` on-disk index (mmap'd, never fully loaded into
    /// RAM), written atomically to `output`. The output is byte-for-byte
    /// equivalent to what [`finish`] would produce on a builder
    /// containing `prior + self` — sort/dedup/first-wins semantics
    /// preserved — but peak RAM is bounded to `self`'s size plus the
    /// new file's string blob, regardless of how large `prior` has grown.
    ///
    /// This is the engine behind `from-kzip`'s rolling snapshot: every
    /// snap drains workers into a small delta and merges it against
    /// the previous partial.s2db. Without streaming, each snap would
    /// have to clone or fully reload the accumulator, doubling RAM at
    /// snap time and growing with the run total.
    ///
    /// Consumes `self`: the delta's tables are sorted in place and
    /// drained, so the caller should drop it after this returns.
    /// Returns total bytes written to `output`.
    pub fn write_merged_snapshot(
        mut self,
        prior: Option<&crate::reader::Index>,
        output: &Path,
    ) -> Result<u64> {
        // ---- 1. Sort delta tables in place ----
        self.xrefs.sort_unstable();
        self.xrefs.dedup();
        self.inherits.sort_unstable();
        self.inherits.dedup();
        self.calls.sort_unstable();
        self.calls.dedup();
        self.aliases.sort_unstable();
        self.aliases.dedup();
        let mut delta_syms: Vec<(u64, u8, u8, String)> = self.syms.drain()
            .map(|(s, (k, l, n))| (s, k, l, n)).collect();
        delta_syms.sort_unstable_by_key(|r| r.0);
        let mut delta_files: Vec<(u32, String)> = self.files.drain().collect();
        delta_files.sort_unstable_by_key(|r| r.0);

        // ---- 2. var_syms (for alias suppression) from merged syms ----
        // First-wins: prior's kind wins if both have the same sym. We
        // need this set before processing aliases, and we can't fold
        // it into the syms write pass below because by_name capacity
        // depends on knowing var_syms first (variable-kind aliases
        // are dropped, not appended). One walk over the merged sym
        // stream — cheap relative to xrefs/calls.
        let var_syms = merge_var_syms(prior, &delta_syms);

        // Prior's aliases come back from `iter_aliases` in *alpha*
        // order (it walks the alphabetical name index). Collect once
        // into a `(sym, alias)`-sorted Vec borrowing into the mmap'd
        // blob — no string copies, bounded by alias count not by
        // xref/call count.
        let mut prior_aliases: Vec<(u64, &str)> = match prior {
            Some(p) => p.iter_aliases().collect(),
            None    => Vec::new(),
        };
        prior_aliases.sort_unstable();
        prior_aliases.dedup();

        // ---- 3. Open tmp; deferred-header layout ----
        // We don't pre-count merged row totals (that was the source
        // of the 14-pass snap slowdown — every count walked prior
        // end-to-end). Instead each section's row count is observed
        // while we write it; the header at byte 0 is filled in last
        // via a seek-back. Section offsets are page-aligned positions
        // chosen incrementally as each prior section's `n` lands.
        let tmp_path: PathBuf = output.with_extension("s2db.tmp");
        let f = File::create(&tmp_path)
            .with_context(|| format!("create {}", tmp_path.display()))?;
        let mut w = BufWriter::with_capacity(8 << 20, f);

        let hdr_placeholder = Header { magic: MAGIC, version: VERSION, ..Default::default() };
        write_header(&mut w, &hdr_placeholder)?;

        // ---- 4. xrefs (write + count in one pass) ----
        let xrefs_off = pad_up(size_of_header() as u64);
        seek_to(&mut w, xrefs_off)?;
        let mut n_xrefs: u64 = 0;
        {
            let pri: Box<dyn Iterator<Item = (u64, u8, u32, u32)>> = match prior {
                Some(p) => Box::new(p.iter_xrefs()),
                None    => Box::new(std::iter::empty()),
            };
            let del = self.xrefs.iter().copied();
            for (sym, role, file, offset) in merge_sorted_dedup(pri, del) {
                w.write_all(&sym.to_be_bytes())?;
                w.write_all(&[role])?;
                w.write_all(&file.to_be_bytes())?;
                w.write_all(&offset.to_be_bytes())?;
                n_xrefs += 1;
            }
        }

        // ---- 5. syms (write + count + accumulate names blob/by_name) ----
        let syms_off = pad_up(xrefs_off + n_xrefs * XREF_LEN as u64);
        seek_to(&mut w, syms_off)?;
        let mut n_syms: u64 = 0;
        let mut blob: Vec<u8> = Vec::new();
        let mut by_name: Vec<(u32, u16, u64)> = Vec::new();
        {
            let pri: Box<dyn Iterator<Item = (u64, u8, u8, &str)>> = match prior {
                Some(p) => Box::new(p.iter_syms()),
                None    => Box::new(std::iter::empty()),
            };
            let del = delta_syms.iter().map(|(s, k, l, n)| (*s, *k, *l, n.as_str()));
            for (sym, kind, lang, name) in merge_syms_first_wins(pri, del) {
                let off = blob.len() as u32;
                let len = name.len() as u16;
                blob.extend_from_slice(name.as_bytes());
                w.write_all(&sym.to_be_bytes())?;
                w.write_all(&[kind, lang])?;
                w.write_all(&off.to_be_bytes())?;
                w.write_all(&len.to_be_bytes())?;
                by_name.push((off, len, sym));
                n_syms += 1;
            }
        }

        // ---- 6. aliases (no dedicated section — they fold into
        //      the alphabetical name index) ----
        // Variable-kind syms are skipped here, so the name count
        // depends on the final filtered set, not on the raw merge
        // cardinality. The blob keeps growing.
        {
            let pri = prior_aliases.iter().copied();
            let del = self.aliases.iter().map(|(s, a)| (*s, a.as_str()));
            for (sym, alias) in merge_aliases_dedup(pri, del) {
                if var_syms.contains(&sym) { continue; }
                let off = blob.len() as u32;
                let len = alias.len() as u16;
                blob.extend_from_slice(alias.as_bytes());
                by_name.push((off, len, sym));
            }
        }
        drop(prior_aliases);
        let n_names = by_name.len() as u64;

        // ---- 7. names section (sort by alpha + write) ----
        let names_off = pad_up(syms_off + n_syms * SYM_LEN as u64);
        by_name.sort_by(|a, b| {
            let an = &blob[a.0 as usize..a.0 as usize + a.1 as usize];
            let bn = &blob[b.0 as usize..b.0 as usize + b.1 as usize];
            an.cmp(bn)
        });
        seek_to(&mut w, names_off)?;
        for (off, len, sym) in &by_name {
            w.write_all(&off.to_be_bytes())?;
            w.write_all(&len.to_be_bytes())?;
            w.write_all(&[0u8, 0u8])?;     // _pad
            w.write_all(&sym.to_be_bytes())?;
        }
        drop(by_name);

        // ---- 8. files (write + count + append paths to blob) ----
        let files_off = pad_up(names_off + n_names * NAME_LEN as u64);
        seek_to(&mut w, files_off)?;
        let mut n_files: u64 = 0;
        {
            let pri: Box<dyn Iterator<Item = (u32, &str)>> = match prior {
                Some(p) => Box::new(p.iter_files()),
                None    => Box::new(std::iter::empty()),
            };
            let del = delta_files.iter().map(|(f, p)| (*f, p.as_str()));
            for (file, path) in merge_files_first_wins(pri, del) {
                let off = blob.len() as u32;
                let len = path.len() as u16;
                blob.extend_from_slice(path.as_bytes());
                w.write_all(&file.to_be_bytes())?;
                w.write_all(&off.to_be_bytes())?;
                w.write_all(&len.to_be_bytes())?;
                n_files += 1;
            }
        }

        // ---- 9. inherits ----
        let inh_off = pad_up(files_off + n_files * FILE_LEN as u64);
        seek_to(&mut w, inh_off)?;
        let mut n_inh: u64 = 0;
        {
            let pri: Box<dyn Iterator<Item = (u64, u64)>> = match prior {
                Some(p) => Box::new(p.iter_inherits()),
                None    => Box::new(std::iter::empty()),
            };
            let del = self.inherits.iter().copied();
            for (c, p) in merge_sorted_dedup(pri, del) {
                w.write_all(&c.to_be_bytes())?;
                w.write_all(&p.to_be_bytes())?;
                n_inh += 1;
            }
        }

        // ---- 10. calls (write + collect for the reverse index) ----
        let calls_off = pad_up(inh_off + n_inh * INH_LEN as u64);
        seek_to(&mut w, calls_off)?;
        let mut merged_calls: Vec<(u64, u64, u8)> = Vec::new();
        {
            let pri: Box<dyn Iterator<Item = (u64, u64, u8)>> = match prior {
                Some(p) => Box::new(p.iter_calls()),
                None    => Box::new(std::iter::empty()),
            };
            let del = self.calls.iter().copied();
            for (caller, callee, role) in merge_sorted_dedup(pri, del) {
                w.write_all(&caller.to_be_bytes())?;
                w.write_all(&callee.to_be_bytes())?;
                w.write_all(&[role])?;
                merged_calls.push((caller, callee, role));
            }
        }
        let n_calls = merged_calls.len() as u64;

        // ---- 11. crev (by-callee) ----
        let crev_off = pad_up(calls_off + n_calls * CALL_LEN as u64);
        seek_to(&mut w, crev_off)?;
        let mut calls_rev: Vec<(u64, u64, u8)> = merged_calls.into_iter()
            .map(|(caller, callee, role)| (callee, caller, role))
            .collect();
        calls_rev.sort_unstable();
        for (callee, caller, role) in &calls_rev {
            w.write_all(&callee.to_be_bytes())?;
            w.write_all(&caller.to_be_bytes())?;
            w.write_all(&[*role])?;
        }
        drop(calls_rev);

        // ---- 12. blob ----
        let blob_off = pad_up(crev_off + n_calls * CALL_LEN as u64);
        seek_to(&mut w, blob_off)?;
        w.write_all(&blob)?;
        let blob_len = blob.len() as u64;
        drop(blob);

        // ---- 13. Header (seek back to byte 0, write final counts) ----
        let hdr = Header {
            magic: MAGIC, version: VERSION,
            xrefs_off, xrefs_n: n_xrefs,
            syms_off,  syms_n:  n_syms,
            names_off, names_n: n_names,
            files_off, files_n: n_files,
            inh_off,   inh_n:   n_inh,
            calls_off, calls_n: n_calls,
            crev_off,  crev_n:  n_calls,
            blob_off,  blob_len,
            ..Default::default()
        };
        seek_to(&mut w, 0)?;
        write_header(&mut w, &hdr)?;

        let total = blob_off + blob_len;
        w.flush()?;
        w.get_mut().sync_all().context("fsync")?;
        drop(w);
        std::fs::rename(&tmp_path, output)
            .with_context(|| format!("rename to {}", output.display()))?;
        Ok(total)
    }

    /// Sort all tables and serialize to `path` atomically. Returns total
    /// bytes written.
    pub fn finish(mut self, path: &Path) -> Result<u64> {
        // ---- 1. Sort xrefs by (sym, role, file, offset) ----
        self.xrefs.sort_unstable();
        self.xrefs.dedup();
        let n_xrefs = self.xrefs.len() as u64;

        // ---- 2. Stable sym order + alphabetical name index ----
        let mut syms_vec: Vec<(u64, u8, u8, String)> = self.syms.into_iter()
            .map(|(s, (k, l, n))| (s, k, l, n)).collect();
        syms_vec.sort_unstable_by_key(|r| r.0);
        let n_syms = syms_vec.len() as u64;

        // Build the strings blob. Names first (so binary search ranges
        // hit a hot region), then paths. Track (offset, length) per name.
        let mut blob: Vec<u8> = Vec::new();
        let mut name_pos: Vec<(u32, u16)> = Vec::with_capacity(syms_vec.len());
        for (_, _, _, name) in &syms_vec {
            assert!(name.len() <= u16::MAX as usize, "name longer than 64KB");
            name_pos.push((blob.len() as u32, name.len() as u16));
            blob.extend_from_slice(name.as_bytes());
        }
        // Also lay out aliases in the blob. We collect `(sym, off, len)`
        // tuples now and fold them into the alpha index later.
        //
        // Two stream-quality fixes happen here, deferred to finish so the
        // ingest path can stay branch-free and order-independent:
        //
        // (a) Dedup. A single CU often emits the same `/kythe/edge/named`
        //     alias on dozens of nodes (every reference to a function
        //     inherits its MarkedSource), so the raw Vec routinely has
        //     30× redundancy per CU. Sort+dedup keys on `(sym, alias)`
        //     because the same alias on a different sym is a separate
        //     fact.
        //
        // (b) Variable-kind suppression. cxx_indexer emits `/kythe/code`
        //     MarkedSource for parameters and locals too — the parsed
        //     FQN of `Method::param` is `Method::param`, indistinguishable
        //     from a top-level entity. Without this filter,
        //     `def writeAligned --substr` returns both the method and its
        //     parameter sym, which surprises users. Kind facts can arrive
        //     before or after the code fact in the stream, so we resolve
        //     here when every sym's kind is known.
        let var_syms: std::collections::HashSet<u64> = syms_vec.iter()
            .filter(|(_, k, _, _)| *k == kind::VARIABLE)
            .map(|(s, _, _, _)| *s)
            .collect();
        self.aliases.retain(|(s, _)| !var_syms.contains(s));
        self.aliases.sort_unstable();
        self.aliases.dedup();
        let mut alias_pos: Vec<(u64, u32, u16)> = Vec::with_capacity(self.aliases.len());
        for (sym, alias) in &self.aliases {
            assert!(alias.len() <= u16::MAX as usize, "alias longer than 64KB");
            alias_pos.push((*sym, blob.len() as u32, alias.len() as u16));
            blob.extend_from_slice(alias.as_bytes());
        }
        let mut files_vec: Vec<(u32, String)> = self.files.into_iter().collect();
        files_vec.sort_unstable_by_key(|r| r.0);
        let n_files = files_vec.len() as u64;
        let mut path_pos: Vec<(u32, u16)> = Vec::with_capacity(files_vec.len());
        for (_, p) in &files_vec {
            assert!(p.len() <= u16::MAX as usize, "path longer than 64KB");
            path_pos.push((blob.len() as u32, p.len() as u16));
            blob.extend_from_slice(p.as_bytes());
        }

        // ---- 3. Build alphabetical name index ----
        //
        // Each entry is `(name_off, name_len, sym)`. Canonical-name
        // entries come from `syms_vec` (one per sym); alias entries
        // come from `alias_pos` (zero or more per sym). We merge both
        // sources into one Vec and sort by the name bytes in `blob`.
        let mut by_name: Vec<(u32, u16, u64)> =
            Vec::with_capacity(syms_vec.len() + alias_pos.len());
        for (i, (sym, _, _, _)) in syms_vec.iter().enumerate() {
            let (off, len) = name_pos[i];
            by_name.push((off, len, *sym));
        }
        for (sym, off, len) in &alias_pos {
            by_name.push((*off, *len, *sym));
        }
        by_name.sort_by(|a, b| {
            let an = &blob[a.0 as usize..a.0 as usize + a.1 as usize];
            let bn = &blob[b.0 as usize..b.0 as usize + b.1 as usize];
            an.cmp(bn)
        });
        let n_names = by_name.len() as u64;

        // ---- 4. Sort inherits ----
        self.inherits.sort_unstable();
        self.inherits.dedup();
        let n_inh = self.inherits.len() as u64;

        // ---- 4b. Sort calls (callgraph), once by caller for
        //          `calls_from`, once by callee for `called_by`.
        self.calls.sort_unstable();
        self.calls.dedup();
        let n_calls = self.calls.len() as u64;
        let mut calls_rev: Vec<(u64, u64, u8)> = self.calls.iter()
            .map(|(caller, callee, role)| (*callee, *caller, *role))
            .collect();
        calls_rev.sort_unstable();
        let n_crev = calls_rev.len() as u64;

        // ---- 5. Compute section offsets ----
        let xrefs_off = pad_up(size_of_header() as u64);
        let syms_off  = pad_up(xrefs_off + n_xrefs * XREF_LEN as u64);
        let names_off = pad_up(syms_off  + n_syms  * SYM_LEN  as u64);
        let files_off = pad_up(names_off + n_names * NAME_LEN as u64);
        let inh_off   = pad_up(files_off + n_files * FILE_LEN as u64);
        let calls_off = pad_up(inh_off   + n_inh   * INH_LEN  as u64);
        let crev_off  = pad_up(calls_off + n_calls * CALL_LEN as u64);
        let blob_off  = pad_up(crev_off  + n_crev  * CALL_LEN as u64);

        // ---- 6. Write to a tempfile, then atomic rename ----
        let tmp_path: PathBuf = path.with_extension("s2db.tmp");
        let f = File::create(&tmp_path).with_context(|| format!("create {}", tmp_path.display()))?;
        let mut w = BufWriter::with_capacity(8 << 20, f);

        let hdr = Header {
            magic:   MAGIC,
            version: VERSION,
            xrefs_off, xrefs_n: n_xrefs,
            syms_off,  syms_n:  n_syms,
            names_off, names_n: n_names,
            files_off, files_n: n_files,
            inh_off,   inh_n:   n_inh,
            calls_off, calls_n: n_calls,
            crev_off,  crev_n:  n_crev,
            blob_off,  blob_len: blob.len() as u64,
            ..Default::default()
        };
        write_header(&mut w, &hdr)?;
        seek_to(&mut w, xrefs_off)?;
        for (s, r, f, o) in &self.xrefs {
            w.write_all(&s.to_be_bytes())?;
            w.write_all(&[*r])?;
            w.write_all(&f.to_be_bytes())?;
            w.write_all(&o.to_be_bytes())?;
        }

        seek_to(&mut w, syms_off)?;
        for (i, (sym, kind, lang, _)) in syms_vec.iter().enumerate() {
            let (off, len) = name_pos[i];
            w.write_all(&sym.to_be_bytes())?;
            w.write_all(&[*kind, *lang])?;
            w.write_all(&off.to_be_bytes())?;
            w.write_all(&len.to_be_bytes())?;
        }

        seek_to(&mut w, names_off)?;
        for (off, len, sym) in &by_name {
            w.write_all(&off.to_be_bytes())?;
            w.write_all(&len.to_be_bytes())?;
            w.write_all(&[0u8, 0u8])?;     // _pad
            w.write_all(&sym.to_be_bytes())?;
        }

        seek_to(&mut w, files_off)?;
        for (i, (file, _)) in files_vec.iter().enumerate() {
            let (off, len) = path_pos[i];
            w.write_all(&file.to_be_bytes())?;
            w.write_all(&off.to_be_bytes())?;
            w.write_all(&len.to_be_bytes())?;
        }

        seek_to(&mut w, inh_off)?;
        for (c, p) in &self.inherits {
            w.write_all(&c.to_be_bytes())?;
            w.write_all(&p.to_be_bytes())?;
        }

        seek_to(&mut w, calls_off)?;
        for (caller, callee, role) in &self.calls {
            w.write_all(&caller.to_be_bytes())?;
            w.write_all(&callee.to_be_bytes())?;
            w.write_all(&[*role])?;
        }

        // calls_rev is sorted by (callee, caller, role) but we serialize
        // each row in the SAME byte layout as the forward table —
        // `(field0_u64, field1_u64, role_u8)` — so the reader code is
        // identical. The first u64 in this section is the callee.
        seek_to(&mut w, crev_off)?;
        for (callee, caller, role) in &calls_rev {
            w.write_all(&callee.to_be_bytes())?;
            w.write_all(&caller.to_be_bytes())?;
            w.write_all(&[*role])?;
        }

        seek_to(&mut w, blob_off)?;
        w.write_all(&blob)?;

        let total = blob_off + blob.len() as u64;
        w.flush()?;
        w.get_mut().sync_all().context("fsync")?;
        drop(w);
        std::fs::rename(&tmp_path, path).with_context(|| format!("rename to {}", path.display()))?;
        Ok(total)
    }
}

// ============================================================
// Streaming-merge helpers (used by `write_merged_snapshot`)
// ============================================================
//
// Two-way merge between an mmap'd prior `.s2db` and an in-memory
// `delta` (already sorted). Each function comes in two flavours:
// a `merge_*` iterator that produces the merged stream, and a
// `count_merged_*` companion that just counts (used to size the
// output file's sections before writing).

fn merge_sorted_dedup<T, A, B>(a: A, b: B) -> impl Iterator<Item = T>
where
    T: Ord + Copy,
    A: Iterator<Item = T>,
    B: Iterator<Item = T>,
{
    use std::iter::Peekable;
    struct Iter<T, A, B>
    where A: Iterator<Item = T>, B: Iterator<Item = T>
    {
        a: Peekable<A>,
        b: Peekable<B>,
        last: Option<T>,
    }
    impl<T, A, B> Iterator for Iter<T, A, B>
    where
        T: Ord + Copy,
        A: Iterator<Item = T>,
        B: Iterator<Item = T>,
    {
        type Item = T;
        fn next(&mut self) -> Option<T> {
            loop {
                let v = match (self.a.peek(), self.b.peek()) {
                    (Some(x), Some(y)) => {
                        match x.cmp(y) {
                            std::cmp::Ordering::Less    => self.a.next(),
                            std::cmp::Ordering::Greater => self.b.next(),
                            std::cmp::Ordering::Equal   => {
                                self.b.next();
                                self.a.next()
                            }
                        }
                    }
                    (Some(_), None) => self.a.next(),
                    (None, Some(_)) => self.b.next(),
                    (None, None)    => return None,
                };
                let v = v?;
                if self.last == Some(v) { continue; }
                self.last = Some(v);
                return Some(v);
            }
        }
    }
    Iter { a: a.peekable(), b: b.peekable(), last: None }
}

/// First-wins on tied sym ids. Yields `(sym, kind, lang, name)`.
fn merge_syms_first_wins<'a, A, B>(a: A, b: B) -> impl Iterator<Item = (u64, u8, u8, &'a str)>
where
    A: Iterator<Item = (u64, u8, u8, &'a str)>,
    B: Iterator<Item = (u64, u8, u8, &'a str)>,
{
    use std::iter::Peekable;
    struct Iter<'a, A, B>
    where
        A: Iterator<Item = (u64, u8, u8, &'a str)>,
        B: Iterator<Item = (u64, u8, u8, &'a str)>,
    {
        a: Peekable<A>,
        b: Peekable<B>,
    }
    impl<'a, A, B> Iterator for Iter<'a, A, B>
    where
        A: Iterator<Item = (u64, u8, u8, &'a str)>,
        B: Iterator<Item = (u64, u8, u8, &'a str)>,
    {
        type Item = (u64, u8, u8, &'a str);
        fn next(&mut self) -> Option<Self::Item> {
            match (self.a.peek(), self.b.peek()) {
                (Some(&(ka, _, _, _)), Some(&(kb, _, _, _))) => {
                    match ka.cmp(&kb) {
                        std::cmp::Ordering::Less    => self.a.next(),
                        std::cmp::Ordering::Greater => self.b.next(),
                        std::cmp::Ordering::Equal   => {
                            self.b.next();
                            self.a.next()
                        }
                    }
                }
                (Some(_), None) => self.a.next(),
                (None, Some(_)) => self.b.next(),
                (None, None)    => None,
            }
        }
    }
    Iter { a: a.peekable(), b: b.peekable() }
}

/// First-wins on tied file ids. Yields `(file_id, path)`.
fn merge_files_first_wins<'a, A, B>(a: A, b: B) -> impl Iterator<Item = (u32, &'a str)>
where
    A: Iterator<Item = (u32, &'a str)>,
    B: Iterator<Item = (u32, &'a str)>,
{
    use std::iter::Peekable;
    struct Iter<'a, A, B>
    where
        A: Iterator<Item = (u32, &'a str)>,
        B: Iterator<Item = (u32, &'a str)>,
    {
        a: Peekable<A>,
        b: Peekable<B>,
    }
    impl<'a, A, B> Iterator for Iter<'a, A, B>
    where
        A: Iterator<Item = (u32, &'a str)>,
        B: Iterator<Item = (u32, &'a str)>,
    {
        type Item = (u32, &'a str);
        fn next(&mut self) -> Option<Self::Item> {
            match (self.a.peek(), self.b.peek()) {
                (Some(&(ka, _)), Some(&(kb, _))) => {
                    match ka.cmp(&kb) {
                        std::cmp::Ordering::Less    => self.a.next(),
                        std::cmp::Ordering::Greater => self.b.next(),
                        std::cmp::Ordering::Equal   => {
                            self.b.next();
                            self.a.next()
                        }
                    }
                }
                (Some(_), None) => self.a.next(),
                (None, Some(_)) => self.b.next(),
                (None, None)    => None,
            }
        }
    }
    Iter { a: a.peekable(), b: b.peekable() }
}

/// Aliases sorted by `(sym, alias)` then deduped. Both sources are
/// expected to be sorted+deduped already.
fn merge_aliases_dedup<'a, A, B>(a: A, b: B) -> impl Iterator<Item = (u64, &'a str)>
where
    A: Iterator<Item = (u64, &'a str)>,
    B: Iterator<Item = (u64, &'a str)>,
{
    use std::iter::Peekable;
    struct Iter<'a, A, B>
    where
        A: Iterator<Item = (u64, &'a str)>,
        B: Iterator<Item = (u64, &'a str)>,
    {
        a: Peekable<A>,
        b: Peekable<B>,
        last: Option<(u64, &'a str)>,
    }
    impl<'a, A, B> Iterator for Iter<'a, A, B>
    where
        A: Iterator<Item = (u64, &'a str)>,
        B: Iterator<Item = (u64, &'a str)>,
    {
        type Item = (u64, &'a str);
        fn next(&mut self) -> Option<Self::Item> {
            loop {
                let v = match (self.a.peek(), self.b.peek()) {
                    (Some(&xa), Some(&xb)) => {
                        match xa.cmp(&xb) {
                            std::cmp::Ordering::Less    => self.a.next(),
                            std::cmp::Ordering::Greater => self.b.next(),
                            std::cmp::Ordering::Equal   => {
                                self.b.next();
                                self.a.next()
                            }
                        }
                    }
                    (Some(_), None) => self.a.next(),
                    (None, Some(_)) => self.b.next(),
                    (None, None)    => return None,
                };
                let v = v?;
                if self.last == Some(v) { continue; }
                self.last = Some(v);
                return Some(v);
            }
        }
    }
    Iter { a: a.peekable(), b: b.peekable(), last: None }
}

/// Build the `var_syms` set from the merged sym stream — the final
/// kind for a sym is whichever source wins (prior on ties), and the
/// set is used to suppress alias entries for VARIABLE-kind syms.
fn merge_var_syms(
    prior: Option<&crate::reader::Index>,
    delta_syms: &[(u64, u8, u8, String)],
) -> std::collections::HashSet<u64> {
    let pri: Box<dyn Iterator<Item = (u64, u8, u8, &str)>> = match prior {
        Some(p) => Box::new(p.iter_syms()),
        None    => Box::new(std::iter::empty()),
    };
    let del = delta_syms.iter().map(|(s, k, l, n)| (*s, *k, *l, n.as_str()));
    merge_syms_first_wins(pri, del)
        .filter(|(_, k, _, _)| *k == kind::VARIABLE)
        .map(|(s, _, _, _)| s)
        .collect()
}

fn size_of_header() -> usize { std::mem::size_of::<Header>() }

fn write_header<W: Write + Seek>(w: &mut W, h: &Header) -> Result<()> {
    // SAFETY: Header is repr(C) with no padding holes and all-POD fields.
    // Serializing its byte representation is sound.
    let bytes = unsafe {
        std::slice::from_raw_parts(h as *const Header as *const u8, size_of_header())
    };
    w.write_all(bytes)?;
    Ok(())
}

fn seek_to<W: Write + Seek>(w: &mut W, off: u64) -> Result<()> {
    w.flush()?;
    w.seek(SeekFrom::Start(off))?;
    Ok(())
}
