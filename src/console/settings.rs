//! `uiSettings`: what somebody has changed about how the console behaves.
//!
//! A setting has three states and the front end can tell them apart. It may be
//! at its default, in which case the server says nothing about it and the
//! front end uses the default it was given in the page. It may have been
//! changed by somebody, in which case it comes back with a `userValue`. Or it
//! may have been fixed in the configuration this server was started with, in
//! which case it comes back `isOverridden` and refusing to be written -- an
//! operator's decision is not a reader's to undo.
//!
//! They live in the engine, as a document the console owns, so that two
//! consoles in front of one cluster show the same thing and a console that is
//! restarted has not forgotten anything.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::engine::{Engine, Failed};

/// Every setting somebody has changed, and every one an operator has fixed.
pub struct Settings<'a> {
    engine: &'a Engine,
    version: &'a str,
    build_number: u64,
    overrides: &'a BTreeMap<String, Value>,
    /// the shape the index should have, for putting it back
    mapping: &'a Value,
}

impl<'a> Settings<'a> {
    pub fn new(
        engine: &'a Engine,
        version: &'a str,
        build_number: u64,
        overrides: &'a BTreeMap<String, Value>,
        mapping: &'a Value,
    ) -> Settings<'a> {
        Settings { engine, version, build_number, overrides, mapping }
    }

    /// The document the settings live in: one per version, because a setting
    /// may mean something different after an upgrade.
    fn id(&self) -> String {
        format!("config:{}", self.version)
    }

    /// What is stored, as it is stored.
    fn stored(&self) -> Result<Map<String, Value>, Failed> {
        let found = self.engine.get(&self.id())?;
        Ok(found
            .and_then(|source| source.get("config").cloned())
            .and_then(|config| config.as_object().cloned())
            .unwrap_or_default())
    }

    /// The answer the front end reads: `{settings: {key: {userValue}}}`.
    ///
    /// A setting reset to its default is stored as null and is not part of the
    /// answer -- the front end is meant to fall back to the default it already
    /// has, and telling it the value is nothing would mean something else.
    pub fn read(&self) -> Result<Value, Failed> {
        let mut out = Map::new();
        // the build number is written on the first start and is how the front
        // end notices it is talking to a server serving different bundles
        out.insert("buildNum".into(), json!({"userValue": self.build_number}));
        for (key, value) in self.stored()? {
            if value.is_null() || key == "buildNum" {
                continue;
            }
            out.insert(key, json!({"userValue": value}));
        }
        for (key, value) in self.overrides {
            out.insert(key.clone(), json!({"isOverridden": true, "userValue": value}));
        }
        Ok(json!({"settings": out}))
    }

    /// Change some settings, and answer with all of them.
    ///
    /// A value of null puts one back to its default. A key an operator has
    /// fixed is refused, and the whole request is refused rather than half of
    /// it applied: a front end that asked for two changes and got one has no
    /// way to find out which.
    pub fn write(&self, changes: &Map<String, Value>) -> Result<Value, Failed> {
        if let Some(key) = changes.keys().find(|k| self.overrides.contains_key(*k)) {
            return Err(Failed {
                status: 400,
                message: format!("Unable to update \"{key}\" because it is overridden"),
            });
        }
        let mut config = self.stored()?;
        config.insert("buildNum".into(), json!(self.build_number));
        for (key, value) in changes {
            config.insert(key.clone(), value.clone());
        }
        // the same rule as every other write: a write may not be the thing
        // that makes the console's index
        let document = json!({
                "config": config,
                "type": "config",
                "references": [],
                "migrationVersion": {"config": "7.9.0"},
            "updated_at": crate::console::now(),
        });
        if self.engine.put(&self.id(), &document).is_err() {
            super::migrate::ensure(self.engine, self.mapping)?;
            self.engine.put(&self.id(), &document)?;
        }
        self.read()
    }

    /// Put one setting back to its default.
    pub fn reset(&self, key: &str) -> Result<Value, Failed> {
        let mut changes = Map::new();
        changes.insert(key.to_string(), Value::Null);
        self.write(&changes)
    }
}
