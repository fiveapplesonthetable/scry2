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
        // v4 sections (typed/childrev/inhrev/sig) are zero in a v3 file, so
        // their bounds check (0+0 <= map_len) trivially passes — that's what
        // makes a v4 reader open a v3 file.
        let sections: [(&str, u64, u64, u64); 12] = [
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

    fn blob_str(&self, off: u64, len: u16) -> &str {
        let s = &self.blob()[off as usize..off as usize + len as usize];
        std::str::from_utf8(s).unwrap_or("<bad utf8>")
    }

    // -- name → sym ----------------------------------------------------------

    /// Binary search the alphabetical name index for an exact match.
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

    /// Return all syms whose qualified name contains `needle` (case-
    /// sensitive substring). Linear scan over the name index; for 5M
    /// syms × 64 B/name = 320 MB this is 1 SSD pass.
    ///
    /// Uses `memchr::memmem::Finder` for the inner string search: it
    /// precomputes a SIMD-friendly state machine once per call and
    /// is ~10× faster than the naive `windows(needle.len()).position`
    /// pattern on long names, which dominates wall time on AOSP
    /// (~5M names averaging 60 bytes).
    pub fn syms_matching_substring(&self, needle: &str, limit: usize) -> Vec<u64> {
        let n = self.hdr.names_n as usize;
        let nb = needle.as_bytes();
        if nb.is_empty() || n == 0 { return Vec::new(); }
        let names = self.names_slice();
        let blob = self.blob();
        // A substring (not prefix) match has to examine every name — there
        // is no trigram index — and substring hits scatter across the
        // alpha-sorted table, so it can't stop early. Split the table
        // across cores: at 91M names a serial scan is ~30s; parallel it is
        // a couple seconds. Each thread keeps the first `limit` matches in
        // its contiguous row range, and the parts are concatenated in row
        // order then truncated, so the result is exactly the first-`limit`
        // set a serial scan would return.
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
                    let finder = memchr::memmem::Finder::new(nb);
                    let mut local = Vec::new();
                    for i in lo..hi {
                        let row_off = i * NAME_LEN;
                        let off = u64::from_be_bytes(names[row_off..row_off + 8].try_into().unwrap()) as usize;
                        let len = u16::from_be_bytes(names[row_off + 8..row_off + 10].try_into().unwrap()) as usize;
                        if finder.find(&blob[off..off + len]).is_some() {
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
        for part in parts {
            for sym in part {
                out.push(sym);
                if out.len() >= limit { return out; }
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

