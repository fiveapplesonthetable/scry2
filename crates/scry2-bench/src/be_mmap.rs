//! Plain-mmap backend: sorted packed array, binary search. This is what
//! scry's `precision_packed` sidecars look like today, generalised to a
//! 13-byte fixed-width record (no value bytes — the key IS the row).
//!
//! Write path: collect all keys, sort in place, dump. O(n log n) but
//! single contiguous flush.
//! Read path: bisect to find lower-bound, then scan forward while key < end.

use crate::backend::Backend;
use crate::workload::{XKey, KEY_LEN};
use anyhow::{Context, Result};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

pub struct MmapBackend {
    path: PathBuf,
    mmap: Option<Mmap>,
    n_rows: usize,
}

impl MmapBackend {
    pub fn create(path: PathBuf) -> Result<Self> {
        let _ = std::fs::remove_file(&path);
        Ok(Self { path, mmap: None, n_rows: 0 })
    }

    fn keys(&self) -> &[u8] {
        self.mmap.as_ref().map(|m| &m[..]).unwrap_or(&[])
    }

    /// Lower-bound binary search: returns the first index i such that
    /// `&data[i*KEY_LEN..][..KEY_LEN] >= needle`. Uses raw byte compare
    /// because keys are big-endian-packed and the natural memcmp order
    /// matches the (sym_id, role, file_id, offset) sort key we want.
    fn lower_bound(&self, needle: &XKey) -> usize {
        let data = self.keys();
        let mut lo = 0usize;
        let mut hi = self.n_rows;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let off = mid * KEY_LEN;
            if &data[off..off + KEY_LEN] < needle.as_slice() {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

impl Backend for MmapBackend {
    fn name(&self) -> &'static str { "mmap-packed" }
    fn paths(&self) -> Vec<PathBuf> { vec![self.path.clone()] }

    fn bulk_write(&mut self, rows: &[XKey]) -> Result<()> {
        // Sort a working copy. 80M * 13 B = ~1 GB which fits comfortably;
        // for the scry2 production path we'd shard by (sym_id >> N) and
        // sort per shard, but for the backend comparison that's noise.
        let mut sorted: Vec<XKey> = rows.to_vec();
        sorted.sort_unstable();
        // Dedup: redb / rocksdb collapse duplicates implicitly; the mmap
        // path must too, otherwise the file is larger and the bench is
        // unfair.
        sorted.dedup();
        let mut f = File::create(&self.path).context("mmap: create file")?;
        // SAFETY: XKey is repr-trivial [u8; 13], so reinterpreting the
        // slice as a byte slice is sound.
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(sorted.as_ptr() as *const u8, sorted.len() * KEY_LEN)
        };
        f.write_all(bytes).context("mmap: write")?;
        f.sync_all().context("mmap: fsync")?;
        self.n_rows = sorted.len();
        Ok(())
    }

    fn reopen_readonly(&mut self) -> Result<()> {
        self.mmap = None;  // unmap first
        let f = File::open(&self.path).context("mmap: open ro")?;
        let m = unsafe { Mmap::map(&f).context("mmap: map")? };
        let len = m.len();
        if len % KEY_LEN != 0 {
            anyhow::bail!("mmap: file size {} not a multiple of key len {}", len, KEY_LEN);
        }
        self.n_rows = len / KEY_LEN;
        self.mmap = Some(m);
        Ok(())
    }

    fn prefix_count(&self, start: &XKey, end: &XKey) -> Result<usize> {
        let data = self.keys();
        let i = self.lower_bound(start);
        // Linear scan forward until key >= end. For long prefixes this
        // dominates; for short ones (Call subset of one sym), it's a
        // handful of rows.
        let mut j = i;
        while j < self.n_rows {
            let off = j * KEY_LEN;
            if &data[off..off + KEY_LEN] >= end.as_slice() {
                break;
            }
            j += 1;
        }
        Ok(j - i)
    }

    fn point_first(&self, start: &XKey, end: &XKey) -> Result<Option<XKey>> {
        let data = self.keys();
        let i = self.lower_bound(start);
        if i >= self.n_rows { return Ok(None); }
        let off = i * KEY_LEN;
        let row = &data[off..off + KEY_LEN];
        if row < end.as_slice() {
            let mut out = [0u8; 13];
            out.copy_from_slice(row);
            Ok(Some(out))
        } else {
            Ok(None)
        }
    }
}
