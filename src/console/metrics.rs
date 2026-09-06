//! What `/api/status` says about the process.
//!
//! The server being replaced samples itself every five seconds -- heap,
//! load, requests served -- and the status page draws the numbers. There is
//! no heap here and no event loop, so the resident set stands in for the
//! one and zero for the other; the requests are counted as they pass and
//! the rest is asked of the operating system when the page asks.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde_json::{Value, json};

/// The counts a request passing through leaves behind.
pub struct Metrics {
    started: Instant,
    requests: AtomicU64,
    disconnects: AtomicU64,
    concurrent: AtomicU64,
    slowest_millis: AtomicU64,
    total_millis: AtomicU64,
    answered: AtomicU64,
    status_codes: Mutex<BTreeMap<u16, u64>>,
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics {
            started: Instant::now(),
            requests: AtomicU64::new(0),
            disconnects: AtomicU64::new(0),
            concurrent: AtomicU64::new(0),
            slowest_millis: AtomicU64::new(0),
            total_millis: AtomicU64::new(0),
            answered: AtomicU64::new(0),
            status_codes: Mutex::new(BTreeMap::new()),
        }
    }
}

impl Metrics {
    /// A request has arrived.
    pub fn arrived(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.concurrent.fetch_add(1, Ordering::Relaxed);
    }

    /// A request has been answered, with this status, after this long.
    pub fn answered(&self, status: u16, millis: u64) {
        self.concurrent.fetch_sub(1, Ordering::Relaxed);
        self.slowest_millis.fetch_max(millis, Ordering::Relaxed);
        self.total_millis.fetch_add(millis, Ordering::Relaxed);
        self.answered.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut codes) = self.status_codes.lock() {
            *codes.entry(status).or_insert(0) += 1;
        }
    }

    /// A caller went away before it was answered.
    pub fn disconnected(&self) {
        self.disconnects.fetch_add(1, Ordering::Relaxed);
    }

    /// The `metrics` object of `/api/status`.
    pub fn report(&self) -> Value {
        let codes: BTreeMap<String, u64> = self
            .status_codes
            .lock()
            .map(|c| c.iter().map(|(k, v)| (k.to_string(), *v)).collect())
            .unwrap_or_default();
        let (load1, load5, load15) = load_average();
        let (memory_total, memory_free) = system_memory();
        let resident = resident_set_size();
        let answered = self.answered.load(Ordering::Relaxed);
        let average = match answered {
            0 => 0,
            n => self.total_millis.load(Ordering::Relaxed) / n,
        };
        json!({
            "last_updated": super::now(),
            "collection_interval_in_millis": 5000,
            "process": {
                "memory": {
                    "heap": {
                        "total_in_bytes": resident,
                        "used_in_bytes": resident,
                        "size_limit": memory_total,
                    },
                    "resident_set_size_in_bytes": resident,
                },
                "event_loop_delay": 0,
                "pid": std::process::id(),
                "uptime_in_millis": self.started.elapsed().as_millis() as u64,
            },
            "os": {
                "load": {"1m": load1, "5m": load5, "15m": load15},
                "memory": {
                    "total_in_bytes": memory_total,
                    "used_in_bytes": memory_total.saturating_sub(memory_free),
                    "free_in_bytes": memory_free,
                },
                "uptime_in_millis": system_uptime_millis(),
                "platform": platform(),
                "platformRelease": platform_release(),
            },
            "response_times": {
                "avg_in_millis": average,
                "max_in_millis": self.slowest_millis.load(Ordering::Relaxed),
            },
            "requests": {
                "total": self.requests.load(Ordering::Relaxed),
                "disconnects": self.disconnects.load(Ordering::Relaxed),
                "statusCodes": codes,
                "status_codes": codes,
            },
            "concurrent_connections": self.concurrent.load(Ordering::Relaxed),
        })
    }
}

/// The name the server being replaced gives the platform: Node's, so
/// `darwin` rather than `macos`.
fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

fn platform_release() -> String {
    // SAFETY: utsname is plain data the call fills in, and is read only
    // after the call said it did
    unsafe {
        let mut name: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut name) != 0 {
            return String::new();
        }
        let release = std::ffi::CStr::from_ptr(name.release.as_ptr());
        format!("{}-{}", platform(), release.to_string_lossy())
    }
}

fn load_average() -> (f64, f64, f64) {
    let mut loads = [0f64; 3];
    // SAFETY: the array is three doubles and the call is told so
    let n = unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) };
    if n == 3 { (loads[0], loads[1], loads[2]) } else { (0.0, 0.0, 0.0) }
}

/// The process's resident set, in bytes.
fn resident_set_size() -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(statm) = std::fs::read_to_string("/proc/self/statm") {
            let pages: u64 =
                statm.split_whitespace().nth(1).and_then(|p| p.parse().ok()).unwrap_or(0);
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
            return pages * page;
        }
    }
    // SAFETY: rusage is plain data the call fills in
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return 0;
    }
    // the high-water mark: bytes on macOS, kilobytes on Linux
    let max = usage.ru_maxrss as u64;
    if cfg!(target_os = "macos") { max } else { max * 1024 }
}

/// Total and free memory, in bytes.
fn system_memory() -> (u64, u64) {
    #[cfg(target_os = "linux")]
    {
        let mut total = 0;
        let mut free = 0;
        if let Ok(info) = std::fs::read_to_string("/proc/meminfo") {
            for line in info.lines() {
                let kb = |l: &str| {
                    l.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)
                        * 1024
                };
                if line.starts_with("MemTotal:") {
                    total = kb(line);
                } else if line.starts_with("MemAvailable:") {
                    free = kb(line);
                }
            }
        }
        (total, free)
    }
    #[cfg(target_os = "macos")]
    {
        (sysctl_u64("hw.memsize"), sysctl_u64("vm.page_free_count") * sysctl_u64("hw.pagesize"))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        (0, 0)
    }
}

#[cfg(target_os = "macos")]
fn sysctl_u64(name: &str) -> u64 {
    let Ok(name) = std::ffi::CString::new(name) else { return 0 };
    let mut value: u64 = 0;
    let mut size = std::mem::size_of::<u64>();
    // SAFETY: the buffer is a u64 and its size is passed with it; a value
    // the kernel gives as 32 bits lands in the low bytes on this
    // little-endian platform
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut value as *mut u64 as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 { value } else { 0 }
}

fn system_uptime_millis() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    #[cfg(target_os = "macos")]
    let clock = libc::CLOCK_UPTIME_RAW;
    #[cfg(not(target_os = "macos"))]
    let clock = libc::CLOCK_BOOTTIME;
    // SAFETY: timespec is plain data the call fills in
    if unsafe { libc::clock_gettime(clock, &mut ts) } != 0 {
        return 0;
    }
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_counted_as_they_pass() {
        let metrics = Metrics::default();
        metrics.arrived();
        metrics.answered(200, 12);
        metrics.arrived();
        metrics.answered(404, 30);
        metrics.arrived();
        let report = metrics.report();
        assert_eq!(report["requests"]["total"], 3);
        assert_eq!(report["concurrent_connections"], 1);
        assert_eq!(report["requests"]["status_codes"]["200"], 1);
        assert_eq!(report["requests"]["status_codes"]["404"], 1);
        assert_eq!(report["response_times"]["max_in_millis"], 30);
        assert_eq!(report["response_times"]["avg_in_millis"], 21);
        assert!(report["process"]["memory"]["heap"]["total_in_bytes"].as_u64().unwrap() > 0);
        assert!(report["os"]["memory"]["total_in_bytes"].as_u64().unwrap() > 0);
        assert!(report["os"]["load"]["1m"].is_number());
        assert!(report["os"]["uptime_in_millis"].as_u64().unwrap() > 0);
    }
}
