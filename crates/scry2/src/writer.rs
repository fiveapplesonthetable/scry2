//! .s2db writer. Accumulates rows in memory, sorts each table, and dumps
//! one page-aligned mmap-ready file.
//!
//! Writes go to a tempfile in the same directory, then atomically
//! rename. A crashed build leaves a `.tmp` behind, never a torn index.

use crate::format::*;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct IndexBuilder {
    xrefs:    Vec<(u64, u8, u32, u32)>,
    syms:     HashMap<u64, (u8, u8, String)>,   // sym → (kind, lang, name)
    files:    HashMap<u32, String>,             // file_id → path
    inherits: Vec<(u64, u64)>,                  // (child, parent)
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

    pub fn n_xrefs(&self) -> usize { self.xrefs.len() }
    pub fn n_syms(&self)  -> usize { self.syms.len() }
    pub fn n_files(&self) -> usize { self.files.len() }
    pub fn n_inh(&self)   -> usize { self.inherits.len() }

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
        // (sym_idx_in_syms_vec, name_off, name_len) sorted by name bytes.
        let mut by_name: Vec<u32> = (0..syms_vec.len() as u32).collect();
        by_name.sort_by(|&a, &b| {
            syms_vec[a as usize].3.as_bytes()
                .cmp(syms_vec[b as usize].3.as_bytes())
        });

        // ---- 4. Sort inherits ----
        self.inherits.sort_unstable();
        self.inherits.dedup();
        let n_inh = self.inherits.len() as u64;

        // ---- 5. Compute section offsets ----
        let xrefs_off = pad_up(size_of_header() as u64);
        let syms_off  = pad_up(xrefs_off + n_xrefs * XREF_LEN as u64);
        let names_off = pad_up(syms_off  + n_syms  * SYM_LEN  as u64);
        let files_off = pad_up(names_off + n_syms  * NAME_LEN as u64);
        let inh_off   = pad_up(files_off + n_files * FILE_LEN as u64);
        let blob_off  = pad_up(inh_off   + n_inh   * INH_LEN  as u64);

        // ---- 6. Write to a tempfile, then atomic rename ----
        let tmp_path: PathBuf = path.with_extension("s2db.tmp");
        let f = File::create(&tmp_path).with_context(|| format!("create {}", tmp_path.display()))?;
        let mut w = BufWriter::with_capacity(8 << 20, f);

        let hdr = Header {
            magic:   MAGIC,
            version: VERSION,
            xrefs_off, xrefs_n: n_xrefs,
            syms_off,  syms_n:  n_syms,
            names_off, names_n: n_syms,
            files_off, files_n: n_files,
            inh_off,   inh_n:   n_inh,
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
        for &idx in &by_name {
            let (off, len) = name_pos[idx as usize];
            let sym = syms_vec[idx as usize].0;
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
