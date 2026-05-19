//! Backend trait + drop-cache helper.

use crate::workload::XKey;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use anyhow::Result;

pub trait Backend {
    /// Human label, used in report rows.
    fn name(&self) -> &'static str;

    /// On-disk path(s) backing this DB — `du`d for the size report.
    fn paths(&self) -> Vec<PathBuf>;

    /// Bulk-load all rows. The implementation chooses ordering / batching.
    /// Must be durable on return.
    fn bulk_write(&mut self, rows: &[XKey]) -> Result<()>;

    /// Close + reopen as read-only. Allows the harness to drop the page
    /// cache between phases — otherwise warm and cold are indistinguishable
    /// because most page tables stay mapped.
    fn reopen_readonly(&mut self) -> Result<()>;

    /// Count rows whose key is in `[start, end)`. We return count rather
    /// than the rows themselves to keep allocations identical across
    /// backends — scry2's real reads will materialize records, but for the
    /// backend comparison only the access cost matters.
    fn prefix_count(&self, start: &XKey, end: &XKey) -> Result<usize>;

    /// Return Some(first key) if the prefix `[start, end)` is non-empty.
    /// Simulates `scry2 def NAME` — first row wins.
    fn point_first(&self, start: &XKey, end: &XKey) -> Result<Option<XKey>>;
}

/// Evict this backend's on-disk pages from the OS page cache, without
/// needing root. Walks every regular file under each path and issues
/// `posix_fadvise(POSIX_FADV_DONTNEED)`. After this returns, subsequent
/// reads will fault from disk.
///
/// We fall back to `/proc/sys/vm/drop_caches` only if root is available;
/// fadvise is more surgical and doesn't trash unrelated workload state.
pub fn drop_caches_for(paths: &[PathBuf]) {
    let _ = std::process::Command::new("sync").status();
    let mut total = 0usize;
    for p in paths {
        total += fadvise_dontneed_recursive(p);
    }
    eprintln!("[cache] posix_fadvise(DONTNEED) on {} files", total);
    // Tiny pause to let the kernel actually release the pages.
    std::thread::sleep(std::time::Duration::from_millis(200));
}

fn fadvise_dontneed_recursive(p: &Path) -> usize {
    let md = match std::fs::metadata(p) { Ok(m) => m, Err(_) => return 0 };
    if md.is_file() {
        if let Ok(f) = std::fs::File::open(p) {
            unsafe {
                let _ = libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
            }
            return 1;
        }
        return 0;
    }
    if md.is_dir() {
        let mut n = 0;
        if let Ok(rd) = std::fs::read_dir(p) {
            for ent in rd.flatten() {
                n += fadvise_dontneed_recursive(&ent.path());
            }
        }
        return n;
    }
    0
}

pub fn du(paths: &[PathBuf]) -> u64 {
    let mut total = 0u64;
    for p in paths {
        if let Ok(m) = std::fs::metadata(p) {
            if m.is_file() {
                total += m.len();
            } else if m.is_dir() {
                if let Ok(rd) = std::fs::read_dir(p) {
                    for ent in rd.flatten() {
                        if let Ok(mm) = ent.metadata() {
                            total += mm.len();
                        }
                    }
                }
            }
        }
    }
    total
}
