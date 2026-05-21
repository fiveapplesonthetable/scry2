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
    /// Resolved type of a sym, pre-rendered to a string at ingest
    /// (`/kythe/edge/typed` → rendered type node). `(sym, type_string)`.
    /// Sorted+deduped at write time and emitted as the `typed` section;
    /// on a tied sym a non-empty string wins over an empty one.
    typed:    Vec<(u64, String)>,
    /// Membership edges: `(parent, child)` from `/kythe/edge/childof`
    /// (a child node points at its enclosing parent; we store the
    /// reverse). Emitted, sorted by `(parent, child)`, as the `childrev`
    /// section so `members(parent)` is an O(log n) range scan — the
    /// mirror of how `inhrev` reverses `inherits`. All childof edges are
    /// kept; `members` filters by the parent sym's kind at query time so
    /// function-local children (params/locals) never surface as a
    /// class's members.
    childof:  Vec<(u64, u64)>,
    /// Full rendered signature of a FUNCTION sym, with parameter names
    /// (e.g. "void setEnabled(bool enabled)"). `(sym, sig_string)`.
    /// Pre-rendered at ingest from the function's `param.N` edges + each
    /// param's name/type, plus the return type. Emitted as the `sig`
    /// section (TypeRow sorted by sym). Honest emptiness: a sym with no
    /// renderable signature stores nothing.
    sig:      Vec<(u64, String)>,
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

    /// Record `sym`'s resolved type, already rendered to a display
    /// string (e.g. "const Box<int> &", "java.lang.String"). Empty
    /// strings are dropped — "honest emptiness": a sym with no
    /// renderable type stores nothing and `type_of` returns None rather
    /// than a guess.
    pub fn add_type(&mut self, sym: u64, type_str: &str) {
        if type_str.is_empty() { return; }
        self.typed.push((sym, type_str.to_string()));
    }

    /// Record a `/kythe/edge/childof` edge: `child` is enclosed by
    /// `parent`. Stored reversed as `(parent, child)` so the `childrev`
    /// section is sorted by parent and `members(parent)` is a range
    /// scan. All edges are kept; the `members` verb filters by the
    /// parent sym's kind so function-local children never surface as a
    /// class's members.
    pub fn add_childof(&mut self, child: u64, parent: u64) {
        self.childof.push((parent, child));
    }

    /// Record `sym`'s full rendered signature with parameter names
    /// (e.g. "void setEnabled(bool enabled)"). Empty strings are dropped
    /// — "honest emptiness": a sym with no renderable signature stores
    /// nothing and `sig_of` returns None rather than a guess.
    pub fn add_sig(&mut self, sym: u64, sig: &str) {
        if sig.is_empty() { return; }
        self.sig.push((sym, sig.to_string()));
    }

    pub fn n_xrefs(&self) -> usize { self.xrefs.len() }
    pub fn n_syms(&self)  -> usize { self.syms.len() }
    pub fn n_files(&self) -> usize { self.files.len() }
    pub fn n_inh(&self)   -> usize { self.inherits.len() }
    pub fn n_aliases(&self) -> usize { self.aliases.len() }
    pub fn n_calls(&self) -> usize { self.calls.len() }
    pub fn n_typed(&self) -> usize { self.typed.len() }
    pub fn n_childof(&self) -> usize { self.childof.len() }
    pub fn n_sig(&self)   -> usize { self.sig.len() }

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
        // Refine, don't blind first-wins: a sym referenced in one CU
        // (kind::UNK) and defined in another (known kind) must end up
        // with the known kind regardless of drain order — matching
        // `upsert_sym`'s in-builder semantics, so `index` and
        // `from-kzip` agree and the result is order-independent.
        for (k, (kind, lang, name)) in other.syms {
            use std::collections::hash_map::Entry;
            match self.syms.entry(k) {
                Entry::Occupied(mut o) => {
                    let e = o.get_mut();
                    if e.0 == kind::UNK { e.0 = kind; }
                    if e.1 == lang::UNK { e.1 = lang; }
                    if e.2.is_empty()   { e.2 = name; }
                }
                Entry::Vacant(v) => { v.insert((kind, lang, name)); }
            }
        }
        for (k, v) in other.files {
            self.files.entry(k).or_insert(v);
        }
        self.inherits.append(&mut other.inherits);
        self.aliases.append(&mut other.aliases);
        self.calls.append(&mut other.calls);
        self.typed.append(&mut other.typed);
        self.childof.append(&mut other.childof);
        self.sig.append(&mut other.sig);
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
        self.typed.sort_unstable();
        self.typed.dedup();
        self.childof.sort_unstable();
        self.childof.dedup();
        self.sig.sort_unstable();
        self.sig.dedup();
        self.xrefs.len() + self.inherits.len() + self.calls.len()
            + self.aliases.len() + self.typed.len()
            + self.childof.len() + self.sig.len()
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
        for (sym, ty) in ix.iter_typed() {
            self.add_type(sym, ty);
        }
        for (parent, child) in ix.iter_childrev() {
            // iter_childrev yields (parent, child); add_childof takes
            // (child, parent) and re-reverses, so round-trip the order.
            self.add_childof(child, parent);
        }
        for (sym, sig) in ix.iter_sig() {
            self.add_sig(sym, sig);
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
        sources: &[&crate::reader::Index],
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
        // Collapse the delta's typed table to one row per sym (sorted by
        // sym), the shape `merge_typed_by_sym` expects. After sort the
        // first row per sym is the smallest non-empty string (`add_type`
        // drops empties) — same per-sym choice `finish` makes.
        self.typed.sort_unstable();
        self.typed.dedup();
        self.typed.dedup_by_key(|(s, _)| *s);
        let mut delta_typed: Vec<(u64, &str)> =
            self.typed.iter().map(|(s, t)| (*s, t.as_str())).collect();
        delta_typed.sort_unstable_by_key(|r| r.0);
        // childof is already stored as (parent, child); sort by that key so
        // the `childrev` fold (merge_sorted_dedup over (parent, child)) sees
        // a sorted, deduped delta stream — exactly like `inhrev`.
        self.childof.sort_unstable();
        self.childof.dedup();
        // Collapse the delta's sig table to one row per sym (sorted by sym),
        // the shape `merge_typed_by_sym` expects (it's reused for sig — both
        // are sym→string TypeRow sections with non-empty-wins on a tie).
        self.sig.sort_unstable();
        self.sig.dedup();
        self.sig.dedup_by_key(|(s, _)| *s);
        let mut delta_sig: Vec<(u64, &str)> =
            self.sig.iter().map(|(s, t)| (*s, t.as_str())).collect();
        delta_sig.sort_unstable_by_key(|r| r.0);
        let mut delta_syms: Vec<(u64, u8, u8, String)> = self.syms.drain()
            .map(|(s, (k, l, n))| (s, k, l, n)).collect();
        delta_syms.sort_unstable_by_key(|r| r.0);
        let mut delta_files: Vec<(u32, String)> = self.files.drain().collect();
        delta_files.sort_unstable_by_key(|r| r.0);

        // ---- 2. var_syms (for alias suppression) ----
        // Variable-kind syms suppress their own aliases. The kind is
        // known for every sym during the syms write pass below, so we
        // collect the set there (one walk, no separate pre-scan) and
        // consume it in the aliases pass that follows. by_name has no
        // pre-sized capacity, so nothing here depends on knowing the
        // set first.
        let mut var_syms: std::collections::HashSet<u64> = std::collections::HashSet::new();

        // Every source's aliases come back from `iter_aliases` in *alpha*
        // order. Gather them all once into a `(sym, alias)`-sorted Vec
        // borrowing into the mmap'd blobs — no string copies, bounded by
        // alias count. One gather for the whole k-way merge, not one per
        // shard (that re-gather was the old chained fold's memory cost).
        let mut prior_aliases: Vec<(u64, &str)> = Vec::new();
        for s in sources { prior_aliases.extend(s.iter_aliases()); }
        prior_aliases.sort_unstable();
        prior_aliases.dedup();

        // ---- 3. Open tmp; deferred-header layout ----
        // We don't pre-count merged row totals (that was the source
        // of the 14-pass snap slowdown — every count walked prior
        // end-to-end). Instead each section's row count is observed
        // while we write it; the header at byte 0 is filled in last
        // via a seek-back. Section offsets are page-aligned positions
        // chosen incrementally as each prior section's `n` lands.
        // pid-suffixed so two processes writing distinct outputs in the
        // same dir (or one writing while another lists) never share a tmp.
        let tmp_path: PathBuf = output.with_extension(format!("s2db.tmp.{}", std::process::id()));
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
            let pri = fold_sources(sources,
                |s| Box::new(s.iter_xrefs()),
                |a, b| Box::new(merge_sorted_dedup(a, b)));
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
        let mut by_name: Vec<(u64, u16, u64)> = Vec::new();
        {
            let pri = fold_sources(sources,
                |s| Box::new(s.iter_syms()),
                |a, b| Box::new(merge_syms_refine(a, b)));
            let del = delta_syms.iter().map(|(s, k, l, n)| (*s, *k, *l, n.as_str()));
            for (sym, kind, lang, name) in merge_syms_refine(pri, del) {
                if kind == kind::VARIABLE { var_syms.insert(sym); }
                let name = clamp_blob_str(name);
                let off = blob.len() as u64;
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
                let alias = clamp_blob_str(alias);
                let off = blob.len() as u64;
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
            // Tie-break by sym id so two distinct syms sharing one name get
            // a deterministic order; otherwise HashMap iteration order leaks
            // in and the same name query can resolve to a different sym
            // across builds of identical input.
            an.cmp(bn).then(a.2.cmp(&b.2))
        });
        seek_to(&mut w, names_off)?;
        for (off, len, sym) in &by_name {
            w.write_all(&off.to_be_bytes())?;
            w.write_all(&len.to_be_bytes())?;
            w.write_all(&sym.to_be_bytes())?;
        }
        drop(by_name);

        // ---- 8. files (write + count + append paths to blob) ----
        let files_off = pad_up(names_off + n_names * NAME_LEN as u64);
        seek_to(&mut w, files_off)?;
        let mut n_files: u64 = 0;
        {
            let pri = fold_sources(sources,
                |s| Box::new(s.iter_files()),
                |a, b| Box::new(merge_files_first_wins(a, b)));
            let del = delta_files.iter().map(|(f, p)| (*f, p.as_str()));
            for (file, path) in merge_files_first_wins(pri, del) {
                let path = clamp_blob_str(path);
                let off = blob.len() as u64;
                let len = path.len() as u16;
                blob.extend_from_slice(path.as_bytes());
                w.write_all(&file.to_be_bytes())?;
                w.write_all(&off.to_be_bytes())?;
                w.write_all(&len.to_be_bytes())?;
                n_files += 1;
            }
        }

        // ---- 9. inherits (write + collect for the reverse index) ----
        let inh_off = pad_up(files_off + n_files * FILE_LEN as u64);
        seek_to(&mut w, inh_off)?;
        let mut merged_inh: Vec<(u64, u64)> = Vec::new();
        {
            let pri = fold_sources(sources,
                |s| Box::new(s.iter_inherits()),
                |a, b| Box::new(merge_sorted_dedup(a, b)));
            let del = self.inherits.iter().copied();
            for (c, p) in merge_sorted_dedup(pri, del) {
                w.write_all(&c.to_be_bytes())?;
                w.write_all(&p.to_be_bytes())?;
                merged_inh.push((c, p));
            }
        }
        let n_inh = merged_inh.len() as u64;

        // ---- 9b. inhrev (by-parent): the SAME edges as `inh`, reversed
        //      to (parent, child) and re-sorted, so `inherited_by(parent)`
        //      is an O(log n) range scan. Mirror of `crev` over `calls`.
        let inhrev_off = pad_up(inh_off + n_inh * INH_LEN as u64);
        seek_to(&mut w, inhrev_off)?;
        let mut inh_rev: Vec<(u64, u64)> = merged_inh.into_iter()
            .map(|(c, p)| (p, c))
            .collect();
        inh_rev.sort_unstable();
        for (p, c) in &inh_rev {
            w.write_all(&p.to_be_bytes())?;
            w.write_all(&c.to_be_bytes())?;
        }
        let n_inhrev = inh_rev.len() as u64;
        drop(inh_rev);

        // ---- 10. calls (write + collect for the reverse index) ----
        let calls_off = pad_up(inhrev_off + n_inhrev * INH_LEN as u64);
        seek_to(&mut w, calls_off)?;
        let mut merged_calls: Vec<(u64, u64, u8)> = Vec::new();
        {
            let pri = fold_sources(sources,
                |s| Box::new(s.iter_calls()),
                |a, b| Box::new(merge_sorted_dedup(a, b)));
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

        // ---- 11b. typed (write + count; strings appended to blob) ----
        // Fold every source's sym-sorted typed table plus the delta into
        // one row per sym (non-empty wins on a tie), exactly like every
        // other section. Strings append to the same blob as names/paths.
        let typed_off = pad_up(crev_off + n_calls * CALL_LEN as u64);
        seek_to(&mut w, typed_off)?;
        let mut n_typed: u64 = 0;
        {
            let pri = fold_sources(sources,
                |s| Box::new(s.iter_typed()),
                |a, b| Box::new(merge_typed_by_sym(a, b)));
            let del = delta_typed.iter().copied();
            for (sym, ty) in merge_typed_by_sym(pri, del) {
                if ty.is_empty() { continue; }
                let ty = clamp_blob_str(ty);
                let off = blob.len() as u64;
                let len = ty.len() as u16;
                blob.extend_from_slice(ty.as_bytes());
                w.write_all(&sym.to_be_bytes())?;
                w.write_all(&off.to_be_bytes())?;
                w.write_all(&len.to_be_bytes())?;
                n_typed += 1;
            }
        }

        // ---- 11c. childrev (by-parent membership): fold every source's
        //      (parent, child) childrev table plus the delta. Both halves
        //      are sorted by (parent, child), so a plain dedup merge keeps
        //      it sorted — same shape as `inhrev`. INH_LEN rows.
        let childrev_off = pad_up(typed_off + n_typed * TYPE_LEN as u64);
        seek_to(&mut w, childrev_off)?;
        let mut n_childrev: u64 = 0;
        {
            let pri = fold_sources(sources,
                |s| Box::new(s.iter_childrev()),
                |a, b| Box::new(merge_sorted_dedup(a, b)));
            let del = self.childof.iter().copied();
            for (parent, child) in merge_sorted_dedup(pri, del) {
                w.write_all(&parent.to_be_bytes())?;
                w.write_all(&child.to_be_bytes())?;
                n_childrev += 1;
            }
        }

        // ---- 11d. sig (sym → full signature string) ----
        // Same TypeRow shape + sym-keyed non-empty-wins fold as `typed`;
        // reuses `merge_typed_by_sym`. Strings append to the shared blob.
        let sig_off = pad_up(childrev_off + n_childrev * INH_LEN as u64);
        seek_to(&mut w, sig_off)?;
        let mut n_sig: u64 = 0;
        {
            let pri = fold_sources(sources,
                |s| Box::new(s.iter_sig()),
                |a, b| Box::new(merge_typed_by_sym(a, b)));
            let del = delta_sig.iter().copied();
            for (sym, sg) in merge_typed_by_sym(pri, del) {
                if sg.is_empty() { continue; }
                let sg = clamp_blob_str(sg);
                let off = blob.len() as u64;
                let len = sg.len() as u16;
                blob.extend_from_slice(sg.as_bytes());
                w.write_all(&sym.to_be_bytes())?;
                w.write_all(&off.to_be_bytes())?;
                w.write_all(&len.to_be_bytes())?;
                n_sig += 1;
            }
        }

        // ---- 12. blob ----
        let blob_off = pad_up(sig_off + n_sig * TYPE_LEN as u64);
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
            inhrev_off, inhrev_n: n_inhrev,
            calls_off, calls_n: n_calls,
            crev_off,  crev_n:  n_calls,
            typed_off, typed_n: n_typed,
            childrev_off, childrev_n: n_childrev,
            sig_off,   sig_n:   n_sig,
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
        let mut name_pos: Vec<(u64, u16)> = Vec::with_capacity(syms_vec.len());
        for (_, _, _, name) in &syms_vec {
            let name = clamp_blob_str(name);
            name_pos.push((blob.len() as u64, name.len() as u16));
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
        let mut alias_pos: Vec<(u64, u64, u16)> = Vec::with_capacity(self.aliases.len());
        for (sym, alias) in &self.aliases {
            let alias = clamp_blob_str(alias);
            alias_pos.push((*sym, blob.len() as u64, alias.len() as u16));
            blob.extend_from_slice(alias.as_bytes());
        }
        let mut files_vec: Vec<(u32, String)> = self.files.into_iter().collect();
        files_vec.sort_unstable_by_key(|r| r.0);
        let n_files = files_vec.len() as u64;
        let mut path_pos: Vec<(u64, u16)> = Vec::with_capacity(files_vec.len());
        for (_, p) in &files_vec {
            let p = clamp_blob_str(p);
            path_pos.push((blob.len() as u64, p.len() as u16));
            blob.extend_from_slice(p.as_bytes());
        }

        // ---- 3. Build alphabetical name index ----
        //
        // Each entry is `(name_off, name_len, sym)`. Canonical-name
        // entries come from `syms_vec` (one per sym); alias entries
        // come from `alias_pos` (zero or more per sym). We merge both
        // sources into one Vec and sort by the name bytes in `blob`.
        let mut by_name: Vec<(u64, u16, u64)> =
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
            // Tie-break by sym id so two distinct syms sharing one name get
            // a deterministic order; otherwise HashMap iteration order leaks
            // in and the same name query can resolve to a different sym
            // across builds of identical input.
            an.cmp(bn).then(a.2.cmp(&b.2))
        });
        let n_names = by_name.len() as u64;

        // ---- 4. Sort inherits ----
        self.inherits.sort_unstable();
        self.inherits.dedup();
        let n_inh = self.inherits.len() as u64;
        // inhrev: the SAME edges as `inh`, reversed to (parent, child) and
        // re-sorted, so `inherited_by(parent)` is an O(log n) range scan.
        // Mirror of how `crev` reverses `calls`.
        let mut inh_rev: Vec<(u64, u64)> = self.inherits.iter()
            .map(|(c, p)| (*p, *c))
            .collect();
        inh_rev.sort_unstable();
        let n_inhrev = inh_rev.len() as u64;

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

        // ---- 4c. typed (sym → resolved type string) ----
        // Sort by (sym, string) then collapse to ONE row per sym. After
        // the sort the first row for each sym is the lexicographically
        // smallest non-empty string (`add_type` already drops empties),
        // a deterministic choice when two CUs rendered the same sym's
        // type slightly differently. Strings append to the blob; the
        // TypeRow table stays sorted by sym so `type_of` is a binary
        // search.
        self.typed.sort_unstable();
        self.typed.dedup();
        let mut typed_pos: Vec<(u64, u64, u16)> = Vec::with_capacity(self.typed.len());
        let mut last_typed_sym: Option<u64> = None;
        for (sym, ty) in &self.typed {
            if last_typed_sym == Some(*sym) { continue; }   // one row per sym
            last_typed_sym = Some(*sym);
            let ty = clamp_blob_str(ty);
            typed_pos.push((*sym, blob.len() as u64, ty.len() as u16));
            blob.extend_from_slice(ty.as_bytes());
        }
        let n_typed = typed_pos.len() as u64;

        // ---- 4d. childrev (parent → child membership) ----
        // childof was recorded reversed as (parent, child); sort by that
        // key so `members(parent)` is an O(log n) range scan. Mirror of
        // `inhrev`. All edges kept; `members` filters by parent kind.
        self.childof.sort_unstable();
        self.childof.dedup();
        let n_childrev = self.childof.len() as u64;

        // ---- 4e. sig (sym → full signature string) ----
        // Same one-row-per-sym collapse as `typed`; strings append to the
        // shared blob. The table stays sorted by sym for a binary search.
        self.sig.sort_unstable();
        self.sig.dedup();
        let mut sig_pos: Vec<(u64, u64, u16)> = Vec::with_capacity(self.sig.len());
        let mut last_sig_sym: Option<u64> = None;
        for (sym, sg) in &self.sig {
            if last_sig_sym == Some(*sym) { continue; }   // one row per sym
            last_sig_sym = Some(*sym);
            let sg = clamp_blob_str(sg);
            sig_pos.push((*sym, blob.len() as u64, sg.len() as u16));
            blob.extend_from_slice(sg.as_bytes());
        }
        let n_sig = sig_pos.len() as u64;

        // ---- 5. Compute section offsets ----
        let xrefs_off = pad_up(size_of_header() as u64);
        let syms_off  = pad_up(xrefs_off + n_xrefs * XREF_LEN as u64);
        let names_off = pad_up(syms_off  + n_syms  * SYM_LEN  as u64);
        let files_off = pad_up(names_off + n_names * NAME_LEN as u64);
        let inh_off   = pad_up(files_off + n_files * FILE_LEN as u64);
        let inhrev_off = pad_up(inh_off  + n_inh   * INH_LEN  as u64);
        let calls_off = pad_up(inhrev_off + n_inhrev * INH_LEN as u64);
        let crev_off  = pad_up(calls_off + n_calls * CALL_LEN as u64);
        let typed_off = pad_up(crev_off  + n_crev  * CALL_LEN as u64);
        let childrev_off = pad_up(typed_off + n_typed * TYPE_LEN as u64);
        let sig_off   = pad_up(childrev_off + n_childrev * INH_LEN as u64);
        let blob_off  = pad_up(sig_off   + n_sig   * TYPE_LEN as u64);

        // ---- 6. Write to a tempfile, then atomic rename ----
        // pid-suffixed so concurrent writers never collide on the tmp.
        let tmp_path: PathBuf = path.with_extension(format!("s2db.tmp.{}", std::process::id()));
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
            inhrev_off, inhrev_n: n_inhrev,
            calls_off, calls_n: n_calls,
            crev_off,  crev_n:  n_crev,
            typed_off, typed_n: n_typed,
            childrev_off, childrev_n: n_childrev,
            sig_off,   sig_n:   n_sig,
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

        // inhrev is the same edges sorted by parent. Same INH_LEN byte
        // layout as `inh`; the first u64 in this section is the parent.
        seek_to(&mut w, inhrev_off)?;
        for (p, c) in &inh_rev {
            w.write_all(&p.to_be_bytes())?;
            w.write_all(&c.to_be_bytes())?;
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

        seek_to(&mut w, typed_off)?;
        for (sym, off, len) in &typed_pos {
            w.write_all(&sym.to_be_bytes())?;
            w.write_all(&off.to_be_bytes())?;
            w.write_all(&len.to_be_bytes())?;
        }

        // childrev: (parent, child) sorted by parent. Same INH_LEN layout
        // as `inh`/`inhrev`; the first u64 here is the parent.
        seek_to(&mut w, childrev_off)?;
        for (parent, child) in &self.childof {
            w.write_all(&parent.to_be_bytes())?;
            w.write_all(&child.to_be_bytes())?;
        }

        seek_to(&mut w, sig_off)?;
        for (sym, off, len) in &sig_pos {
            w.write_all(&sym.to_be_bytes())?;
            w.write_all(&off.to_be_bytes())?;
            w.write_all(&len.to_be_bytes())?;
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

/// Clamp a blob string to at most `u16::MAX` bytes on a char boundary.
/// `name_len`/`path_len` are u16, so a longer string would either panic
/// (finish's old `assert!`) or silently truncate the length (merge's
/// `as u16`) and mis-slice the blob. Names this long are deeply nested
/// C++ template USRs and vanishingly rare; a truncated-but-consistent
/// name is strictly better than a crash or a corrupt one.
fn clamp_blob_str(s: &str) -> &str {
    if s.len() <= u16::MAX as usize { return s; }
    let mut end = u16::MAX as usize;
    while end > 0 && !s.is_char_boundary(end) { end -= 1; }
    &s[..end]
}

/// Fold every source's section stream into one k-way merge via the given
/// 2-way `merge`, in a single pass. A merge of sorted streams stays
/// sorted (and deduped/refined by the chosen `merge`), so folding N
/// sources left-deep is identical to what `finish` would produce on
/// their concatenation — but each source is read exactly once (no
/// O(N*shards) re-reads of a growing accumulator) and the output blob is
/// built once (RAM bounded regardless of shard count).
fn fold_sources<'s, T, M>(
    sources: &[&'s crate::reader::Index],
    iter_of: impl Fn(&'s crate::reader::Index) -> Box<dyn Iterator<Item = T> + 's>,
    merge: M,
) -> Box<dyn Iterator<Item = T> + 's>
where
    T: 's,
    M: Fn(Box<dyn Iterator<Item = T> + 's>, Box<dyn Iterator<Item = T> + 's>)
        -> Box<dyn Iterator<Item = T> + 's>,
{
    let mut acc: Box<dyn Iterator<Item = T> + 's> = Box::new(std::iter::empty());
    for s in sources {
        acc = merge(acc, iter_of(s));
    }
    acc
}

/// Merge two sym streams (each sorted by sym id), refining on tied ids.
/// Yields `(sym, kind, lang, name)`. On a tie, a known kind/lang/name
/// from either side wins over the other's UNK/empty — mirroring
/// `upsert_sym`/`merge_from`, so a sym referenced as UNK in one shard
/// and defined in another ends up defined regardless of merge order.
fn merge_syms_refine<'a, A, B>(a: A, b: B) -> impl Iterator<Item = (u64, u8, u8, &'a str)>
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
                            let (sym, ak, al, an) = self.a.next().unwrap();
                            let (_,  bk, bl, bn) = self.b.next().unwrap();
                            let kind = if ak != kind::UNK { ak } else { bk };
                            let lang = if al != lang::UNK { al } else { bl };
                            let name = if !an.is_empty()  { an } else { bn };
                            Some((sym, kind, lang, name))
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

/// Merge two `(sym, type_string)` streams, each sorted by sym with one
/// row per sym, into one row per sym. On a tied sym the non-empty string
/// wins; if both are non-empty (two CUs rendered the same sym's type
/// differently) the lexicographically smaller wins — a deterministic
/// tie-break matching `finish`, which dedups its sorted typed Vec and
/// keeps the first (smallest) string per sym. Mirrors the sym-keyed fold
/// every other section uses so the k-way merge equals a single `finish`.
fn merge_typed_by_sym<'a, A, B>(a: A, b: B) -> impl Iterator<Item = (u64, &'a str)>
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
    }
    fn pick<'a>(x: (u64, &'a str), y: (u64, &'a str)) -> (u64, &'a str) {
        // Same sym; choose the better string. Empty loses; otherwise the
        // lexically smaller wins (deterministic, finish-compatible).
        match (x.1.is_empty(), y.1.is_empty()) {
            (false, true) => x,
            (true, false) => y,
            _ => if x.1 <= y.1 { x } else { y },
        }
    }
    impl<'a, A, B> Iterator for Iter<'a, A, B>
    where
        A: Iterator<Item = (u64, &'a str)>,
        B: Iterator<Item = (u64, &'a str)>,
    {
        type Item = (u64, &'a str);
        fn next(&mut self) -> Option<Self::Item> {
            match (self.a.peek(), self.b.peek()) {
                (Some(&(ka, _)), Some(&(kb, _))) => {
                    match ka.cmp(&kb) {
                        std::cmp::Ordering::Less    => self.a.next(),
                        std::cmp::Ordering::Greater => self.b.next(),
                        std::cmp::Ordering::Equal   => {
                            let x = self.a.next().unwrap();
                            let y = self.b.next().unwrap();
                            Some(pick(x, y))
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
