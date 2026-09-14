//! What the machine and this process are using, asked of the operating system.
//!
//! `_nodes/stats`, `_cat/nodes`, `_cat/allocation` and `_nodes/hot_threads`
//! report these. They used to be fixed numbers -- a gigabyte of memory half
//! used, two of disk -- which a dashboard drew as if they were measured and an
//! alert could fire on. Everything here is read when it is asked for; the two
//! percentages that need a before and an after keep the last reading.

use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

/// Physical memory, in bytes.
pub struct Memory {
    pub total: u64,
    /// what can be had without swapping: free pages and the ones the kernel
    /// would drop first, which is what an operator means by free
    pub free: u64,
}

/// Swap space, in bytes.
pub struct Swap {
    pub total: u64,
    pub free: u64,
}

/// What this process holds.
pub struct Process {
    pub resident: u64,
    pub virtual_size: u64,
    /// user and system time together, in milliseconds
    pub cpu_millis: u64,
    pub open_fds: u64,
    pub max_fds: u64,
    pub threads: u64,
}

/// The filesystem a path lives on.
pub struct Disk {
    pub path: String,
    pub mount: String,
    pub kind: String,
    pub total: u64,
    pub free: u64,
    pub available: u64,
}

/// What a thread is doing when it is looked at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreadState {
    Running,
    /// asleep until something wakes it: a lock, a condition, a socket
    Waiting,
    /// asleep in the kernel and not to be woken early, which is what a
    /// thread stuck on a disk looks like
    Blocked,
    Other,
}

/// One thread of this process at one moment.
#[derive(Clone)]
pub struct ThreadSample {
    pub id: u64,
    pub name: String,
    /// user and system time the thread has used since it started
    pub cpu_nanos: u64,
    pub state: ThreadState,
    /// the one thing the kernel will say about where the thread is: its run
    /// state, and on Linux the kernel function it sleeps in
    pub detail: String,
}

/// The figures the `_cat` tables show for this node, read once per table.
pub struct Resources {
    /// what stands for the heap: the allocator's own bytes, against the
    /// machine's memory as the most it could have
    pub heap_used: u64,
    pub heap_max: u64,
    pub ram_used: u64,
    pub ram_total: u64,
    pub cpu_percent: u64,
    pub load: Option<[f64; 3]>,
    pub fds: u64,
    pub max_fds: u64,
    pub disk: Option<Disk>,
}

/// This node's figures, its disk being the one `data` lives on.
pub fn resources(data: Option<&Path>) -> Resources {
    let mem = memory();
    let fds = open_fds();
    let here = data
        .map(|d| d.to_path_buf())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("/"));
    Resources {
        heap_used: allocator().0,
        heap_max: mem.total,
        ram_used: mem.total.saturating_sub(mem.free),
        ram_total: mem.total,
        cpu_percent: process_cpu_percent(),
        load: load_average(),
        fds,
        max_fds: max_fds(),
        disk: disk(&here),
    }
}

/// A share as a whole percentage, rounded as the reference rounds it.
pub fn percent(part: u64, whole: u64) -> u64 {
    if whole == 0 { 0 } else { ((part as f64 / whole as f64) * 100.0).round() as u64 }
}

/// How long the node has been running, in milliseconds, from the first time
/// this was asked -- which `main` does before anything else.
pub fn uptime_millis() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// The most threads this process has been seen running at once.
pub fn peak_threads(now: u64) -> u64 {
    static PEAK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    PEAK.fetch_max(now, std::sync::atomic::Ordering::Relaxed).max(now)
}

pub fn memory() -> Memory {
    imp::memory()
}

pub fn swap() -> Swap {
    imp::swap()
}

pub fn process() -> Process {
    let mut p = imp::process();
    p.cpu_millis = cpu_time_millis();
    p.open_fds = open_fds();
    p.max_fds = max_fds();
    p
}

pub fn threads() -> Vec<ThreadSample> {
    imp::threads()
}

/// The one, five and fifteen minute load averages.
pub fn load_average() -> Option<[f64; 3]> {
    let mut v = [0f64; 3];
    let n = unsafe { libc::getloadavg(v.as_mut_ptr(), 3) };
    (n == 3).then_some(v)
}

/// Bytes the allocator holds for this process now, and the most it has held.
///
/// There is no garbage-collected heap here; what the allocator has taken
/// from the system is the nearest thing to one, and it is what the heap
/// figures of a node report. mimalloc's resident figure is the whole
/// process's -- mapped index files and thread stacks with it -- so what it
/// has committed for its own allocations is the one that is the allocator's.
pub fn allocator() -> (u64, u64) {
    let (mut elapsed, mut user, mut sys, mut rss, mut peak_rss, mut commit, mut peak, mut faults) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    unsafe {
        libmimalloc_sys::mi_process_info(
            &mut elapsed,
            &mut user,
            &mut sys,
            &mut rss,
            &mut peak_rss,
            &mut commit,
            &mut peak,
            &mut faults,
        );
    }
    (commit as u64, peak as u64)
}

/// The filesystem holding `path`: its size, and what is left of it.
pub fn disk(path: &Path) -> Option<Disk> {
    let c = std::ffi::CString::new(path.as_os_str().to_string_lossy().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let unit = s.f_frsize as u64;
    let (mount, kind) = imp::mount_of(path);
    Some(Disk {
        path: path.to_string_lossy().into_owned(),
        mount,
        kind,
        total: s.f_blocks as u64 * unit,
        free: s.f_bfree as u64 * unit,
        available: s.f_bavail as u64 * unit,
    })
}

/// How busy every processor of the machine was since the last time this
/// was asked, as a percentage.
pub fn os_cpu_percent() -> u64 {
    static LAST: Mutex<Option<(u64, u64, u64)>> = Mutex::new(None);
    let Some((busy, total)) = imp::cpu_ticks() else { return 0 };
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let percent = match *last {
        Some((b0, t0, kept)) if total > t0 => {
            let p = (busy.saturating_sub(b0) * 100) / (total - t0);
            // a second question straight after the first has too few ticks
            // between them to say anything, so the last answer stands
            if total - t0 < 10 {
                *last = Some((b0, t0, kept));
                return kept;
            }
            p
        }
        Some((_, _, kept)) => return kept,
        None => (busy * 100).checked_div(total).unwrap_or(0),
    };
    *last = Some((busy, total, percent));
    percent
}

/// How much of the machine's processors this process used since the last
/// time this was asked, as a percentage of all of them.
pub fn process_cpu_percent() -> u64 {
    static LAST: Mutex<Option<(Instant, u64, u64)>> = Mutex::new(None);
    let now = Instant::now();
    let used = cpu_time_millis();
    let cpus = std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(1);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let percent = match *last {
        Some((at, _, kept)) if now.duration_since(at).as_millis() < 100 => return kept,
        Some((at, before, _)) => {
            let wall = now.duration_since(at).as_millis() as u64;
            (used.saturating_sub(before) * 100 / (wall * cpus).max(1)).min(100)
        }
        None => 0,
    };
    *last = Some((now, used, percent));
    percent
}

fn cpu_time_millis() -> u64 {
    let mut r: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut r) } != 0 {
        return 0;
    }
    let ms = |t: libc::timeval| t.tv_sec as u64 * 1000 + t.tv_usec as u64 / 1000;
    ms(r.ru_utime) + ms(r.ru_stime)
}

fn max_fds() -> u64 {
    let mut r: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) } != 0 {
        return 0;
    }
    r.rlim_cur as u64
}

/// The descriptors open in this process, less the one reading the list.
fn open_fds() -> u64 {
    let dir = if cfg!(target_os = "linux") { "/proc/self/fd" } else { "/dev/fd" };
    std::fs::read_dir(dir).map(|d| d.count().saturating_sub(1) as u64).unwrap_or(0)
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    unsafe extern "C" {
        fn mach_port_deallocate(task: libc::mach_port_t, name: libc::mach_port_t) -> i32;
    }

    /// The host port, taken once: every call for it hands out another
    /// reference that nothing gives back.
    fn host() -> libc::mach_port_t {
        static HOST: std::sync::OnceLock<libc::mach_port_t> = std::sync::OnceLock::new();
        #[allow(deprecated)]
        *HOST.get_or_init(|| unsafe { libc::mach_host_self() })
    }

    fn sysctl_u64(name: &str) -> Option<u64> {
        let c = std::ffi::CString::new(name).ok()?;
        let mut v = 0u64;
        let mut len = std::mem::size_of::<u64>();
        let rc = unsafe {
            libc::sysctlbyname(
                c.as_ptr(),
                &mut v as *mut u64 as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0).then_some(v)
    }

    pub fn memory() -> Memory {
        let total = sysctl_u64("hw.memsize").unwrap_or(0);
        let mut vm: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let rc = unsafe {
            libc::host_statistics64(
                host(),
                libc::HOST_VM_INFO64,
                &mut vm as *mut _ as libc::host_info64_t,
                &mut count,
            )
        };
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(0) as u64;
        let free = if rc == 0 {
            (vm.free_count as u64 + vm.inactive_count as u64 + vm.speculative_count as u64) * page
        } else {
            0
        };
        Memory { total, free: free.min(total) }
    }

    pub fn swap() -> Swap {
        let name = c"vm.swapusage";
        let mut x: libc::xsw_usage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::xsw_usage>();
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                &mut x as *mut _ as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Swap { total: 0, free: 0 };
        }
        Swap { total: x.xsu_total, free: x.xsu_avail }
    }

    pub fn cpu_ticks() -> Option<(u64, u64)> {
        let mut info: libc::host_cpu_load_info = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
        let rc = unsafe {
            libc::host_statistics(
                host(),
                libc::HOST_CPU_LOAD_INFO,
                &mut info as *mut _ as libc::host_info_t,
                &mut count,
            )
        };
        if rc != 0 {
            return None;
        }
        let t: Vec<u64> = info.cpu_ticks.iter().map(|v| *v as u64).collect();
        let total: u64 = t.iter().sum();
        let idle = t[libc::CPU_STATE_IDLE as usize];
        Some((total - idle, total))
    }

    pub fn process() -> Process {
        let mut info: libc::mach_task_basic_info = unsafe { std::mem::zeroed() };
        let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
        #[allow(deprecated)]
        let task = unsafe { libc::mach_task_self() };
        let rc = unsafe {
            libc::task_info(
                task,
                libc::MACH_TASK_BASIC_INFO,
                &mut info as *mut _ as libc::task_info_t,
                &mut count,
            )
        };
        let (resident, virtual_size) =
            if rc == 0 { (info.resident_size, info.virtual_size) } else { (0, 0) };
        Process {
            resident,
            virtual_size,
            cpu_millis: 0,
            open_fds: 0,
            max_fds: 0,
            threads: thread_count(),
        }
    }

    /// How many threads this task has. Every port the list hands out is a
    /// reference, given back here.
    fn thread_count() -> u64 {
        #[allow(deprecated)]
        let task = unsafe { libc::mach_task_self() };
        let mut list: libc::thread_act_array_t = std::ptr::null_mut();
        let mut count: libc::mach_msg_type_number_t = 0;
        if unsafe { libc::task_threads(task, &mut list, &mut count) } != 0 || list.is_null() {
            return 0;
        }
        for p in unsafe { std::slice::from_raw_parts(list, count as usize) } {
            unsafe { mach_port_deallocate(task, *p) };
        }
        unsafe {
            libc::vm_deallocate(
                task,
                list as libc::vm_address_t,
                (count as usize * std::mem::size_of::<libc::thread_act_t>()) as libc::vm_size_t,
            );
        }
        count as u64
    }

    pub fn threads() -> Vec<ThreadSample> {
        #[allow(deprecated)]
        let task = unsafe { libc::mach_task_self() };
        let mut list: libc::thread_act_array_t = std::ptr::null_mut();
        let mut count: libc::mach_msg_type_number_t = 0;
        if unsafe { libc::task_threads(task, &mut list, &mut count) } != 0 || list.is_null() {
            return Vec::new();
        }
        let ports = unsafe { std::slice::from_raw_parts(list, count as usize) }.to_vec();
        let mut out = Vec::with_capacity(ports.len());
        for port in &ports {
            let mut ext: libc::thread_extended_info = unsafe { std::mem::zeroed() };
            let mut n = libc::THREAD_EXTENDED_INFO_COUNT;
            let rc = unsafe {
                libc::thread_info(
                    *port,
                    libc::THREAD_EXTENDED_INFO as libc::thread_flavor_t,
                    &mut ext as *mut _ as libc::thread_info_t,
                    &mut n,
                )
            };
            let mut ident: libc::thread_identifier_info = unsafe { std::mem::zeroed() };
            let mut m = libc::THREAD_IDENTIFIER_INFO_COUNT;
            let rc2 = unsafe {
                libc::thread_info(
                    *port,
                    libc::THREAD_IDENTIFIER_INFO as libc::thread_flavor_t,
                    &mut ident as *mut _ as libc::thread_info_t,
                    &mut m,
                )
            };
            unsafe { mach_port_deallocate(task, *port) };
            if rc != 0 {
                continue;
            }
            let name = unsafe { std::ffi::CStr::from_ptr(ext.pth_name.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let (state, word) = match ext.pth_run_state {
                libc::TH_STATE_RUNNING => (ThreadState::Running, "running"),
                libc::TH_STATE_WAITING => (ThreadState::Waiting, "waiting"),
                libc::TH_STATE_UNINTERRUPTIBLE => (ThreadState::Blocked, "uninterruptible"),
                libc::TH_STATE_STOPPED => (ThreadState::Other, "stopped"),
                libc::TH_STATE_HALTED => (ThreadState::Other, "halted"),
                _ => (ThreadState::Other, "unknown"),
            };
            out.push(ThreadSample {
                id: if rc2 == 0 { ident.thread_id } else { *port as u64 },
                name,
                cpu_nanos: ext.pth_user_time + ext.pth_system_time,
                state,
                detail: format!("run_state[{word}] priority[{}]", ext.pth_curpri),
            });
        }
        unsafe {
            libc::vm_deallocate(
                task,
                list as libc::vm_address_t,
                (count as usize * std::mem::size_of::<libc::thread_act_t>()) as libc::vm_size_t,
            );
        }
        out
    }

    pub fn mount_of(path: &Path) -> (String, String) {
        let Ok(c) = std::ffi::CString::new(path.as_os_str().to_string_lossy().as_bytes()) else {
            return (String::new(), String::new());
        };
        let mut s: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
            return (String::new(), String::new());
        }
        let text = |b: &[libc::c_char]| {
            unsafe { std::ffi::CStr::from_ptr(b.as_ptr()) }.to_string_lossy().into_owned()
        };
        let on = text(&s.f_mntonname);
        let from = text(&s.f_mntfromname);
        (format!("{on} ({from})"), text(&s.f_fstypename))
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;

    /// A `/proc/meminfo` line's value, in bytes.
    fn meminfo(key: &str) -> Option<u64> {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        text.lines().find_map(|l| {
            let rest = l.strip_prefix(key)?.strip_prefix(':')?;
            let kb: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            Some(kb * 1024)
        })
    }

    pub fn memory() -> Memory {
        let total = meminfo("MemTotal").unwrap_or(0);
        let free = meminfo("MemAvailable").or_else(|| meminfo("MemFree")).unwrap_or(0);
        Memory { total, free: free.min(total) }
    }

    pub fn swap() -> Swap {
        Swap { total: meminfo("SwapTotal").unwrap_or(0), free: meminfo("SwapFree").unwrap_or(0) }
    }

    pub fn cpu_ticks() -> Option<(u64, u64)> {
        let text = std::fs::read_to_string("/proc/stat").ok()?;
        let line = text.lines().find(|l| l.starts_with("cpu "))?;
        let t: Vec<u64> = line.split_whitespace().skip(1).filter_map(|v| v.parse().ok()).collect();
        if t.len() < 4 {
            return None;
        }
        let total: u64 = t.iter().sum();
        // idle and waiting on I/O are both a processor with nothing to run
        let idle = t[3] + t.get(4).copied().unwrap_or(0);
        Some((total - idle, total))
    }

    fn page_size() -> u64 {
        unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(0) as u64
    }

    pub fn process() -> Process {
        let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
        let mut parts = statm.split_whitespace().filter_map(|v| v.parse::<u64>().ok());
        let virtual_size = parts.next().unwrap_or(0) * page_size();
        let resident = parts.next().unwrap_or(0) * page_size();
        let threads = std::fs::read_dir("/proc/self/task").map(|d| d.count() as u64).unwrap_or(0);
        Process { resident, virtual_size, cpu_millis: 0, open_fds: 0, max_fds: 0, threads }
    }

    pub fn threads() -> Vec<ThreadSample> {
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
        let Ok(dir) = std::fs::read_dir("/proc/self/task") else { return Vec::new() };
        let mut out = Vec::new();
        for entry in dir.flatten() {
            let base = entry.path();
            let Ok(stat) = std::fs::read_to_string(base.join("stat")) else { continue };
            // the name is in parentheses and may itself hold spaces or
            // parentheses, so the fields are counted from the last one
            let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else { continue };
            let name = stat[open + 1..close].to_string();
            let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
            if fields.len() < 13 {
                continue;
            }
            let utime: u64 = fields[11].parse().unwrap_or(0);
            let stime: u64 = fields[12].parse().unwrap_or(0);
            let state = match fields[0] {
                "R" => ThreadState::Running,
                "S" => ThreadState::Waiting,
                "D" => ThreadState::Blocked,
                _ => ThreadState::Other,
            };
            let wchan = std::fs::read_to_string(base.join("wchan")).unwrap_or_default();
            let wchan = wchan.trim();
            let detail = if wchan.is_empty() || wchan == "0" {
                format!("state[{}]", fields[0])
            } else {
                format!("state[{}] wchan[{wchan}]", fields[0])
            };
            out.push(ThreadSample {
                id: entry.file_name().to_string_lossy().parse().unwrap_or(0),
                name,
                cpu_nanos: (utime + stime) * 1_000_000_000 / ticks,
                state,
                detail,
            });
        }
        out
    }

    pub fn mount_of(path: &Path) -> (String, String) {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let text = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
        let mut best: Option<(String, String, String)> = None;
        for line in text.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 3 || !path.starts_with(f[1]) {
                continue;
            }
            if best.as_ref().map(|b| f[1].len() >= b.1.len()).unwrap_or(true) {
                best = Some((f[0].to_string(), f[1].to_string(), f[2].to_string()));
            }
        }
        match best {
            Some((dev, on, kind)) => (format!("{on} ({dev})"), kind),
            None => (String::new(), String::new()),
        }
    }
}
