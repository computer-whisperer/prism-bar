//! System monitor sampling — CPU, memory, disk.
//!
//! All sources are cheap synchronous reads (`/proc/stat`,
//! `/proc/meminfo`, one `statvfs`), so sampling happens inline on the
//! main loop's poll deadline; no thread.

use std::time::{Duration, Instant};

/// How often to resample.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

/// One render-ready reading. Fractions are `0.0..=1.0`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SysStats {
    /// None until two /proc/stat samples exist (CPU load is a delta).
    pub cpu: Option<f32>,
    pub mem: Option<f32>,
    pub disk: Option<f32>,
}

pub struct SysMon {
    /// (busy, total) jiffies from the previous /proc/stat read.
    prev_cpu: Option<(u64, u64)>,
    pub stats: SysStats,
    pub next_sample: Instant,
}

impl SysMon {
    pub fn new() -> Self {
        let mut mon = Self {
            prev_cpu: None,
            stats: SysStats::default(),
            next_sample: Instant::now(),
        };
        mon.sample(); // primes prev_cpu; mem/disk are valid immediately
        mon
    }

    /// Take a fresh reading and arm the next deadline. Returns true if
    /// the rendered stats changed.
    pub fn sample(&mut self) -> bool {
        self.next_sample = Instant::now() + SAMPLE_INTERVAL;
        let new = SysStats {
            cpu: self.sample_cpu(),
            mem: sample_mem(),
            disk: sample_disk(),
        };
        let changed = new != self.stats;
        self.stats = new;
        changed
    }

    fn sample_cpu(&mut self) -> Option<f32> {
        let text = std::fs::read_to_string("/proc/stat").ok()?;
        // "cpu  user nice system idle iowait irq softirq steal ..."
        let fields: Vec<u64> = text
            .lines()
            .next()?
            .split_whitespace()
            .skip(1)
            .filter_map(|f| f.parse().ok())
            .collect();
        if fields.len() < 5 {
            return None;
        }
        let total: u64 = fields.iter().sum();
        let idle = fields[3] + fields[4]; // idle + iowait
        let busy = total - idle;

        let prev = self.prev_cpu.replace((busy, total));
        let (prev_busy, prev_total) = prev?;
        let dt = total.checked_sub(prev_total)?;
        if dt == 0 {
            return None;
        }
        Some((busy.saturating_sub(prev_busy)) as f32 / dt as f32)
    }
}

fn sample_mem() -> Option<f32> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let field = |name: &str| -> Option<u64> {
        text.lines()
            .find(|l| l.starts_with(name))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    let total = field("MemTotal:")?;
    let available = field("MemAvailable:")?;
    if total == 0 {
        return None;
    }
    Some(1.0 - available as f32 / total as f32)
}

fn sample_disk() -> Option<f32> {
    let vfs = rustix::fs::statvfs("/").ok()?;
    if vfs.f_blocks == 0 {
        return None;
    }
    // Match `df`: used fraction excludes the root reserve.
    Some(1.0 - vfs.f_bavail as f32 / vfs.f_blocks as f32)
}
