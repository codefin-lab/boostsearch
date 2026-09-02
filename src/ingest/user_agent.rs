//! The `user_agent` processor: a browser's user agent string read into the
//! browser, its version, the operating system and the device, by the
//! regexes uap-core keeps (shipped here as OpenSearch ships them).

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use serde_json::{Map, Value, json};

use super::IngestError;

#[derive(Clone)]
struct Rule {
    regex: Arc<fancy_regex::Regex>,
    family: Option<String>,
    v1: Option<String>,
    v2: Option<String>,
    v3: Option<String>,
    v4: Option<String>,
    brand: Option<String>,
    model: Option<String>,
}

pub struct Parser {
    agents: Vec<Rule>,
    os: Vec<Rule>,
    devices: Vec<Rule>,
}

fn rules(section: &serde_yaml::Value, kind: &str) -> Vec<Rule> {
    let Some(list) = section.as_sequence() else { return Vec::new() };
    let text =
        |m: &serde_yaml::Value, k: &str| m.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    list.iter()
        .filter_map(|m| {
            let mut pattern = text(m, "regex")?;
            if text(m, "regex_flag").as_deref() == Some("i") {
                pattern = format!("(?i){pattern}");
            }
            let regex = fancy_regex::Regex::new(&pattern).ok()?;
            Some(Rule {
                regex: Arc::new(regex),
                family: text(
                    m,
                    if kind == "device" { "device_replacement" } else { "family_replacement" },
                )
                .or_else(|| if kind == "os" { text(m, "os_replacement") } else { None }),
                v1: text(m, if kind == "os" { "os_v1_replacement" } else { "v1_replacement" }),
                v2: text(m, if kind == "os" { "os_v2_replacement" } else { "v2_replacement" }),
                v3: text(m, if kind == "os" { "os_v3_replacement" } else { "v3_replacement" }),
                v4: text(m, if kind == "os" { "os_v4_replacement" } else { "v4_replacement" }),
                brand: text(m, "brand_replacement"),
                model: text(m, "model_replacement"),
            })
        })
        .collect()
}

impl Parser {
    pub fn from_yaml(text: &str) -> Result<Parser, IngestError> {
        let doc: serde_yaml::Value = serde_yaml::from_str(text)
            .map_err(|e| IngestError::illegal(format!("error while reading regex file: {e}")))?;
        let section = |k: &str| doc.get(k).cloned().unwrap_or(serde_yaml::Value::Null);
        Ok(Parser {
            agents: rules(&section("user_agent_parsers"), "agent"),
            os: rules(&section("os_parsers"), "os"),
            devices: rules(&section("device_parsers"), "device"),
        })
    }

    pub fn parse(&self, ua: &str) -> Parsed {
        let mut out = Parsed {
            name: "Other".into(),
            os_name: "Other".into(),
            device: "Other".into(),
            ..Parsed::default()
        };
        for r in &self.agents {
            if let Ok(Some(c)) = r.regex.captures(ua) {
                out.name = replaced(&r.family, &c, 1).unwrap_or_else(|| "Other".into());
                out.major = replaced(&r.v1, &c, 2);
                out.minor = replaced(&r.v2, &c, 3);
                out.patch = replaced(&r.v3, &c, 4);
                out.build = replaced(&r.v4, &c, 5);
                break;
            }
        }
        for r in &self.os {
            if let Ok(Some(c)) = r.regex.captures(ua) {
                out.os_name = replaced(&r.family, &c, 1).unwrap_or_else(|| "Other".into());
                out.os_major = replaced(&r.v1, &c, 2);
                out.os_minor = replaced(&r.v2, &c, 3);
                out.os_patch = replaced(&r.v3, &c, 4);
                out.os_build = replaced(&r.v4, &c, 5);
                break;
            }
        }
        for r in &self.devices {
            if let Ok(Some(c)) = r.regex.captures(ua) {
                out.device = replaced(&r.family, &c, 1).unwrap_or_else(|| "Other".into());
                out.brand = replaced(&r.brand, &c, 0);
                out.model = replaced(&r.model, &c, 1);
                break;
            }
        }
        out
    }
}

/// A replacement with `$1`-style holes filled from the match, or the
/// numbered group itself where no replacement was given.
fn replaced(
    template: &Option<String>,
    c: &fancy_regex::Captures<'_>,
    group: usize,
) -> Option<String> {
    let group_text = |i: usize| c.get(i).map(|m| m.as_str().to_string()).filter(|s| !s.is_empty());
    match template {
        Some(t) => {
            let mut out = t.clone();
            for i in (1..=9).rev() {
                let hole = format!("${i}");
                if out.contains(&hole) {
                    out = out.replace(&hole, &group_text(i).unwrap_or_default());
                }
            }
            let out = out.trim().to_string();
            (!out.is_empty()).then_some(out)
        }
        None => {
            if group == 0 {
                None
            } else {
                group_text(group)
            }
        }
    }
}

#[derive(Default, Debug)]
pub struct Parsed {
    pub name: String,
    pub major: Option<String>,
    pub minor: Option<String>,
    pub patch: Option<String>,
    pub build: Option<String>,
    pub os_name: String,
    pub os_major: Option<String>,
    pub os_minor: Option<String>,
    pub os_patch: Option<String>,
    pub os_build: Option<String>,
    pub device: String,
    pub brand: Option<String>,
    pub model: Option<String>,
}

fn version_of(parts: &[&Option<String>]) -> Option<String> {
    let v: Vec<&str> = parts.iter().filter_map(|p| p.as_deref()).collect();
    (!v.is_empty()).then(|| v.join("."))
}

impl Parsed {
    /// The fields the processor writes, in the ECS shape: `name`, `version`,
    /// `os.{name,version,full}`, `device.name`, `original`.
    pub fn to_json(&self, original: &str, properties: Option<&[String]>) -> Value {
        let wanted = |k: &str| properties.map(|p| p.iter().any(|x| x == k)).unwrap_or(true);
        let mut out = Map::new();
        if wanted("name") {
            out.insert("name".into(), json!(self.name));
        }
        if wanted("version")
            && let Some(v) = version_of(&[&self.major, &self.minor, &self.patch, &self.build])
        {
            out.insert("version".into(), json!(v));
        }
        if wanted("os") {
            let mut os = Map::new();
            os.insert("name".into(), json!(self.os_name));
            if let Some(v) =
                version_of(&[&self.os_major, &self.os_minor, &self.os_patch, &self.os_build])
            {
                os.insert("version".into(), json!(v));
                os.insert("full".into(), json!(format!("{} {}", self.os_name, v)));
            } else {
                os.insert("full".into(), json!(self.os_name));
            }
            out.insert("os".into(), Value::Object(os));
        }
        if wanted("device") {
            out.insert("device".into(), json!({"name": self.device}));
        }
        if wanted("original") {
            out.insert("original".into(), json!(original));
        }
        Value::Object(out)
    }
}

/// The parsers by regex file, built once each.
fn cache() -> &'static RwLock<HashMap<String, Arc<Parser>>> {
    static CACHE: OnceLock<RwLock<HashMap<String, Arc<Parser>>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// The parser a processor asks for: the shipped regexes, or a file under
/// `config/ingest-user-agent/`.
pub fn parser(regex_file: Option<&str>) -> Result<Arc<Parser>, IngestError> {
    let key = regex_file.unwrap_or("").to_string();
    if let Some(p) = cache().read().ok().and_then(|c| c.get(&key).cloned()) {
        return Ok(p);
    }
    let text = match regex_file {
        None => include_str!("regexes.yml").to_string(),
        Some(name) => {
            let mut tried = Vec::new();
            for dir in config_dirs() {
                let path = dir.join("ingest-user-agent").join(name);
                tried.push(path.display().to_string());
                if let Ok(t) = std::fs::read_to_string(&path) {
                    return finish(&key, &t);
                }
            }
            return Err(IngestError::illegal(format!(
                "regex file [{name}] doesn't exist (in the config directory ingest-user-agent)"
            )));
        }
    };
    finish(&key, &text)
}

fn finish(key: &str, text: &str) -> Result<Arc<Parser>, IngestError> {
    let p = Arc::new(Parser::from_yaml(text)?);
    if let Ok(mut c) = cache().write() {
        c.insert(key.to_string(), p.clone());
    }
    Ok(p)
}

fn config_dirs() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(d) = std::env::var("BOOSTSEARCH_CONFIG") {
        out.push(std::path::PathBuf::from(d));
    }
    if let Ok(d) = std::env::var("BOOSTSEARCH_DATA") {
        out.push(std::path::PathBuf::from(&d).join("config"));
        out.push(std::path::PathBuf::from(&d).join("..").join("config"));
    }
    out.push(std::path::PathBuf::from("config"));
    out
}
