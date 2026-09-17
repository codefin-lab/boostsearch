//! The search and indexing slow logs.
//!
//! `index.search.slowlog.threshold.*` and `index.indexing.slowlog.threshold.*`
//! were accepted, stored and reported, and nothing was ever written: an
//! operator who set `query.warn: 1s` to find the slow searches was told there
//! were none. An operation that took longer than a threshold is written here,
//! in the text OpenSearch writes -- to the node's log output, and to
//! `<cluster>_index_search_slowlog.log` and `<cluster>_index_indexing_slowlog.log`
//! when the node has a logs directory (`path.logs`, or `VELOSEARCH_LOGS`).

use std::io::Write;

/// The four levels a threshold is set for, most severe first.
const LEVELS: [&str; 4] = ["WARN", "INFO", "DEBUG", "TRACE"];

/// One slow log's thresholds, in nanoseconds, in the order of `LEVELS`; a
/// negative threshold is one that is off, which is every one by default.
#[derive(Clone, Debug, PartialEq)]
pub struct Thresholds {
    pub nanos: [i64; 4],
    /// the least severe level written: `index.*.slowlog.level`
    pub level: usize,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds { nanos: [-1; 4], level: 3 }
    }
}

impl Thresholds {
    /// Read the thresholds under `prefix` -- `search.slowlog.threshold.query`
    /// and the like -- with a setting lookup that knows every shape a setting
    /// is written in.
    pub fn read(setting: impl Fn(&str) -> Option<String>, prefix: &str, level_key: &str) -> Self {
        let mut nanos = [-1i64; 4];
        for (i, level) in ["warn", "info", "debug", "trace"].iter().enumerate() {
            if let Some(v) = setting(&format!("{prefix}.{level}")) {
                nanos[i] = match v.trim() {
                    "-1" => -1,
                    t => crate::api::shared::parse_time_value_nanos(t)
                        .map(|n| n as i64)
                        .unwrap_or(-1),
                };
            }
        }
        let level = setting(level_key)
            .and_then(|l| LEVELS.iter().position(|x| x.eq_ignore_ascii_case(l.trim())))
            .unwrap_or(3);
        Thresholds { nanos, level }
    }

    pub fn is_off(&self) -> bool {
        self.nanos.iter().all(|n| *n < 0)
    }

    /// The most severe level whose threshold `took` reached, if one did and
    /// it is one the log writes.
    fn level_for(&self, took: u64) -> Option<&'static str> {
        (0..4)
            .find(|i| self.nanos[*i] >= 0 && took as i64 >= self.nanos[*i])
            .filter(|i| *i <= self.level)
            .map(|i| LEVELS[i])
    }
}

/// Every slow log setting of an index, read when its settings change.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SlowLogKnobs {
    pub query: Thresholds,
    pub fetch: Thresholds,
    pub index: Thresholds,
    /// how much of a document's source an indexing entry carries
    pub index_source_chars: usize,
}

impl SlowLogKnobs {
    pub fn read(setting: impl Fn(&str) -> Option<String>) -> Self {
        SlowLogKnobs {
            query: Thresholds::read(
                &setting,
                "search.slowlog.threshold.query",
                "search.slowlog.level",
            ),
            fetch: Thresholds::read(
                &setting,
                "search.slowlog.threshold.fetch",
                "search.slowlog.level",
            ),
            index: Thresholds::read(
                &setting,
                "indexing.slowlog.threshold.index",
                "indexing.slowlog.level",
            ),
            index_source_chars: match setting("indexing.slowlog.source").as_deref() {
                Some("false") => 0,
                Some("true") => usize::MAX,
                Some(n) => n.parse().unwrap_or(1000),
                None => 1000,
            },
        }
    }
}

/// A search phase that may belong in the search slow log: `phase` is
/// `query` or `fetch`.
#[allow(clippy::too_many_arguments)]
pub fn search(
    knobs: &Thresholds,
    phase: &str,
    index: &str,
    took: u64,
    total_hits: u64,
    groups: &[String],
    total_shards: u64,
    source: &serde_json::Value,
) {
    let Some(level) = knobs.level_for(took) else { return };
    let message = format!(
        "[{index}][0] took[{}], took_millis[{}], total_hits[{total_hits} hits], types[], \
         stats[{}], search_type[QUERY_THEN_FETCH], total_shards[{total_shards}], source[{}], \
         id[]",
        crate::api::shared::time_value_text(took),
        took / 1_000_000,
        groups.join(", "),
        source,
    );
    write("index_search_slowlog", &format!("index.search.slowlog.{phase}"), level, &message);
}

/// A write that may belong in the indexing slow log.
pub fn indexing(
    knobs: &SlowLogKnobs,
    index: &str,
    uuid: &str,
    took: u64,
    id: &str,
    routing: Option<&str>,
    source: &str,
) {
    let Some(level) = knobs.index.level_for(took) else { return };
    let mut message = format!(
        "[{index}/{uuid}] took[{}], took_millis[{}], id[{id}], routing[{}]",
        crate::api::shared::time_value_text(took),
        took / 1_000_000,
        routing.unwrap_or(""),
    );
    if knobs.index_source_chars > 0 && !source.is_empty() {
        // the source as one line, cut at a character rather than inside one
        let one_line = serde_json::from_str::<serde_json::Value>(source)
            .map(|v| v.to_string())
            .unwrap_or_else(|_| source.replace('\n', " "));
        let cut: String = one_line.chars().take(knobs.index_source_chars).collect();
        message.push_str(&format!(", source[{}]", cut.trim()));
    }
    write("index_indexing_slowlog", "index.indexing.slowlog.index", level, &message);
}

/// The logger name the way the reference abbreviates it in its log lines:
/// every part but the last cut to its first letter.
fn short_logger(name: &str) -> String {
    let parts: Vec<&str> = name.split('.').collect();
    let mut out: Vec<String> =
        parts[..parts.len() - 1].iter().map(|p| p.chars().take(1).collect()).collect();
    out.push(parts[parts.len() - 1].to_string());
    out.join(".")
}

fn write(file: &str, logger: &str, level: &str, message: &str) {
    let me = crate::cluster::identity();
    let now = crate::store::IdxState::now_iso();
    // the reference's log pattern: an ISO time with a comma before the
    // milliseconds, the level padded to five, the logger to twenty-five
    let stamp = now.trim_end_matches('Z').replacen('.', ",", 1);
    let line =
        format!("[{stamp}][{level:<5}][{:<25}] [{}] {message}\n", short_logger(logger), me.name);
    eprint!("{line}");
    let Some(dir) = logs_dir() else { return };
    let path = dir.join(format!("{}_{file}.log", me.cluster_name));
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Where the node writes its log files, if it was given somewhere.
fn logs_dir() -> Option<&'static std::path::Path> {
    static DIR: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let named =
            std::env::var("VELOSEARCH_LOGS").ok().filter(|d| !d.is_empty()).or_else(|| {
                crate::tls::node_setting(&crate::tls::node_settings(), "path.logs")
                    .filter(|d| !d.is_empty())
            })?;
        let dir = std::path::PathBuf::from(named);
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir)
    })
    .as_deref()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_most_severe_threshold_reached_names_the_level() {
        let t = Thresholds { nanos: [10_000_000_000, 1_000_000_000, 0, -1], level: 3 };
        assert_eq!(t.level_for(2_000_000_000), Some("INFO"));
        assert_eq!(t.level_for(5), Some("DEBUG"));
        assert_eq!(t.level_for(20_000_000_000), Some("WARN"));
        let quiet = Thresholds { level: 1, ..t };
        assert_eq!(quiet.level_for(5), None, "debug is below the level the log writes");
    }

    #[test]
    fn a_logger_is_abbreviated_as_the_reference_writes_it() {
        assert_eq!(short_logger("index.search.slowlog.query"), "i.s.s.query");
    }
}
