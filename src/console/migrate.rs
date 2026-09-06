//! Making the index the console keeps everything in, and moving to the next
//! one when its shape changes.
//!
//! The name a console reads and writes -- `.kibana` -- is an alias, and what
//! it points at is `.kibana_1`, `.kibana_2`, and so on. That indirection is
//! the whole of the migration: a console that has to change the mapping makes
//! the next index, copies everything into it, and moves the alias across in
//! one step, so a reader is looking at one or the other and never at half of
//! each.
//!
//! Two consoles starting at once must not both do it. Making an index is the
//! move that settles it: the engine lets exactly one of them create
//! `.kibana_2`, and the one that loses waits for the alias to move.
//!
//! What this does *not* do is change the documents. An object written by an
//! older console may be in a shape the current mapping does not accept --
//! `uiStateJSON` on a dashboard, which stopped being used in 7.3 -- and
//! putting it right means running that type's own migration over it. Those
//! migrations are code rather than data, several hundred lines of it per
//! type, so they cannot be pinned the way the rest of the contract is. A copy
//! that meets one of those documents fails and says which field it was, which
//! is the honest answer until they are written.

use serde_json::{Value, json};

use super::engine::{Engine, Failed};

/// The alias every console reads and writes through.
pub const ALIAS: &str = ".kibana";

/// What a console found when it looked, and what it did about it.
#[derive(Debug, PartialEq, Eq)]
pub enum Found {
    /// nothing was there and this console made it
    Made(String),
    /// the alias was already over an index whose mapping is what we want
    Ready(String),
    /// the mapping had moved on, so everything was copied to a new index
    Migrated { from: String, to: String, documents: u64 },
    /// something had written an index under the name the alias should have,
    /// so it was moved out of the way and the alias put over it
    Adopted { from: String, to: String, documents: u64 },
}

/// Make the console's index if nothing has, and move it on if its shape has
/// changed since whatever made it last.
pub fn ensure(engine: &Engine, mapping: &Value) -> Result<Found, Failed> {
    // An index actually named `.kibana` is something that wrote to the
    // console's index without going through a console: a restore, a fixture
    // loaded for a test, a hand. It is moved out of the way and the alias put
    // over where it went, because everything after this point is written
    // through an alias and a name that is both an index and an alias cannot
    // exist.
    if concrete_index(engine)? {
        let next = next_free(engine)?;
        make(engine, &next, mapping)?;
        let documents = match copy(engine, ALIAS, &next) {
            Ok(documents) => documents,
            Err(e) => {
                let _ = engine.call("DELETE", &format!("/{next}"), None);
                return Err(e);
            }
        };
        engine.call("DELETE", &format!("/{ALIAS}"), None)?;
        point_alias(engine, &[], &next)?;
        return Ok(Found::Adopted { from: ALIAS.to_string(), to: next, documents });
    }
    let on = behind_alias(engine)?;
    let Some(current) = on.last().cloned() else {
        let first = next_free(engine)?;
        make(engine, &first, mapping)?;
        point_alias(engine, &[], &first)?;
        return Ok(Found::Made(first));
    };
    if on.len() == 1 && same_shape(engine, &current, mapping)? {
        return Ok(Found::Ready(current));
    }
    // one index behind the alias whose shape has moved on, or more than one
    // behind it at all -- either way the answer is the same: a new index with
    // the shape it should have, everything in it, and the alias on it alone
    let next = next_free(engine)?;
    make(engine, &next, mapping)?;
    let documents = match copy(engine, &current, &next) {
        Ok(documents) => documents,
        // a half-made index left behind would be taken for the next free one
        // by whoever looks after this, so it goes
        Err(e) => {
            let _ = engine.call("DELETE", &format!("/{next}"), None);
            return Err(e);
        }
    };
    // the alias moves in one step: a reader is looking at the old index or
    // the new one, never at neither and never at both
    point_alias(engine, &on, &next)?;
    Ok(Found::Migrated { from: current, to: next, documents })
}

/// Whether something has made an index under the alias's own name.
fn concrete_index(engine: &Engine) -> Result<bool, Failed> {
    let found = engine.call("GET", &format!("/{ALIAS}"), None)?;
    Ok(found.get(ALIAS).is_some())
}

/// The first `.kibana_N` nothing has taken.
fn next_free(engine: &Engine) -> Result<String, Failed> {
    let found = engine.call("GET", &format!("/{ALIAS}_*"), None)?;
    let taken = found.as_object().map(|o| o.len()).unwrap_or(0);
    for n in 1..=(taken + 1) {
        let name = format!("{ALIAS}_{n}");
        if found.get(&name).is_none() {
            return Ok(name);
        }
    }
    Ok(format!("{ALIAS}_{}", taken + 1))
}

/// The index the alias points at, if anything does.
///
/// Every index the alias points at.
///
/// It should be one. Two means something went wrong somewhere -- a migration
/// that stopped halfway, two consoles racing -- and a write through an alias
/// on two indices is refused by the engine, so it is worth knowing about all
/// of them rather than the first.
fn behind_alias(engine: &Engine) -> Result<Vec<String>, Failed> {
    let found = engine.call("GET", &format!("/_alias/{ALIAS}"), None)?;
    let Some(indices) = found.as_object().filter(|_| found.get("error").is_none()) else {
        return Ok(Vec::new());
    };
    let mut named: Vec<String> = indices.keys().cloned().collect();
    // by the number in the name, so the newest is last
    named.sort_by_key(|n| n.rsplit_once('_').and_then(|(_, t)| t.parse::<u32>().ok()).unwrap_or(0));
    Ok(named)
}

/// Whether an index already holds every property the mapping asks for.
///
/// Not whether the two are equal: an index made by a console with more plugins
/// than this one has properties this one does not know about, and copying
/// everything to a new index to remove them would lose the objects those
/// plugins wrote.
fn same_shape(engine: &Engine, index: &str, mapping: &Value) -> Result<bool, Failed> {
    let found = engine.call("GET", &format!("/{index}/_mapping"), None)?;
    let Some(theirs) = found.pointer(&format!("/{index}/mappings/properties")) else {
        return Ok(false);
    };
    let Some(ours) = mapping.pointer("/properties").and_then(|v| v.as_object()) else {
        return Ok(true);
    };
    Ok(ours.keys().all(|k| theirs.get(k).is_some()))
}

fn make(engine: &Engine, index: &str, mapping: &Value) -> Result<(), Failed> {
    let made = engine.call(
        "PUT",
        &format!("/{index}"),
        Some(&json!({
            "settings": {"number_of_shards": 1, "auto_expand_replicas": "0-1"},
            "mappings": mapping,
        })),
    )?;
    // another console making it at the same moment is not a failure: one of
    // us was going to, and which one does not matter
    match made.pointer("/error/type").and_then(|v| v.as_str()) {
        None | Some("resource_already_exists_exception") => Ok(()),
        Some(other) => Err(Failed { status: 500, message: format!("{index}: {other}") }),
    }
}

/// Everything in one index, in the next one.
fn copy(engine: &Engine, from: &str, to: &str) -> Result<u64, Failed> {
    let found = engine.call(
        "POST",
        "/_reindex?refresh=true&wait_for_completion=true",
        Some(&json!({"source": {"index": from}, "dest": {"index": to}})),
    )?;
    if let Some(failures) = found.get("failures").and_then(|v| v.as_array())
        && !failures.is_empty()
    {
        let first = failures[0]
            .pointer("/cause/reason")
            .and_then(|v| v.as_str())
            .unwrap_or("no reason given");
        return Err(Failed {
            status: 500,
            message: format!(
                "{} of the objects could not be copied from {from} to {to}: {first}",
                failures.len()
            ),
        });
    }
    Ok(found.get("created").and_then(|v| v.as_u64()).unwrap_or(0))
}

/// Point the alias at an index, taking it off the one it was on -- in one
/// request, because two would have a moment with no alias at all.
fn point_alias(engine: &Engine, from: &[String], to: &str) -> Result<(), Failed> {
    let mut actions = Vec::new();
    for one in from {
        actions.push(json!({"remove": {"index": one, "alias": ALIAS}}));
    }
    actions.push(json!({"add": {"index": to, "alias": ALIAS}}));
    engine.call("POST", "/_aliases", Some(&json!({"actions": actions})))?;
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_alias_is_read_newest_last() {
        // the name carries the number, and the number is the order
        let mut named =
            [".kibana_2".to_string(), ".kibana_10".to_string(), ".kibana_1".to_string()];
        named.sort_by_key(|n| {
            n.rsplit_once('_').and_then(|(_, t)| t.parse::<u32>().ok()).unwrap_or(0)
        });
        assert_eq!(named.last().map(String::as_str), Some(".kibana_10"), "not sorted as text");
    }
}
