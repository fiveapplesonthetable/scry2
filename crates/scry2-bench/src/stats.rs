//! Latency + IO accounting. "Every ns accounted" means: for each phase
//! we capture wall time, plus delta-page-faults (minor+major) and
//! delta-bytes-read from /proc/self/io. With that we can attribute slow
//! cold reads to actual disk I/O vs. CPU work.

use std::fs;

#[derive(Clone, Copy, Default, Debug)]
pub struct IoSnapshot {
    pub rchar:        u64,  // bytes read into userspace (incl. cache hits)
    pub wchar:        u64,  // bytes written from userspace
    pub read_bytes:   u64,  // bytes actually fetched from disk
    pub write_bytes:  u64,  // bytes actually sent to disk
    pub minor_faults: u64,
    pub major_faults: u64,
}

impl IoSnapshot {
    pub fn now() -> Self {
        let mut s = Self::default();
        if let Ok(text) = fs::read_to_string("/proc/self/io") {
            for line in text.lines() {
                let (k, v) = match line.split_once(": ") {
                    Some(x) => x,
                    None => continue,
                };
                let n: u64 = v.parse().unwrap_or(0);
                match k {
                    "rchar"       => s.rchar = n,
                    "wchar"       => s.wchar = n,
                    "read_bytes"  => s.read_bytes = n,
                    "write_bytes" => s.write_bytes = n,
                    _ => {}
                }
            }
        }
        unsafe {
            let mut ru: libc::rusage = std::mem::zeroed();
            if libc::getrusage(libc::RUSAGE_SELF, &mut ru) == 0 {
                s.minor_faults = ru.ru_minflt as u64;
                s.major_faults = ru.ru_majflt as u64;
            }
        }
        s
    }

    pub fn delta(&self, prev: &IoSnapshot) -> IoSnapshot {
        IoSnapshot {
            rchar:        self.rchar.saturating_sub(prev.rchar),
            wchar:        self.wchar.saturating_sub(prev.wchar),
            read_bytes:   self.read_bytes.saturating_sub(prev.read_bytes),
            write_bytes:  self.write_bytes.saturating_sub(prev.write_bytes),
            minor_faults: self.minor_faults.saturating_sub(prev.minor_faults),
            major_faults: self.major_faults.saturating_sub(prev.major_faults),
        }
    }
}

pub fn fmt_bytes(n: u64) -> String {
    if n >= 1 << 30 { format!("{:.2} GB", n as f64 / (1u64<<30) as f64) }
    else if n >= 1 << 20 { format!("{:.1} MB", n as f64 / (1u64<<20) as f64) }
    else if n >= 1 << 10 { format!("{:.1} KB", n as f64 / 1024.0) }
    else { format!("{} B", n) }
}

pub fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() { return 0; }
    let idx = ((sorted.len() as f64 - 1.0) * p / 100.0).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Per-query latency stream (in nanoseconds). Reports the full distribution.
pub struct LatencyReport<'a> {
    pub label: &'a str,
    pub nanos: Vec<u64>,
    pub n_rows_total: u64,  // sum of result-set sizes — to derive per-row ns
}

impl<'a> LatencyReport<'a> {
    pub fn print(mut self) {
        self.nanos.sort();
        let n = self.nanos.len();
        let p50 = percentile(&self.nanos, 50.0);
        let p90 = percentile(&self.nanos, 90.0);
        let p99 = percentile(&self.nanos, 99.0);
        let p999= percentile(&self.nanos, 99.9);
        let max = *self.nanos.last().unwrap_or(&0);
        let sum: u128 = self.nanos.iter().map(|&x| x as u128).sum();
        let mean = if n > 0 { (sum / n as u128) as u64 } else { 0 };
        let per_row_ns = if self.n_rows_total > 0 { sum / self.n_rows_total as u128 } else { 0 };
        println!(
            "  [{:<14}] n={:<5} mean={:>6}ns p50={:>6}ns p90={:>7}ns p99={:>8}ns p99.9={:>8}ns max={:>9}ns | per-row={}ns | rows={}",
            self.label, n,
            mean, p50, p90, p99, p999, max,
            per_row_ns, self.n_rows_total,
        );
    }
}

pub fn print_io_delta(label: &str, delta: &IoSnapshot, wall_secs: f64) {
    println!(
        "  [{:<14}] wall={:.2}s  disk-read={}  disk-write={}  rchar={}  major-faults={}  minor-faults={}",
        label, wall_secs,
        fmt_bytes(delta.read_bytes),
        fmt_bytes(delta.write_bytes),
        fmt_bytes(delta.rchar),
        delta.major_faults,
        delta.minor_faults,
    );
}
