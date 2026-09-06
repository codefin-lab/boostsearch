//! The `geoip` processor: an address read into where it is.
//!
//! The lookup is a MaxMind database -- the same format and the same files
//! OpenSearch reads, so a database a cluster already has works here unchanged.
//! The databases themselves are not shipped in this repository; where they are
//! looked for, and why, is in `docs/geoip.md`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

use serde_json::{Map, Value, json};

use super::IngestError;

/// A database, opened once and kept.
pub struct Db {
    reader: maxminddb::Reader<Vec<u8>>,
    kind: Kind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    City,
    Country,
    Asn,
}

fn cache() -> &'static RwLock<HashMap<String, Arc<Db>>> {
    static C: OnceLock<RwLock<HashMap<String, Arc<Db>>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// Where a database may be. A cluster that keeps its databases where
/// OpenSearch keeps them -- `config/ingest-geoip` -- is read without being
/// told anything.
fn database_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(d) = std::env::var("BOOSTSEARCH_GEOIP_PATH") {
        out.push(PathBuf::from(d));
    }
    if let Ok(d) = std::env::var("BOOSTSEARCH_CONFIG") {
        out.push(PathBuf::from(&d).join("ingest-geoip"));
    }
    if let Ok(d) = std::env::var("BOOSTSEARCH_DATA") {
        out.push(PathBuf::from(&d).join("config").join("ingest-geoip"));
        out.push(PathBuf::from(&d).join("..").join("config").join("ingest-geoip"));
    }
    out.push(PathBuf::from("config").join("ingest-geoip"));
    // beside the binary, which is where a distribution puts them
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        out.push(dir.join("ingest-geoip"));
        out.push(dir.join("..").join("modules").join("ingest-geoip"));
    }
    out
}

/// The database a processor named, opened once and shared.
pub fn database(name: &str) -> Result<Arc<Db>, IngestError> {
    if let Some(db) = cache().read().ok().and_then(|c| c.get(name).cloned()) {
        return Ok(db);
    }
    for dir in database_dirs() {
        let path = dir.join(name);
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Ok(reader) = maxminddb::Reader::from_source(bytes) else { continue };
        // what a database holds is named in its own metadata, and what the
        // processor writes follows from that rather than from the file's name
        let kind = match reader.metadata().database_type.as_str() {
            t if t.contains("ASN") => Kind::Asn,
            t if t.contains("Country") => Kind::Country,
            _ => Kind::City,
        };
        let db = Arc::new(Db { reader, kind });
        if let Ok(mut c) = cache().write() {
            c.insert(name.to_string(), db.clone());
        }
        return Ok(db);
    }
    Err(IngestError::illegal(format!(
        "database file [{name}] doesn't exist (in the config directory ingest-geoip)"
    )))
}

/// Everything a City database can say, in the order OpenSearch names them.
const CITY_ALL: &[&str] = &[
    "ip",
    "country_iso_code",
    "country_name",
    "continent_name",
    "region_iso_code",
    "region_name",
    "city_name",
    "timezone",
    "location",
];
/// What it says when the processor was not told which properties to take.
const CITY_DEFAULT: &[&str] = &[
    "continent_name",
    "country_iso_code",
    "country_name",
    "region_iso_code",
    "region_name",
    "city_name",
    "location",
];
const COUNTRY_ALL: &[&str] = &["ip", "country_iso_code", "country_name", "continent_name"];
const COUNTRY_DEFAULT: &[&str] = &["country_iso_code", "country_name", "continent_name"];
const ASN_ALL: &[&str] = &["ip", "asn", "organization_name", "network"];

impl Db {
    fn known(&self) -> &'static [&'static str] {
        match self.kind {
            Kind::City => CITY_ALL,
            Kind::Country => COUNTRY_ALL,
            Kind::Asn => ASN_ALL,
        }
    }

    fn default_properties(&self) -> &'static [&'static str] {
        match self.kind {
            Kind::City => CITY_DEFAULT,
            Kind::Country => COUNTRY_DEFAULT,
            Kind::Asn => ASN_ALL,
        }
    }

    /// Check the properties a processor asked for against what this database
    /// can answer, the way OpenSearch checks them when the pipeline is stored.
    pub fn check(&self, wanted: &[String]) -> Result<(), IngestError> {
        for p in wanted {
            if !self.known().contains(&p.as_str()) {
                return Err(IngestError::illegal(format!(
                    "[properties] illegal property value [{p}]. valid values are {:?}",
                    self.known()
                )));
            }
        }
        Ok(())
    }

    /// What this database says about one address, as the document will hold it.
    pub fn lookup(&self, address: &str, wanted: Option<&[String]>) -> Option<Value> {
        let ip: IpAddr = address.parse().ok()?;
        let take: Vec<String> = match wanted {
            Some(w) => w.to_vec(),
            None => self.default_properties().iter().map(|s| s.to_string()).collect(),
        };
        let mut out = Map::new();
        match self.kind {
            Kind::Asn => {
                let result = self.reader.lookup(ip).ok()?;
                let found: maxminddb::geoip2::Asn = result.decode().ok()??;
                for p in &take {
                    match p.as_str() {
                        "ip" => {
                            out.insert("ip".into(), json!(address));
                        }
                        "asn" => {
                            if let Some(n) = found.autonomous_system_number {
                                out.insert("asn".into(), json!(n));
                            }
                        }
                        "organization_name" => {
                            if let Some(n) = found.autonomous_system_organization {
                                out.insert("organization_name".into(), json!(n));
                            }
                        }
                        // the block the address was found in, which the reader
                        // knows from how deep in the tree the record sat
                        "network" => {
                            if let Ok(net) = result.network() {
                                out.insert("network".into(), json!(net.to_string()));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Kind::Country => {
                let result = self.reader.lookup(ip).ok()?;
                let found: maxminddb::geoip2::Country = result.decode().ok()??;
                self.write_country(
                    &take,
                    address,
                    found.country.iso_code,
                    found.country.names.english,
                    found.continent.names.english,
                    &mut out,
                );
            }
            Kind::City => {
                let result = self.reader.lookup(ip).ok()?;
                let found: maxminddb::geoip2::City = result.decode().ok()??;
                self.write_country(
                    &take,
                    address,
                    found.country.iso_code,
                    found.country.names.english,
                    found.continent.names.english,
                    &mut out,
                );
                // a region is the first subdivision: the state, the province
                let region = found.subdivisions.first();
                for p in &take {
                    match p.as_str() {
                        "region_iso_code" => {
                            // the code a region is known by is the country's
                            // and its own together, which is what ISO 3166-2 is
                            if let (Some(country), Some(code)) =
                                (found.country.iso_code, region.and_then(|r| r.iso_code))
                            {
                                out.insert(
                                    "region_iso_code".into(),
                                    json!(format!("{country}-{code}")),
                                );
                            }
                        }
                        "region_name" => {
                            if let Some(n) = region.and_then(|r| r.names.english) {
                                out.insert("region_name".into(), json!(n));
                            }
                        }
                        "city_name" => {
                            if let Some(n) = found.city.names.english {
                                out.insert("city_name".into(), json!(n));
                            }
                        }
                        "timezone" => {
                            if let Some(tz) = found.location.time_zone {
                                out.insert("timezone".into(), json!(tz));
                            }
                        }
                        "location" => {
                            if let (Some(lat), Some(lon)) =
                                (found.location.latitude, found.location.longitude)
                            {
                                out.insert("location".into(), json!({"lat": lat, "lon": lon}));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        (!out.is_empty()).then(|| Value::Object(out))
    }

    fn write_country(
        &self,
        take: &[String],
        address: &str,
        iso: Option<&str>,
        country: Option<&str>,
        continent: Option<&str>,
        out: &mut Map<String, Value>,
    ) {
        for p in take {
            match p.as_str() {
                "ip" => {
                    out.insert("ip".into(), json!(address));
                }
                "country_iso_code" => {
                    if let Some(v) = iso {
                        out.insert("country_iso_code".into(), json!(v));
                    }
                }
                "country_name" => {
                    if let Some(v) = country {
                        out.insert("country_name".into(), json!(v));
                    }
                }
                "continent_name" => {
                    if let Some(v) = continent {
                        out.insert("continent_name".into(), json!(v));
                    }
                }
                _ => {}
            }
        }
    }
}
