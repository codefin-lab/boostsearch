//! The server the console's front end talks to.
//!
//! OpenSearch Dashboards is two programs: a Node server and a React
//! application in the browser. This is the first one. The second is left
//! exactly as the OpenSearch project publishes it -- we serve their bundle and
//! answer the requests it makes, rather than forking a line of its JavaScript.
//!
//! What the browser needs before it can run at all is in [`shell`]: a page
//! carrying the metadata the application boots from, the script that starts
//! it, and the assets both of those name. That metadata is a contract between
//! the two halves of one program and is written down nowhere, so it is pinned
//! from a Dashboards that works rather than guessed -- see `tools/osd_pin.py`
//! and `console/osd-<version>.json`.

pub mod assets;
pub mod pinned;
pub mod shell;

use std::path::PathBuf;

/// Where the console's front end and its pinned contract are.
pub struct Console {
    /// an OpenSearch Dashboards distribution: the built bundles, the assets
    /// and the plugin manifests
    pub home: PathBuf,
    /// what that distribution's server would have told the front end
    pub pinned: pinned::Pinned,
    /// the path every URL this serves is under, `""` for none
    pub base_path: String,
    /// where each plugin's built files are, by the id the browser asks for
    ///
    /// A URL names a plugin the way its manifest does -- `usageCollection` --
    /// and the directory it lives in is named another way -- `usage_collection`
    /// for the ones a distribution ships with, and the manifest's own name for
    /// the ones added to it. Guessing at the conversion works until a plugin is
    /// named in a way the guess does not cover, so the manifests are read
    /// instead: each one says which id it is, and it is standing in its own
    /// directory while it says so.
    pub plugin_dirs: std::collections::HashMap<String, PathBuf>,
}

impl Console {
    /// Read a distribution and the pin that goes with its version.
    ///
    /// Both have to be there and they have to agree: a pin from one version
    /// in front of another version's bundles serves a page that names files
    /// which are not there, and the browser's failure would say nothing about
    /// why.
    pub fn open(
        home: PathBuf,
        pins: &std::path::Path,
        base_path: String,
    ) -> Result<Console, String> {
        let package = home.join("package.json");
        let raw = std::fs::read_to_string(&package)
            .map_err(|e| format!("no OpenSearch Dashboards at {}: {e}", home.display()))?;
        let found: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", package.display()))?;
        let version = found
            .get("version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{} says no version", package.display()))?
            .to_string();
        let pinned = pinned::Pinned::read(&pins.join(format!("osd-{version}.json")), &version)?;
        let plugin_dirs = plugin_dirs(&home);
        Ok(Console { home, pinned, base_path, plugin_dirs })
    }
}

/// Every plugin in a distribution, by the id its manifest gives it.
fn plugin_dirs(home: &std::path::Path) -> std::collections::HashMap<String, PathBuf> {
    let mut out = std::collections::HashMap::new();
    for root in ["src/plugins", "plugins"] {
        let Ok(entries) = std::fs::read_dir(home.join(root)) else { continue };
        for entry in entries.flatten() {
            let manifest = entry.path().join("opensearch_dashboards.json");
            let Ok(raw) = std::fs::read_to_string(&manifest) else { continue };
            let Ok(found) = serde_json::from_str::<serde_json::Value>(&raw) else { continue };
            if let Some(id) = found.get("id").and_then(|v| v.as_str()) {
                out.insert(id.to_string(), entry.path());
            }
        }
    }
    out
}
