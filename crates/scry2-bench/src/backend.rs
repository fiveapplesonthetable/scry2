//! Backend trait + drop-cache helper.

use crate::workload::XKey;
use std::path::PathBuf;
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

/// Drop the kernel page cache. Requires root; otherwise no-op (and the
/// "cold" phase will be misleadingly fast — we print a warning).
pub fn drop_caches() {
    let _ = std::process::Command::new("sync").status();
    match std::fs::write("/proc/sys/vm/drop_caches", "3") {
        Ok(_)  => eprintln!("[cache] dropped page cache (root)"),
        Err(e) => eprintln!("[cache] WARN: could not drop page cache ({}); cold phase is approximate", e),
    }
    std::thread::sleep(std::time::Duration::from_secs(2));
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
