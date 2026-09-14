//! `_nodes/hot_threads` -- which threads of the node are busy, in the plain
//! text OpenSearch writes it in.
//!
//! OpenSearch reads its threads' CPU time before and after an interval, keeps
//! the busiest, and prints a few stack snapshots of each. The CPU time and the
//! run state of every thread of this process can be had from the operating
//! system (the Mach thread calls on macOS, `/proc/self/task` on Linux); a
//! stack of another running thread cannot be had without stopping it, so
//! what stands where a stack would is what the kernel says the thread is
//! doing -- its run state, and on Linux the kernel function it sleeps in.
//! The layout is the reference's, so a tool that reads the percentages and the
//! thread names reads them here too.

use super::sysinfo::{self, ThreadSample, ThreadState};
use super::*;

/// What a hot threads request asked for.
struct Ask {
    interval_ms: u64,
    threads: usize,
    kind: String,
    ignore_idle: bool,
    snapshots: usize,
}

const KNOWN: &[&str] = &[
    "interval",
    "snapshots",
    "threads",
    "ignore_idle_threads",
    "type",
    "timeout",
    // the parameters every request may carry
    "pretty",
    "human",
    "error_trace",
    "filter_path",
    "source",
    "source_content_type",
];

pub async fn hot_threads(path: &str, p: &Params) -> Response {
    let bad = |reason: String| err(StatusCode::BAD_REQUEST, "illegal_argument_exception", reason);
    if let Some(k) = p.keys().find(|k| !KNOWN.contains(&String::as_str(k))) {
        return bad(format!("request [{path}] contains unrecognized parameter: [{k}]"));
    }
    let interval_ms = match p.get("interval") {
        None => 500,
        Some(v) => match crate::api::shared::parse_time_value_nanos(v) {
            Some(nanos) => nanos / 1_000_000,
            None => {
                return bad(format!(
                    "failed to parse setting [interval] with value [{v}] as a time value: unit \
                     is missing or unrecognized"
                ));
            }
        },
    };
    let count = |key: &str, default: usize| -> Result<usize, Response> {
        match p.get(key) {
            None => Ok(default),
            Some(v) => v.parse::<usize>().map_err(|_| {
                bad(format!("Failed to parse int parameter [{key}] with value [{v}]"))
            }),
        }
    };
    let threads = match count("threads", 3) {
        Ok(n) => n,
        Err(r) => return r,
    };
    let snapshots = match count("snapshots", 10) {
        Ok(n) => n,
        Err(r) => return r,
    };
    let ask = Ask {
        // the reference holds a request for as long as it asks; a minute is
        // longer than any operator means, and a thread held longer is lost
        interval_ms: interval_ms.min(60_000),
        threads,
        kind: p.get("type").cloned().unwrap_or_else(|| "cpu".into()).to_lowercase(),
        ignore_idle: p.get("ignore_idle_threads").map(|v| v != "false").unwrap_or(true),
        snapshots,
    };
    // the reference answers a type it does not measure with nothing at all
    if !matches!(ask.kind.as_str(), "cpu" | "wait" | "block") {
        return ([("content-type", "text/plain; charset=UTF-8")], String::new()).into_response();
    }
    let text = tokio::task::spawn_blocking(move || report(&ask)).await.unwrap_or_default();
    ([("content-type", "text/plain; charset=UTF-8")], text).into_response()
}

/// The id the operating system gives the thread asking, which is left out of
/// its own report as the reference leaves out the thread that samples.
fn own_thread_id() -> u64 {
    #[cfg(target_os = "macos")]
    {
        let mut id = 0u64;
        unsafe { libc::pthread_threadid_np(0 as libc::pthread_t, &mut id) };
        id
    }
    #[cfg(not(target_os = "macos"))]
    {
        unsafe { libc::syscall(libc::SYS_gettid) as u64 }
    }
}

/// What one thread did over the interval.
struct Tally {
    name: String,
    first_cpu: u64,
    last_cpu: u64,
    waiting: u64,
    blocked: u64,
    seen: u64,
}

fn report(ask: &Ask) -> String {
    let me = own_thread_id();
    // the run state is looked at twenty times over the interval: what a
    // thread spent waiting or blocked is the share of those looks that found
    // it so, which is as close as a sample gets to the time the JVM counts
    const LOOKS: u64 = 20;
    let step = std::time::Duration::from_micros(ask.interval_ms * 1000 / LOOKS);
    let mut tallies: std::collections::HashMap<u64, Tally> = std::collections::HashMap::new();
    for look in 0..=LOOKS {
        for t in sysinfo::threads() {
            if t.id == me {
                continue;
            }
            let e = tallies.entry(t.id).or_insert_with(|| Tally {
                name: t.name.clone(),
                first_cpu: t.cpu_nanos,
                last_cpu: t.cpu_nanos,
                waiting: 0,
                blocked: 0,
                seen: 0,
            });
            e.last_cpu = t.cpu_nanos;
            e.seen += 1;
            match t.state {
                ThreadState::Waiting => e.waiting += 1,
                ThreadState::Blocked => e.blocked += 1,
                _ => {}
            }
        }
        if look < LOOKS {
            std::thread::sleep(step);
        }
    }
    let interval_nanos = ask.interval_ms * 1_000_000;
    let time_of = |t: &Tally| -> u64 {
        match ask.kind.as_str() {
            "wait" => interval_nanos * t.waiting / t.seen.max(1),
            "block" => interval_nanos * t.blocked / t.seen.max(1),
            _ => t.last_cpu.saturating_sub(t.first_cpu),
        }
    };
    // a thread seen once was born or died during the interval, and has no
    // interval of its own to measure
    let mut ranked: Vec<(u64, &Tally)> =
        tallies.iter().filter(|(_, t)| t.seen > 1).map(|(id, t)| (*id, t)).collect();
    ranked.sort_by(|a, b| time_of(b.1).cmp(&time_of(a.1)).then(a.0.cmp(&b.0)));

    // the snapshots of the busiest, ten milliseconds apart as the reference
    // takes them
    let wanted: Vec<u64> = ranked.iter().map(|(id, _)| *id).collect();
    let mut shots: std::collections::HashMap<u64, Vec<ThreadSample>> =
        std::collections::HashMap::new();
    for i in 0..ask.snapshots {
        for t in sysinfo::threads() {
            if wanted.contains(&t.id) {
                shots.entry(t.id).or_default().push(t);
            }
        }
        if i + 1 < ask.snapshots {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    let mut body = format!(
        "Hot threads at {}, interval={}, busiestThreads={}, ignoreIdleThreads={}:\n",
        crate::store::IdxState::now_iso(),
        crate::api::shared::time_value_text(interval_nanos),
        ask.threads,
        ask.ignore_idle,
    );
    let mut shown = 0;
    for (id, t) in &ranked {
        if shown >= ask.threads {
            break;
        }
        let taken = shots.get(id).cloned().unwrap_or_default();
        // an idle thread is one that used no time and was found asleep every
        // time it was looked at: a pool worker waiting for work
        let idle = t.last_cpu == t.first_cpu
            && !taken.is_empty()
            && taken.iter().all(|s| s.state != ThreadState::Running);
        if ask.ignore_idle && idle {
            continue;
        }
        shown += 1;
        let time = time_of(t);
        let name = if t.name.is_empty() { format!("thread-{id}") } else { t.name.clone() };
        body.push_str(&format!(
            "\n{:4.1}% ({} out of {}) {} usage by thread '{}'\n",
            time as f64 * 100.0 / interval_nanos.max(1) as f64,
            crate::api::shared::time_value_text(time),
            crate::api::shared::time_value_text(interval_nanos),
            ask.kind,
            name,
        ));
        if taken.is_empty() {
            continue;
        }
        let total = taken.len();
        let all_same = taken.iter().all(|s| s.detail == taken[0].detail);
        if all_same {
            body.push_str(&format!("  {total}/{total} snapshots sharing following 1 elements\n"));
            body.push_str(&format!("    {}\n", taken[0].detail));
            continue;
        }
        // grouped by what they showed, the way the reference groups stacks
        let mut groups: Vec<(String, usize)> = Vec::new();
        for s in &taken {
            match groups.iter_mut().find(|(d, _)| *d == s.detail) {
                Some(g) => g.1 += 1,
                None => groups.push((s.detail.clone(), 1)),
            }
        }
        for (detail, n) in groups {
            if n == 1 {
                body.push_str("  unique snapshot\n");
            } else {
                body.push_str(&format!("  {n}/{total} snapshots sharing following 1 elements\n"));
            }
            body.push_str(&format!("    {detail}\n"));
        }
    }

    let mut out = format!("::: {}\n", node_text());
    for line in body.lines() {
        out.push_str("   ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out
}

/// The node, written the way OpenSearch writes a node in text:
/// `{name}{id}{ephemeral id}{host}{address}{roles}{attributes}`.
fn node_text() -> String {
    let me = crate::cluster::identity();
    let host = me.transport_address.rsplit_once(':').map(|(h, _)| h).unwrap_or(&me.host);
    let mut letters: Vec<char> = me
        .roles
        .iter()
        .filter_map(|r| match r.as_str() {
            "cluster_manager" | "master" => Some('m'),
            "data" => Some('d'),
            "ingest" => Some('i'),
            "remote_cluster_client" => Some('r'),
            "search" => Some('s'),
            "warm" => Some('w'),
            "ml" => Some('l'),
            _ => None,
        })
        .collect();
    letters.sort();
    let mut out = format!(
        "{{{}}}{{{}}}{{{}}}{{{}}}{{{}}}",
        me.name,
        me.id.as_str(),
        me.ephemeral_id.as_str(),
        host,
        me.transport_address
    );
    if !letters.is_empty() {
        out.push_str(&format!("{{{}}}", letters.into_iter().collect::<String>()));
    }
    // the attributes the node was configured with, and the ones the engine
    // adds, as `_cat/nodeattrs` lists them
    let mut attrs: Vec<(String, String)> = me
        .attributes
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().map(|s| s.to_string()).unwrap_or(v.to_string())))
        .collect();
    for (k, v) in node_attrs() {
        if !attrs.iter().any(|(x, _)| *x == k) {
            attrs.push((k, v));
        }
    }
    if !attrs.is_empty() {
        let text: Vec<String> = attrs.iter().map(|(k, v)| format!("{k}={v}")).collect();
        out.push_str(&format!("{{{}}}", text.join(", ")));
    }
    out
}
