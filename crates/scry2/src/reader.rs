//! .s2db reader. Opens an index file via mmap and exposes the verbs:
//!   - sym_for_name(name)        → u64
//!   - xrefs(sym, role_filter)   → iterator over (file_id, offset)
//!   - sym_meta(sym)             → (name, kind, lang)
//!   - file_path(file)           → &str
//!   - inherited_by(parent)      → Vec<child>
//!   - inherits(child)           → Vec<parent>

use crate::format::*;
use anyhow::{bail, Context, Result};
use memmap2::Mmap;
use std::fs::File;
use std::path::Path;

pub struct Index {
    _file: File,
    map:   Mmap,
    hdr:   Header,
}

/// A trigram's posting descriptor, read straight from its dict row with no
/// blob decode. `off`/`len` bound the trigram's block-skip region in the
/// postings blob; `count` is the number of postings (ascending name-row-ids)
/// — the selectivity the galloping intersection uses to pick a driver list.
#[derive(Clone, Copy, Debug)]
struct Posting {
    off:   usize,
    len:   usize,
    count: usize,
}

impl Index {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let map = unsafe { Mmap::map(&file).context("mmap")? };
        if map.len() < std::mem::size_of::<Header>() {
            bail!("file too small for header ({} bytes)", map.len());
        }
        let hdr: Header = unsafe { std::ptr::read_unaligned(map.as_ptr() as *const Header) };
        if hdr.magic != MAGIC {
            bail!("bad magic: not a scry2 index");
        }
        if hdr.version < MIN_VERSION || hdr.version > VERSION {
            bail!("unsupported version: file={} reader supports {}..={}", hdr.version, MIN_VERSION, VERSION);
        }
        // Validate every section fits within the mapped file. A truncated
        // or zero-length .s2db (e.g. a shard whose write was killed) would
        // otherwise panic with an out-of-range slice index deep inside a
        // query or the final merge's shard re-read. Checked arithmetic so
        // a corrupt header can't overflow into a spurious in-range end.
        let map_len = map.len() as u64;
        let sections: [(&str, u64, u64, u64); 14] = [
            ("xrefs",    hdr.xrefs_off,    hdr.xrefs_n,    XREF_LEN as u64),
            ("syms",     hdr.syms_off,     hdr.syms_n,     SYM_LEN  as u64),
            ("names",    hdr.names_off,    hdr.names_n,    NAME_LEN as u64),
            ("files",    hdr.files_off,    hdr.files_n,    FILE_LEN as u64),
            ("inh",      hdr.inh_off,      hdr.inh_n,      INH_LEN  as u64),
            ("calls",    hdr.calls_off,    hdr.calls_n,    CALL_LEN as u64),
            ("crev",     hdr.crev_off,     hdr.crev_n,     CALL_LEN as u64),
            ("typed",    hdr.typed_off,    hdr.typed_n,    TYPE_LEN as u64),
            ("childrev", hdr.childrev_off, hdr.childrev_n, INH_LEN  as u64),
            ("inhrev",   hdr.inhrev_off,   hdr.inhrev_n,   INH_LEN  as u64),
            ("sig",      hdr.sig_off,      hdr.sig_n,      TYPE_LEN as u64),
            // Trigram dict counts rows (TRIGRAM_LEN each); postings is a
            // flat byte run (stride 1) of block-skip / gap-delta varints.
            ("trigram_dict", hdr.trigram_dict_off, hdr.trigram_dict_n, TRIGRAM_LEN as u64),
            ("trigram_post", hdr.trigram_post_off, hdr.trigram_post_len, 1),
            ("blob",     hdr.blob_off,     hdr.blob_len,   1),
        ];
        for (name, off, n, stride) in sections {
            let end = n.checked_mul(stride)
                .and_then(|bytes| off.checked_add(bytes))
                .with_context(|| format!("{name} section size overflow in header"))?;
            if end > map_len {
                bail!("{name} section [{off}, {end}) exceeds file ({map_len} bytes) — truncated or corrupt index");
            }
        }
        Ok(Self { _file: file, map, hdr })
    }

    pub fn n_xrefs(&self) -> u64 { self.hdr.xrefs_n }
    pub fn n_syms(&self)  -> u64 { self.hdr.syms_n }
    pub fn n_files(&self) -> u64 { self.hdr.files_n }
    pub fn n_inh(&self)   -> u64 { self.hdr.inh_n }
    pub fn n_calls(&self) -> u64 { self.hdr.calls_n }
    pub fn n_names(&self) -> u64 { self.hdr.names_n }
    pub fn n_typed(&self) -> u64 { self.hdr.typed_n }
    pub fn n_inhrev(&self) -> u64 { self.hdr.inhrev_n }
    pub fn n_childrev(&self) -> u64 { self.hdr.childrev_n }
    pub fn n_sig(&self) -> u64 { self.hdr.sig_n }
    pub fn n_trigram_dict(&self) -> u64 { self.hdr.trigram_dict_n }

    // -- raw section slices --------------------------------------------------

    fn xrefs_slice(&self) -> &[u8] {
        let off = self.hdr.xrefs_off as usize;
        let len = self.hdr.xrefs_n as usize * XREF_LEN;
        &self.map[off..off + len]
    }
    fn syms_slice(&self) -> &[u8] {
        let off = self.hdr.syms_off as usize;
        let len = self.hdr.syms_n as usize * SYM_LEN;
        &self.map[off..off + len]
    }
    fn names_slice(&self) -> &[u8] {
        let off = self.hdr.names_off as usize;
        let len = self.hdr.names_n as usize * NAME_LEN;
        &self.map[off..off + len]
    }
    fn files_slice(&self) -> &[u8] {
        let off = self.hdr.files_off as usize;
        let len = self.hdr.files_n as usize * FILE_LEN;
        &self.map[off..off + len]
    }
    fn inh_slice(&self) -> &[u8] {
        let off = self.hdr.inh_off as usize;
        let len = self.hdr.inh_n as usize * INH_LEN;
        &self.map[off..off + len]
    }
    fn calls_slice(&self) -> &[u8] {
        let off = self.hdr.calls_off as usize;
        let len = self.hdr.calls_n as usize * CALL_LEN;
        &self.map[off..off + len]
    }
    fn crev_slice(&self) -> &[u8] {
        let off = self.hdr.crev_off as usize;
        let len = self.hdr.crev_n as usize * CALL_LEN;
        &self.map[off..off + len]
    }
    fn typed_slice(&self) -> &[u8] {
        let off = self.hdr.typed_off as usize;
        let len = self.hdr.typed_n as usize * TYPE_LEN;
        &self.map[off..off + len]
    }
    fn inhrev_slice(&self) -> &[u8] {
        let off = self.hdr.inhrev_off as usize;
        let len = self.hdr.inhrev_n as usize * INH_LEN;
        &self.map[off..off + len]
    }
    fn childrev_slice(&self) -> &[u8] {
        let off = self.hdr.childrev_off as usize;
        let len = self.hdr.childrev_n as usize * INH_LEN;
        &self.map[off..off + len]
    }
    fn sig_slice(&self) -> &[u8] {
        let off = self.hdr.sig_off as usize;
        let len = self.hdr.sig_n as usize * TYPE_LEN;
        &self.map[off..off + len]
    }
    fn blob(&self) -> &[u8] {
        let off = self.hdr.blob_off as usize;
        let len = self.hdr.blob_len as usize;
        &self.map[off..off + len]
    }
    fn trigram_dict_slice(&self) -> &[u8] {
        let off = self.hdr.trigram_dict_off as usize;
        let len = self.hdr.trigram_dict_n as usize * TRIGRAM_LEN;
        &self.map[off..off + len]
    }
    fn trigram_post_slice(&self) -> &[u8] {
        let off = self.hdr.trigram_post_off as usize;
        let len = self.hdr.trigram_post_len as usize;
        &self.map[off..off + len]
    }

    fn blob_str(&self, off: u64, len: u16) -> &str {
        let s = &self.blob()[off as usize..off as usize + len as usize];
        std::str::from_utf8(s).unwrap_or("<bad utf8>")
    }

    // -- name → sym ----------------------------------------------------------

    /// Binary search the alphabetical name index for an exact match,
    /// returning the FIRST landing only. A name can map to several syms —
    /// overloads, per-jar copies of the same method, language-pair
    /// variants — which sit contiguously in the (name, sym)-sorted index;
    /// this returns whichever the binary search lands on (alphabetically
    /// first by the tie-break on `sym`), which for an ambiguous name may be
    /// a variant with no xrefs. Callers that need ALL syms of an exact
    /// name (the def/ref/callers verbs do) must use [`Self::syms_for_name`].
    pub fn sym_for_name(&self, query: &str) -> Option<u64> {
        let names = self.names_slice();
        let n = self.hdr.names_n as usize;
        let qb = query.as_bytes();
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let row_off = mid * NAME_LEN;
            let name_off = u64::from_be_bytes(names[row_off..row_off + 8].try_into().unwrap());
            let name_len = u16::from_be_bytes(names[row_off + 8..row_off + 10].try_into().unwrap());
            let row_name = self.blob_str(name_off, name_len);
            match row_name.as_bytes().cmp(qb) {
                std::cmp::Ordering::Less    => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal   => {
                    let sym = u64::from_be_bytes(names[row_off + 10..row_off + 18].try_into().unwrap());
                    return Some(sym);
                }
            }
        }
        None
    }

    /// Every sym whose name (canonical or alias) exactly equals `query`.
    /// A name can map to several syms — a Java method present in many stub
    /// variants, or an overload set — and they sit contiguously in the
    /// (name, sym)-sorted index. `sym_for_name` returns only the
    /// binary-search landing, which for an ambiguous name may be a variant
    /// with no xrefs; def/ref/callers aggregate over all of them instead.
    pub fn syms_for_name(&self, query: &str) -> Vec<u64> {
        let names = self.names_slice();
        let n = self.hdr.names_n as usize;
        let qb = query.as_bytes();
        // Lower bound: first row whose name >= query.
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let off = mid * NAME_LEN;
            let no = u64::from_be_bytes(names[off..off + 8].try_into().unwrap());
            let nl = u16::from_be_bytes(names[off + 8..off + 10].try_into().unwrap());
            if self.blob_str(no, nl).as_bytes() < qb { lo = mid + 1; } else { hi = mid; }
        }
        // Walk the contiguous run of exact matches.
        let mut out = Vec::new();
        let mut i = lo;
        while i < n {
            let off = i * NAME_LEN;
            let no = u64::from_be_bytes(names[off..off + 8].try_into().unwrap());
            let nl = u16::from_be_bytes(names[off + 8..off + 10].try_into().unwrap());
            if self.blob_str(no, nl).as_bytes() != qb { break; }
            out.push(u64::from_be_bytes(names[off + 10..off + 18].try_into().unwrap()));
            i += 1;
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Diagnostic: yield up to `limit` name rows whose stored bytes
    /// match `prefix`. Walks the alphabetical name index from the
    /// lower bound for `prefix` until the prefix no longer matches.
    /// Used by `scry2 names PREFIX` to inspect what aliases actually
    /// landed in the index — non-obvious failures (trailing
    /// whitespace, unicode normalisation, missing aliases) are then
    /// obvious from the dump.
    pub fn names_with_prefix(&self, prefix: &str, limit: usize) -> Vec<(String, u64)> {
        let names = self.names_slice();
        let n = self.hdr.names_n as usize;
        let pb = prefix.as_bytes();
        // Binary search for the first row whose name >= prefix.
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let row_off = mid * NAME_LEN;
            let off = u64::from_be_bytes(names[row_off..row_off + 8].try_into().unwrap());
            let len = u16::from_be_bytes(names[row_off + 8..row_off + 10].try_into().unwrap());
            let row = self.blob_str(off, len);
            if row.as_bytes() < pb { lo = mid + 1; } else { hi = mid; }
        }
        let mut out = Vec::with_capacity(limit.min(64));
        let mut i = lo;
        while i < n && out.len() < limit {
            let row_off = i * NAME_LEN;
            let off = u64::from_be_bytes(names[row_off..row_off + 8].try_into().unwrap());
            let len = u16::from_be_bytes(names[row_off + 8..row_off + 10].try_into().unwrap());
            let row = self.blob_str(off, len);
            if !row.as_bytes().starts_with(pb) { break; }
            let sym = u64::from_be_bytes(names[row_off + 10..row_off + 18].try_into().unwrap());
            out.push((row.to_string(), sym));
            i += 1;
        }
        out
    }

    /// Read the name-row at `row_id` (an index into the alpha-sorted names
    /// table) as `(blob_slice, sym)`. Row layout: name_off u64 BE,
    /// name_len u16 BE, sym u64 BE. Used by the trigram path to verify a
    /// candidate and recover its sym.
    fn name_row<'a>(&self, names: &[u8], blob: &'a [u8], row_id: u32) -> (&'a [u8], u64) {
        let row_off = row_id as usize * NAME_LEN;
        let off = u64::from_be_bytes(names[row_off..row_off + 8].try_into().unwrap()) as usize;
        let len = u16::from_be_bytes(names[row_off + 8..row_off + 10].try_into().unwrap()) as usize;
        let sym = u64::from_be_bytes(names[row_off + 10..row_off + 18].try_into().unwrap());
        (&blob[off..off + len], sym)
    }

    /// Look up `tri` in the trigram dictionary (sorted ascending by the
    /// 3-byte key), returning its `Posting` descriptor (region offset/len +
    /// posting count), or None if absent. Binary search over the 3-byte key
    /// only — it decodes NOTHING from the postings blob, so the caller can
    /// compare list sizes (via `count`) before deciding what to decode.
    fn trigram_lookup(&self, dict: &[u8], tri: [u8; 3]) -> Option<Posting> {
        let n = self.hdr.trigram_dict_n as usize;
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let row = mid * TRIGRAM_LEN;
            let key = [dict[row], dict[row + 1], dict[row + 2]];
            match key.cmp(&tri) {
                std::cmp::Ordering::Less    => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal   => {
                    // Layout: trigram[3] + _pad[1] + post_off u64 BE +
                    // post_len u32 BE + count u32 BE  (block-skip).
                    let post_off = u64::from_be_bytes(dict[row + 4..row + 12].try_into().unwrap()) as usize;
                    let post_len = u32::from_be_bytes(dict[row + 12..row + 16].try_into().unwrap()) as usize;
                    let count    = u32::from_be_bytes(dict[row + 16..row + 20].try_into().unwrap()) as usize;
                    return Some(Posting { off: post_off, len: post_len, count });
                }
            }
        }
        None
    }

    /// Fully decode a trigram's block-skip posting region into ascending
    /// name-row-ids. Used only for the DRIVER list (the smallest needle
    /// trigram) — every other list is probed without a full decode. The
    /// region is `[skip-table][packed blocks]`: skip the `n_blocks`-entry
    /// skip-table, then walk the blocks. Each block's first id is stored
    /// as-is (the gap base resets per block); later ids add their gap. This
    /// reproduces the exact strictly-ascending id sequence the builder fed.
    fn trigram_postings(&self, post: &[u8], p: Posting) -> Vec<u32> {
        let mut out = Vec::with_capacity(p.count);
        let n_blocks = p.count.div_ceil(TRIGRAM_BLOCK);
        let blocks_start = p.off + n_blocks * SKIP_ENTRY;
        let region_end = p.off + p.len;
        let mut pos = blocks_start;
        let mut remaining = p.count;
        while remaining > 0 {
            let this = remaining.min(TRIGRAM_BLOCK);
            let mut acc: u32 = 0;
            for i in 0..this {
                let (delta, used) = read_varint(post, pos);
                pos += used;
                acc = if i == 0 { delta } else { acc + delta };
                out.push(acc);
            }
            remaining -= this;
        }
        debug_assert!(pos <= region_end);
        out
    }

    /// Test whether `cand` is present in a trigram's posting list WITHOUT
    /// decoding the whole list. Binary-searches the skip-table for the first
    /// block whose `max_id >= cand` (galloping is done by the caller via
    /// `hint`, which advances a forward cursor so a run of ascending
    /// candidates never re-scans earlier blocks), then decodes ONLY that
    /// block and scans it for `cand`. Returns `(found, block_idx)` so the
    /// caller can keep its cursor at the block that satisfied (or would
    /// satisfy) this candidate; the next, larger candidate resumes the
    /// skip-table search from there. This is what bounds a substring query
    /// to the DRIVER list's size plus a per-candidate O(log n_blocks +
    /// TRIGRAM_BLOCK) probe, instead of the sum of all list sizes.
    fn trigram_contains(&self, post: &[u8], p: Posting, cand: u32, hint: usize) -> (bool, usize) {
        let n_blocks = p.count.div_ceil(TRIGRAM_BLOCK);
        if n_blocks == 0 { return (false, 0); }
        // Find the first block (at index >= hint) whose max_id >= cand.
        // max_id lives at the block's skip entry (8-byte stride): bytes
        // [0..4] are max_id BE, [4..8] are the block's varint offset.
        let max_id_at = |b: usize| -> u32 {
            let e = p.off + b * SKIP_ENTRY;
            u32::from_be_bytes(post[e..e + 4].try_into().unwrap())
        };
        // Galloping search from `hint` for the first block with max_id >= cand.
        let mut lo = hint.min(n_blocks);
        // Exponential gallop to bracket the target, then binary search.
        if lo < n_blocks && max_id_at(lo) < cand {
            let mut step = 1usize;
            let mut hi = lo + 1;
            while hi < n_blocks && max_id_at(hi) < cand {
                lo = hi;
                step *= 2;
                hi = lo.saturating_add(step);
            }
            let hi = hi.min(n_blocks);
            // Binary search within (lo, hi] for first block with max_id >= cand.
            let (mut a, mut b) = (lo + 1, hi);
            while a < b {
                let mid = (a + b) / 2;
                if max_id_at(mid) < cand { a = mid + 1; } else { b = mid; }
            }
            lo = a;
        }
        if lo >= n_blocks { return (false, n_blocks); }
        // `lo` is the first candidate block. If its max_id < cand we've run
        // off the end (handled above). Decode this block and scan for cand.
        let block_idx = lo;
        let max_id = max_id_at(block_idx);
        if max_id < cand { return (false, n_blocks); }
        // Block byte range: from this block's offset to the next block's
        // offset (or the region end for the last block).
        let block_off = {
            let e = p.off + block_idx * SKIP_ENTRY;
            u32::from_be_bytes(post[e + 4..e + 8].try_into().unwrap()) as usize
        };
        let block_start = p.off + block_off;
        let block_end = if block_idx + 1 < n_blocks {
            let e = p.off + (block_idx + 1) * SKIP_ENTRY;
            p.off + u32::from_be_bytes(post[e + 4..e + 8].try_into().unwrap()) as usize
        } else {
            p.off + p.len
        };
        // Walk the block's gap-deltas (first id stored as-is) until we meet
        // or pass cand. Ascending, so a value > cand means cand is absent.
        let mut pos = block_start;
        let mut acc: u32 = 0;
        let mut first = true;
        while pos < block_end {
            let (delta, used) = read_varint(post, pos);
            pos += used;
            acc = if first { delta } else { acc + delta };
            first = false;
            if acc == cand { return (true, block_idx); }
            if acc > cand { return (false, block_idx); }
        }
        (false, block_idx)
    }

    /// Return all syms whose qualified name contains `needle` as a
    /// CASE-SENSITIVE substring, up to `limit`. The default `--substr`
    /// path — see `syms_matching_substring_impl` for the mechanism. For an
    /// opt-in case-insensitive search use `syms_matching_substring_ci`.
    pub fn syms_matching_substring(&self, needle: &str, limit: usize) -> Vec<u64> {
        self.syms_matching_substring_impl(needle, limit, /*ignore_case=*/false)
    }

    /// Return all syms whose qualified name contains `needle` as a
    /// CASE-INSENSITIVE (ASCII-folded) substring, up to `limit`. The opt-in
    /// `--substr --ignore-case` path. Runs at the same trigram speed as the
    /// case-sensitive default — the index is a case-folded candidate filter
    /// either way; only the per-candidate verify differs.
    pub fn syms_matching_substring_ci(&self, needle: &str, limit: usize) -> Vec<u64> {
        self.syms_matching_substring_impl(needle, limit, /*ignore_case=*/true)
    }

    /// Shared substring engine for the case-sensitive default and the
    /// opt-in case-insensitive search. The trigram index is a CASE-FOLDED
    /// candidate filter (it was built from each name's lowercased bytes);
    /// `ignore_case` decides only how each surviving candidate is VERIFIED.
    ///
    /// Fast path (the common case): when the trigram index is present and
    /// `needle` has at least 3 bytes, intersect the posting lists of the
    /// needle's distinct LOWERCASED trigrams (the dict is lowercased). The
    /// intersection is a NECESSARY but not sufficient condition — a name
    /// can hold all of the needle's trigrams without holding the needle
    /// contiguously (e.g. "abcZZbcd" has both "abc" and "bcd" but not
    /// "abcd") — so each surviving candidate is verified with a real
    /// substring check before its sym is collected: case-sensitive on the
    /// RAW bytes when `!ignore_case`, case-folded when `ignore_case`. This
    /// turns a many-million-row linear scan into a few small list
    /// intersections plus a handful of verifications.
    ///
    /// Fallback path: a needle shorter than 3 bytes has no trigram, and an
    /// index with an empty names table has no dict — in both cases we keep
    /// the parallel linear scan, so behaviour is never worse than before.
    ///
    /// Result semantics match the prior linear scan: the same set of syms
    /// (deduped), capped at `limit`. Order may differ — the trigram path
    /// visits candidates in name-row-id order of the smallest posting
    /// list, not whole-table order — which the callers already tolerate.
    fn syms_matching_substring_impl(&self, needle: &str, limit: usize, ignore_case: bool) -> Vec<u64> {
        let n = self.hdr.names_n as usize;
        let nb = needle.as_bytes();
        if nb.is_empty() || n == 0 || limit == 0 { return Vec::new(); }

        // Trigram fast path: 3+ byte needle AND a non-empty dict.
        if nb.len() >= 3 && self.hdr.trigram_dict_n > 0 {
            return self.syms_matching_substring_trigram(needle, limit, ignore_case);
        }
        self.syms_matching_substring_linear(nb, limit, ignore_case)
    }

    /// Trigram-accelerated substring search via SKIP-LIST + GALLOPING
    /// INTERSECTION. See `syms_matching_substring_impl` for the contract.
    ///
    /// The win over a full-decode intersection: cost is bounded by the
    /// SMALLEST (most selective) needle trigram, not the sum of all lists.
    /// On AOSP the canonical `kythe:...` VName prefix makes common trigrams
    /// near-universal (millions of postings each); decoding every needle
    /// trigram's whole list would dominate. Here we:
    ///   1. Look up each distinct needle trigram's `Posting` descriptor —
    ///      offset/len/count — WITHOUT decoding any postings. The smallest
    ///      `count` becomes the DRIVER.
    ///   2. Fully decode only the driver (small). For each driver candidate
    ///      in ascending order, probe every OTHER trigram for membership via
    ///      its skip-table (`trigram_contains`): galloping/binary search to
    ///      the one block that could hold the candidate, decode just that
    ///      block. A per-other-list forward cursor (the block index the last
    ///      probe stopped at) means ascending candidates never re-scan
    ///      earlier blocks.
    ///   3. A candidate present in ALL lists is then VERIFIED (the trigram
    ///      intersection is necessary-not-sufficient — "abcZZbcd" holds
    ///      "abc" and "bcd" but not "abcd") and its sym collected.
    fn syms_matching_substring_trigram(&self, needle: &str, limit: usize, ignore_case: bool) -> Vec<u64> {
        let names = self.names_slice();
        let blob = self.blob();
        let dict = self.trigram_dict_slice();
        let post = self.trigram_post_slice();

        // Distinct trigrams of the needle, ALWAYS lowercased to match the
        // case-folded build side (the dict holds lowercased trigrams). A
        // trigram absent from the dict means NO name contains it (in any
        // case), so no name can contain the needle → empty result.
        let lower_needle: Vec<u8> = needle.bytes().map(|b| b.to_ascii_lowercase()).collect();
        let mut tris: Vec<[u8; 3]> = lower_needle.windows(3)
            .map(|w| [w[0], w[1], w[2]]).collect();
        tris.sort_unstable();
        tris.dedup();

        // Resolve each trigram to its descriptor (no decode). Bail to empty
        // the moment any trigram is missing (necessary condition fails for
        // every name).
        let mut posts: Vec<Posting> = Vec::with_capacity(tris.len());
        for tri in tris {
            match self.trigram_lookup(dict, tri) {
                Some(p) => posts.push(p),
                None => return Vec::new(),
            }
        }
        if posts.is_empty() {
            // Shouldn't happen (needle.len() >= 3 yields >= 1 window), but
            // be safe rather than index an empty slice below.
            return self.syms_matching_substring_linear(needle.as_bytes(), limit, ignore_case);
        }

        // Driver = the fewest-postings list, so we decode the least and
        // probe the fewest candidates. The rest are membership-tested via
        // their skip-tables only.
        let driver_idx = posts.iter().enumerate()
            .min_by_key(|(_, p)| p.count).map(|(i, _)| i).unwrap();
        let driver = posts[driver_idx];
        let driver_ids = self.trigram_postings(post, driver);

        // The "other" lists, each with a forward block cursor that only
        // advances as candidates ascend (galloping resumes from it).
        let others: Vec<Posting> = posts.iter().enumerate()
            .filter(|(i, _)| *i != driver_idx).map(|(_, p)| *p).collect();
        let mut cursors: Vec<usize> = vec![0; others.len()];

        // Substring verify: trigram intersection is necessary-not-sufficient
        // (a name can hold every needle trigram without the contiguous
        // needle), so confirm each candidate actually contains it. The
        // verify decides case: case-sensitive search of the RAW needle in
        // the RAW name when `!ignore_case`; case-folded search of the
        // lowercased needle in the lowercased candidate when `ignore_case`.
        let verify_needle: &[u8] = if ignore_case { &lower_needle } else { needle.as_bytes() };
        let nfind = memchr::memmem::Finder::new(verify_needle);
        let mut out = Vec::with_capacity(limit.min(64));
        let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for &cand in &driver_ids {
            // Candidate must appear in every OTHER list. Probe each via its
            // skip-table, advancing that list's forward cursor. Short-circuit
            // on the first miss.
            let mut in_all = true;
            for (k, op) in others.iter().enumerate() {
                let (found, block_idx) = self.trigram_contains(post, *op, cand, cursors[k]);
                cursors[k] = block_idx; // resume here for the next, larger cand
                if !found { in_all = false; break; }
            }
            if !in_all { continue; }
            let (name, sym) = self.name_row(names, blob, cand);
            let hit = if ignore_case {
                let lower_name: Vec<u8> = name.iter().map(|b| b.to_ascii_lowercase()).collect();
                nfind.find(&lower_name).is_some()
            } else {
                nfind.find(name).is_some()
            };
            if hit && seen.insert(sym) {
                out.push(sym);
                if out.len() >= limit { break; }
            }
        }
        out
    }

    /// Parallel linear scan over the whole name table — the fallback when
    /// the needle is shorter than a trigram or no trigram index exists.
    /// The verify mirrors the trigram path: case-sensitive on the raw bytes
    /// when `!ignore_case`, case-folded (needle + candidate ASCII-lowercased)
    /// when `ignore_case`. Splits the table across cores; each thread keeps
    /// the first `limit` matches in its contiguous range and the parts are
    /// concatenated in row order then truncated, so the result is the
    /// first-`limit` set a serial scan would return. The caller's
    /// per-call cap (`limit`) bounds the work that survives into the
    /// `ref`/`callers --substr` aggregation.
    fn syms_matching_substring_linear(&self, nb: &[u8], limit: usize, ignore_case: bool) -> Vec<u64> {
        let n = self.hdr.names_n as usize;
        if nb.is_empty() || n == 0 || limit == 0 { return Vec::new(); }
        let names = self.names_slice();
        let blob = self.blob();
        // The finder holds the needle as searched: lowercased when folding
        // (the name is lowercased per-row below), raw otherwise.
        let needle: Vec<u8> = if ignore_case {
            nb.iter().map(|b| b.to_ascii_lowercase()).collect()
        } else {
            nb.to_vec()
        };
        let needle = needle.as_slice();
        let threads = std::thread::available_parallelism()
            .map(|p| p.get()).unwrap_or(1).clamp(1, n);
        let chunk = n.div_ceil(threads);
        let parts: Vec<Vec<u64>> = std::thread::scope(|s| {
            let mut handles = Vec::new();
            for t in 0..threads {
                let lo = t * chunk;
                let hi = ((t + 1) * chunk).min(n);
                if lo >= hi { continue; }
                handles.push(s.spawn(move || {
                    let finder = memchr::memmem::Finder::new(needle);
                    let mut local = Vec::new();
                    for i in lo..hi {
                        let row_off = i * NAME_LEN;
                        let off = u64::from_be_bytes(names[row_off..row_off + 8].try_into().unwrap()) as usize;
                        let len = u16::from_be_bytes(names[row_off + 8..row_off + 10].try_into().unwrap()) as usize;
                        let row = &blob[off..off + len];
                        let hit = if ignore_case {
                            let lower: Vec<u8> = row.iter().map(|b| b.to_ascii_lowercase()).collect();
                            finder.find(&lower).is_some()
                        } else {
                            finder.find(row).is_some()
                        };
                        if hit {
                            local.push(u64::from_be_bytes(names[row_off + 10..row_off + 18].try_into().unwrap()));
                            if local.len() >= limit { break; }
                        }
                    }
                    local
                }));
            }
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut out = Vec::with_capacity(limit.min(64));
        let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for part in parts {
            for sym in part {
                if seen.insert(sym) {
                    out.push(sym);
                    if out.len() >= limit { return out; }
                }
            }
        }
        out
    }

    // -- sym → meta ----------------------------------------------------------

    pub fn sym_meta(&self, sym: u64) -> Option<(&str, u8, u8)> {
        let syms = self.syms_slice();
        let n = self.hdr.syms_n as usize;
        let sym_be = sym.to_be_bytes();
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let row_off = mid * SYM_LEN;
            let row_sym: [u8; 8] = syms[row_off..row_off + 8].try_into().unwrap();
            match row_sym.cmp(&sym_be) {
                std::cmp::Ordering::Less    => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal   => {
                    let kind = syms[row_off + 8];
                    let lang = syms[row_off + 9];
                    let name_off = u64::from_be_bytes(syms[row_off + 10..row_off + 18].try_into().unwrap());
                    let name_len = u16::from_be_bytes(syms[row_off + 18..row_off + 20].try_into().unwrap());
                    return Some((self.blob_str(name_off, name_len), kind, lang));
                }
            }
        }
        None
    }

    // -- sym → resolved type -------------------------------------------------

    /// The resolved type of `sym`, rendered to a string at ingest
    /// (`/kythe/edge/typed` → rendered type node). O(log n) binary search
    /// over the sym-sorted `typed` section. Returns None when the sym has
    /// no typed edge or its type couldn't be rendered — never a guess.
    pub fn type_of(&self, sym: u64) -> Option<&str> {
        let typed = self.typed_slice();
        let n = self.hdr.typed_n as usize;
        let sym_be = sym.to_be_bytes();
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let row_off = mid * TYPE_LEN;
            let row_sym: [u8; 8] = typed[row_off..row_off + 8].try_into().unwrap();
            match row_sym.cmp(&sym_be) {
                std::cmp::Ordering::Less    => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal   => {
                    let str_off = u64::from_be_bytes(typed[row_off + 8..row_off + 16].try_into().unwrap());
                    let str_len = u16::from_be_bytes(typed[row_off + 16..row_off + 18].try_into().unwrap());
                    return Some(self.blob_str(str_off, str_len));
                }
            }
        }
        None
    }

    // -- xref iteration ------------------------------------------------------

    /// Iterate xref rows in `[(sym, role_lo, 0, 0), (sym, role_hi+1, 0, 0))`.
    /// `role_lo > role_hi` returns no rows; `role_hi = u8::MAX` means
    /// "all roles for this sym".
    pub fn xrefs(&self, sym: u64, role_lo: u8, role_hi: u8) -> XrefIter<'_> {
        let xrefs = self.xrefs_slice();
        let total = self.hdr.xrefs_n as usize;
        let sym_be = sym.to_be_bytes();
        let mut start_key = [0u8; XREF_LEN];
        start_key[0..8].copy_from_slice(&sym_be);
        start_key[8] = role_lo;
        let mut end_key = [0u8; XREF_LEN];
        end_key[0..8].copy_from_slice(&sym_be);
        end_key[8] = role_hi.saturating_add(1);
        // If role_hi==u8::MAX we want to include all roles up to and
        // including 255 — that becomes "next sym" automatically because
        // role_hi + 1 wraps. We special-case it: bump sym.
        if role_hi == u8::MAX {
            end_key[0..8].copy_from_slice(&(sym.wrapping_add(1)).to_be_bytes());
            end_key[8] = 0;
        }
        let lo = lower_bound(xrefs, total, XREF_LEN, &start_key);
        let hi = lower_bound(xrefs, total, XREF_LEN, &end_key);
        XrefIter { xrefs, idx: lo, end: hi }
    }

    /// File path of the first DECL/DEF xref for `sym`, if any. Used
    /// by path filters that ask "where is X defined?" without forcing
    /// the caller to iterate xrefs manually.
    pub fn sym_def_path(&self, sym: u64) -> Option<&str> {
        for (_, _, file, _) in self.xrefs(sym, role::DECL, role::DEF) {
            if let Some(p) = self.file_path(file) { return Some(p); }
        }
        None
    }

    /// The definition location of `sym` as `(file_id, offset)`, preferring
    /// a DEF row over a DECL row (so a real definition wins over a forward
    /// declaration). Returns None when the sym has neither. Used by the
    /// inheritance verbs to give every related sym a concrete `path@off`
    /// locator instead of a bare ticket. The DECL/DEF range is small and
    /// already binary-searched, so scanning it to pick the DEF is cheap.
    pub fn sym_def_loc(&self, sym: u64) -> Option<(u32, u32)> {
        let mut decl: Option<(u32, u32)> = None;
        for (_, r, file, off) in self.xrefs(sym, role::DECL, role::DEF) {
            if r == role::DEF { return Some((file, off)); }
            if r == role::DECL && decl.is_none() { decl = Some((file, off)); }
        }
        decl
    }

    // -- inheritance ---------------------------------------------------------

    /// `inherits(child)` returns each parent. (super)
    pub fn inherits_of(&self, child: u64) -> Vec<u64> {
        let inh = self.inh_slice();
        let total = self.hdr.inh_n as usize;
        let child_be = child.to_be_bytes();
        let mut start = [0u8; INH_LEN];
        start[0..8].copy_from_slice(&child_be);
        let mut end = [0u8; INH_LEN];
        end[0..8].copy_from_slice(&(child.wrapping_add(1)).to_be_bytes());
        let lo = lower_bound(inh, total, INH_LEN, &start);
        let hi = lower_bound(inh, total, INH_LEN, &end);
        let mut out = Vec::with_capacity(hi - lo);
        for i in lo..hi {
            let off = i * INH_LEN;
            let p: [u8; 8] = inh[off + 8..off + 16].try_into().unwrap();
            out.push(u64::from_be_bytes(p));
        }
        out
    }

    /// `inherited_by(parent)` returns each child. (sub)
    /// O(log n) — binary search the parent-sorted `inhrev` section (the
    /// same edges as `inh`, reversed to (parent, child)), then walk the
    /// contiguous range. Mirrors `called_by` over `crev`.
    pub fn inherited_by(&self, parent: u64) -> Vec<u64> {
        // O(log n) via the by-parent `inhrev` table (every index is v4 here;
        // dev mode, no v3 fallback).
        let inhrev = self.inhrev_slice();
        let n = self.hdr.inhrev_n as usize;
        let p_be = parent.to_be_bytes();
        let mut start = [0u8; INH_LEN];
        start[0..8].copy_from_slice(&p_be);
        let mut end = [0u8; INH_LEN];
        end[0..8].copy_from_slice(&(parent.wrapping_add(1)).to_be_bytes());
        let lo = lower_bound(inhrev, n, INH_LEN, &start);
        let hi = lower_bound(inhrev, n, INH_LEN, &end);
        let mut out = Vec::with_capacity(hi - lo);
        for i in lo..hi {
            let off = i * INH_LEN;
            // inhrev layout: (parent u64 BE, child u64 BE)
            let c: [u8; 8] = inhrev[off + 8..off + 16].try_into().unwrap();
            out.push(u64::from_be_bytes(c));
        }
        out
    }

    // -- membership ----------------------------------------------------------

    /// `members(parent)` returns each direct child sym recorded over
    /// `/kythe/edge/childof` (a class's fields and methods, a package's
    /// types). O(log n) — binary search the parent-sorted `childrev`
    /// section. The caller is responsible for the parent-kind filter
    /// (the `members` verb only expands a type/record/interface/package),
    /// so function-local children (params/locals) never surface even
    /// though they live in `childrev`.
    pub fn members(&self, parent: u64) -> Vec<u64> {
        let childrev = self.childrev_slice();
        let n = self.hdr.childrev_n as usize;
        let p_be = parent.to_be_bytes();
        let mut start = [0u8; INH_LEN];
        start[0..8].copy_from_slice(&p_be);
        let mut end = [0u8; INH_LEN];
        end[0..8].copy_from_slice(&(parent.wrapping_add(1)).to_be_bytes());
        let lo = lower_bound(childrev, n, INH_LEN, &start);
        let hi = lower_bound(childrev, n, INH_LEN, &end);
        let mut out = Vec::with_capacity(hi - lo);
        for i in lo..hi {
            let off = i * INH_LEN;
            // childrev layout: (parent u64 BE, child u64 BE)
            let c: [u8; 8] = childrev[off + 8..off + 16].try_into().unwrap();
            out.push(u64::from_be_bytes(c));
        }
        out
    }

    // -- sym → signature -----------------------------------------------------

    /// The full rendered signature of `sym` with parameter names (e.g.
    /// "void setEnabled(bool enabled)"), or None when none was rendered
    /// (not a function, no param info, unrenderable types). O(log n)
    /// binary search over the sym-sorted `sig` section. Never a guess.
    pub fn sig_of(&self, sym: u64) -> Option<&str> {
        let sig = self.sig_slice();
        let n = self.hdr.sig_n as usize;
        let sym_be = sym.to_be_bytes();
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let row_off = mid * TYPE_LEN;
            let row_sym: [u8; 8] = sig[row_off..row_off + 8].try_into().unwrap();
            match row_sym.cmp(&sym_be) {
                std::cmp::Ordering::Less    => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal   => {
                    let str_off = u64::from_be_bytes(sig[row_off + 8..row_off + 16].try_into().unwrap());
                    let str_len = u16::from_be_bytes(sig[row_off + 16..row_off + 18].try_into().unwrap());
                    return Some(self.blob_str(str_off, str_len));
                }
            }
        }
        None
    }

    // -- callgraph -----------------------------------------------------------

    /// Direct callees of `caller`. O(log n) — binary search the calls
    /// table by caller, then walk forward.
    pub fn calls_from(&self, caller: u64) -> Vec<(u64, u8)> {
        let calls = self.calls_slice();
        let n = self.hdr.calls_n as usize;
        let c_be = caller.to_be_bytes();
        let mut start = [0u8; CALL_LEN];
        start[0..8].copy_from_slice(&c_be);
        let mut end = [0u8; CALL_LEN];
        end[0..8].copy_from_slice(&(caller.wrapping_add(1)).to_be_bytes());
        let lo = lower_bound(calls, n, CALL_LEN, &start);
        let hi = lower_bound(calls, n, CALL_LEN, &end);
        let mut out = Vec::with_capacity(hi - lo);
        for i in lo..hi {
            let off = i * CALL_LEN;
            let callee = u64::from_be_bytes(calls[off + 8..off + 16].try_into().unwrap());
            let role   = calls[off + 16];
            out.push((callee, role));
        }
        out
    }

    /// Direct callers of `callee`. O(log n) — binary search the
    /// callee-sorted `crev` table.
    pub fn called_by(&self, callee: u64) -> Vec<(u64, u8)> {
        let crev = self.crev_slice();
        let n = self.hdr.crev_n as usize;
        let c_be = callee.to_be_bytes();
        let mut start = [0u8; CALL_LEN];
        start[0..8].copy_from_slice(&c_be);
        let mut end = [0u8; CALL_LEN];
        end[0..8].copy_from_slice(&(callee.wrapping_add(1)).to_be_bytes());
        let lo = lower_bound(crev, n, CALL_LEN, &start);
        let hi = lower_bound(crev, n, CALL_LEN, &end);
        let mut out = Vec::with_capacity(hi - lo);
        for i in lo..hi {
            let off = i * CALL_LEN;
            // crev layout: (callee u64 BE, caller u64 BE, role u8)
            let caller = u64::from_be_bytes(crev[off + 8..off + 16].try_into().unwrap());
            let role   = crev[off + 16];
            out.push((caller, role));
        }
        out
    }

    // -- file lookup ---------------------------------------------------------

    pub fn file_path(&self, file: u32) -> Option<&str> {
        let files = self.files_slice();
        let n = self.hdr.files_n as usize;
        let f_be = file.to_be_bytes();
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let row_off = mid * FILE_LEN;
            let row_file: [u8; 4] = files[row_off..row_off + 4].try_into().unwrap();
            match row_file.cmp(&f_be) {
                std::cmp::Ordering::Less    => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal   => {
                    let off = u64::from_be_bytes(files[row_off + 4..row_off + 12].try_into().unwrap());
                    let len = u16::from_be_bytes(files[row_off + 12..row_off + 14].try_into().unwrap());
                    return Some(self.blob_str(off, len));
                }
            }
        }
        None
    }

    // -- whole-table iterators ----------------------------------------------
    //
    // Used by `IndexBuilder::load_from_index` to replay a partial
    // snapshot into a fresh builder when --resume picks up a killed
    // from-kzip run. No mutation contracts — iteration is read-only
    // over the mapped pages.

    pub fn iter_xrefs(&self) -> impl Iterator<Item = (u64, u8, u32, u32)> + '_ {
        let n = self.hdr.xrefs_n as usize;
        let xrefs = self.xrefs_slice();
        (0..n).map(move |i| {
            let off = i * XREF_LEN;
            let sym  = u64::from_be_bytes(xrefs[off..off + 8].try_into().unwrap());
            let role = xrefs[off + 8];
            let file = u32::from_be_bytes(xrefs[off + 9..off + 13].try_into().unwrap());
            let xoff = u32::from_be_bytes(xrefs[off + 13..off + 17].try_into().unwrap());
            (sym, role, file, xoff)
        })
    }

    pub fn iter_syms(&self) -> impl Iterator<Item = (u64, u8, u8, &str)> + '_ {
        let n = self.hdr.syms_n as usize;
        let syms = self.syms_slice();
        let blob = self.blob();
        (0..n).map(move |i| {
            let off = i * SYM_LEN;
            let sym  = u64::from_be_bytes(syms[off..off + 8].try_into().unwrap());
            let kind = syms[off + 8];
            let lang = syms[off + 9];
            let no = u64::from_be_bytes(syms[off + 10..off + 18].try_into().unwrap()) as usize;
            let nl = u16::from_be_bytes(syms[off + 18..off + 20].try_into().unwrap()) as usize;
            let name = std::str::from_utf8(&blob[no..no + nl]).unwrap_or("");
            (sym, kind, lang, name)
        })
    }

    pub fn iter_files(&self) -> impl Iterator<Item = (u32, &str)> + '_ {
        let n = self.hdr.files_n as usize;
        let files = self.files_slice();
        let blob = self.blob();
        (0..n).map(move |i| {
            let off = i * FILE_LEN;
            let f = u32::from_be_bytes(files[off..off + 4].try_into().unwrap());
            let po = u64::from_be_bytes(files[off + 4..off + 12].try_into().unwrap()) as usize;
            let pl = u16::from_be_bytes(files[off + 12..off + 14].try_into().unwrap()) as usize;
            let p = std::str::from_utf8(&blob[po..po + pl]).unwrap_or("");
            (f, p)
        })
    }

    pub fn iter_inherits(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        let n = self.hdr.inh_n as usize;
        let inh = self.inh_slice();
        (0..n).map(move |i| {
            let off = i * INH_LEN;
            let c = u64::from_be_bytes(inh[off..off + 8].try_into().unwrap());
            let p = u64::from_be_bytes(inh[off + 8..off + 16].try_into().unwrap());
            (c, p)
        })
    }

    pub fn iter_calls(&self) -> impl Iterator<Item = (u64, u64, u8)> + '_ {
        let n = self.hdr.calls_n as usize;
        let calls = self.calls_slice();
        (0..n).map(move |i| {
            let off = i * CALL_LEN;
            let caller = u64::from_be_bytes(calls[off..off + 8].try_into().unwrap());
            let callee = u64::from_be_bytes(calls[off + 8..off + 16].try_into().unwrap());
            let role   = calls[off + 16];
            (caller, callee, role)
        })
    }

    /// Names that are NOT a sym's canonical name — i.e., aliases learned
    /// via `/kythe/edge/named` or MarkedSource. Used by snapshot/resume
    /// to round-trip alias rows back through `IndexBuilder::add_alias`
    /// without doubling the canonical names.
    pub fn iter_aliases(&self) -> impl Iterator<Item = (u64, &str)> + '_ {
        // Build a per-sym canonical (off,len) lookup once.
        let syms = self.syms_slice();
        let n_syms = self.hdr.syms_n as usize;
        let mut canon: std::collections::HashMap<u64, (u64, u16)> =
            std::collections::HashMap::with_capacity(n_syms);
        for i in 0..n_syms {
            let off = i * SYM_LEN;
            let s  = u64::from_be_bytes(syms[off..off + 8].try_into().unwrap());
            let no = u64::from_be_bytes(syms[off + 10..off + 18].try_into().unwrap());
            let nl = u16::from_be_bytes(syms[off + 18..off + 20].try_into().unwrap());
            canon.insert(s, (no, nl));
        }
        let names = self.names_slice();
        let n_names = self.hdr.names_n as usize;
        let blob = self.blob();
        (0..n_names).filter_map(move |i| {
            let off = i * NAME_LEN;
            let no = u64::from_be_bytes(names[off..off + 8].try_into().unwrap());
            let nl = u16::from_be_bytes(names[off + 8..off + 10].try_into().unwrap());
            let sym = u64::from_be_bytes(names[off + 10..off + 18].try_into().unwrap());
            if canon.get(&sym) == Some(&(no, nl)) { return None; }
            let s = std::str::from_utf8(&blob[no as usize..no as usize + nl as usize]).ok()?;
            Some((sym, s))
        })
    }

    /// Every `(sym, type_string)` row in the `typed` section, in sym
    /// order. Used by snapshot/resume to round-trip resolved types and
    /// by the k-way merge to fold typed tables across shards.
    pub fn iter_typed(&self) -> impl Iterator<Item = (u64, &str)> + '_ {
        let n = self.hdr.typed_n as usize;
        let typed = self.typed_slice();
        let blob = self.blob();
        (0..n).map(move |i| {
            let off = i * TYPE_LEN;
            let sym = u64::from_be_bytes(typed[off..off + 8].try_into().unwrap());
            let so  = u64::from_be_bytes(typed[off + 8..off + 16].try_into().unwrap()) as usize;
            let sl  = u16::from_be_bytes(typed[off + 16..off + 18].try_into().unwrap()) as usize;
            let s = std::str::from_utf8(&blob[so..so + sl]).unwrap_or("");
            (sym, s)
        })
    }

    /// Every `(parent, child)` row in the `childrev` section, in
    /// (parent, child) order. Used by snapshot/resume to round-trip
    /// membership edges and by the k-way merge to fold childrev across
    /// shards.
    pub fn iter_childrev(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        let n = self.hdr.childrev_n as usize;
        let childrev = self.childrev_slice();
        (0..n).map(move |i| {
            let off = i * INH_LEN;
            let p = u64::from_be_bytes(childrev[off..off + 8].try_into().unwrap());
            let c = u64::from_be_bytes(childrev[off + 8..off + 16].try_into().unwrap());
            (p, c)
        })
    }

    /// Every `(sym, signature_string)` row in the `sig` section, in sym
    /// order. Used by snapshot/resume to round-trip signatures and by the
    /// k-way merge to fold sig tables across shards.
    pub fn iter_sig(&self) -> impl Iterator<Item = (u64, &str)> + '_ {
        let n = self.hdr.sig_n as usize;
        let sig = self.sig_slice();
        let blob = self.blob();
        (0..n).map(move |i| {
            let off = i * TYPE_LEN;
            let sym = u64::from_be_bytes(sig[off..off + 8].try_into().unwrap());
            let so  = u64::from_be_bytes(sig[off + 8..off + 16].try_into().unwrap()) as usize;
            let sl  = u16::from_be_bytes(sig[off + 16..off + 18].try_into().unwrap()) as usize;
            let s = std::str::from_utf8(&blob[so..so + sl]).unwrap_or("");
            (sym, s)
        })
    }

}

pub struct XrefIter<'a> {
    xrefs: &'a [u8],
    idx:   usize,
    end:   usize,
}

impl<'a> Iterator for XrefIter<'a> {
    type Item = (u64, u8, u32, u32);  // (sym, role, file, offset)
    fn next(&mut self) -> Option<Self::Item> {
        if self.idx >= self.end { return None; }
        let off = self.idx * XREF_LEN;
        let sym: [u8; 8]   = self.xrefs[off..off + 8].try_into().unwrap();
        let role: u8       = self.xrefs[off + 8];
        let file: [u8; 4]  = self.xrefs[off + 9..off + 13].try_into().unwrap();
        let xoff: [u8; 4]  = self.xrefs[off + 13..off + 17].try_into().unwrap();
        self.idx += 1;
        Some((
            u64::from_be_bytes(sym),
            role,
            u32::from_be_bytes(file),
            u32::from_be_bytes(xoff),
        ))
    }
}

fn lower_bound(table: &[u8], n: usize, stride: usize, needle: &[u8]) -> usize {
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let row = &table[mid * stride..(mid + 1) * stride];
        if row < needle { lo = mid + 1; } else { hi = mid; }
    }
    lo
}

