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
        if hdr.version != VERSION {
            bail!("bad version: file={} reader={}", hdr.version, VERSION);
        }
        Ok(Self { _file: file, map, hdr })
    }

    pub fn n_xrefs(&self) -> u64 { self.hdr.xrefs_n }
    pub fn n_syms(&self)  -> u64 { self.hdr.syms_n }
    pub fn n_files(&self) -> u64 { self.hdr.files_n }
    pub fn n_inh(&self)   -> u64 { self.hdr.inh_n }
    pub fn n_calls(&self) -> u64 { self.hdr.calls_n }

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
    fn blob(&self) -> &[u8] {
        let off = self.hdr.blob_off as usize;
        let len = self.hdr.blob_len as usize;
        &self.map[off..off + len]
    }

    fn blob_str(&self, off: u32, len: u16) -> &str {
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
            let name_off = u32::from_be_bytes(names[row_off..row_off + 4].try_into().unwrap());
            let name_len = u16::from_be_bytes(names[row_off + 4..row_off + 6].try_into().unwrap());
            let row_name = self.blob_str(name_off, name_len);
            match row_name.as_bytes().cmp(qb) {
                std::cmp::Ordering::Less    => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal   => {
                    let sym = u64::from_be_bytes(names[row_off + 8..row_off + 16].try_into().unwrap());
                    return Some(sym);
                }
            }
        }
        None
    }

    /// Return all syms whose qualified name contains `needle` (case-
    /// sensitive substring). Linear scan over the name index; for 5M
    /// syms × 64 B/name = 320 MB this is 1 SSD pass, ~few hundred ms cold,
    /// instant warm.
    pub fn syms_matching_substring(&self, needle: &str, limit: usize) -> Vec<u64> {
        let names = self.names_slice();
        let n = self.hdr.names_n as usize;
        let mut out = Vec::with_capacity(limit.min(64));
        let nb = needle.as_bytes();
        for i in 0..n {
            let row_off = i * NAME_LEN;
            let name_off = u32::from_be_bytes(names[row_off..row_off + 4].try_into().unwrap());
            let name_len = u16::from_be_bytes(names[row_off + 4..row_off + 6].try_into().unwrap());
            let row_name = &self.blob()[name_off as usize..name_off as usize + name_len as usize];
            if memmem(row_name, nb).is_some() {
                let sym = u64::from_be_bytes(names[row_off + 8..row_off + 16].try_into().unwrap());
                out.push(sym);
                if out.len() >= limit { break; }
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
                    let name_off = u32::from_be_bytes(syms[row_off + 10..row_off + 14].try_into().unwrap());
                    let name_len = u16::from_be_bytes(syms[row_off + 14..row_off + 16].try_into().unwrap());
                    return Some((self.blob_str(name_off, name_len), kind, lang));
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
    /// We don't keep a reversed table — this is a linear scan over the
    /// inh slice. For interactive queries on ~5M inheritance rows this
    /// is ~100ms cold, ~10ms warm. If it ever needs to be O(log) we
    /// can emit a second sorted-by-parent table.
    pub fn inherited_by(&self, parent: u64) -> Vec<u64> {
        let inh = self.inh_slice();
        let n = self.hdr.inh_n as usize;
        let p_be = parent.to_be_bytes();
        let mut out = Vec::new();
        for i in 0..n {
            let off = i * INH_LEN;
            let row_parent: [u8; 8] = inh[off + 8..off + 16].try_into().unwrap();
            if row_parent == p_be {
                let c: [u8; 8] = inh[off..off + 8].try_into().unwrap();
                out.push(u64::from_be_bytes(c));
            }
        }
        out
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
                    let off = u32::from_be_bytes(files[row_off + 4..row_off + 8].try_into().unwrap());
                    let len = u16::from_be_bytes(files[row_off + 8..row_off + 10].try_into().unwrap());
                    return Some(self.blob_str(off, len));
                }
            }
        }
        None
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

/// Trivial substring search. We don't link memchr to keep the dep list
/// flat; for our query budget this is fine.
fn memmem(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() { return Some(0); }
    if hay.len() < needle.len() { return None; }
    for i in 0..=(hay.len() - needle.len()) {
        if &hay[i..i + needle.len()] == needle { return Some(i); }
    }
    None
}
