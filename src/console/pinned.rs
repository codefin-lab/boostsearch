//! The contract, as a Dashboards that works emits it.
//!
//! Everything here was read out of a running OpenSearch Dashboards by
//! `tools/osd_pin.py`. It is version data rather than behaviour: which
//! bundles the browser loads and in what order, what each plugin's browser
//! configuration is, what every setting's default is. None of it can be
//! worked out from the distribution on disk -- the order comes from a
//! dependency sort the manifests do not give back, and a plugin's browser
//! configuration lives in its compiled server code -- which is why it is
//! pinned and why moving to a new Dashboards is a task rather than an
//! accident.

use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
pub struct Pinned {
    pub version: String,
    #[serde(rename = "buildNumber")]
    pub build_number: u64,
    pub branch: Option<String>,
    pub env: Value,
    pub csp: Value,
    pub i18n: Value,
    pub vars: Value,
    #[serde(rename = "anonymousStatusPage")]
    pub anonymous_status_page: bool,
    pub branding: Value,
    pub survey: Option<String>,
    #[serde(rename = "uiPlugins")]
    pub ui_plugins: Value,
    /// every setting the front end may ask for, and what it is when nobody
    /// has said otherwise
    #[serde(rename = "uiSettingDefaults")]
    pub setting_defaults: Value,
    /// where each bundle's own files are fetched from, by bundle name
    #[serde(rename = "publicPaths")]
    pub public_paths: std::collections::BTreeMap<String, String>,
    /// the bundles the boot script loads, in the order it loads them
    pub bundles: Vec<String>,
    #[serde(rename = "styleSheets")]
    pub style_sheets: Vec<String>,
    /// the page around the two elements: the fonts, the favicons, the loading
    /// markup and the two scripts. The same bytes for every request, so they
    /// are carried rather than written again
    #[serde(rename = "shellHead")]
    pub shell_head: String,
    #[serde(rename = "shellTail")]
    pub shell_tail: String,
    /// the script that runs before anything else, to pick the theme out of
    /// what the browser remembered
    pub startup: String,
    /// what a caller may do, as the plugins between them decided
    pub capabilities: Value,
    /// the index the console keeps everything in, as the server it replaces
    /// makes it: strict, one property per type, and a `_meta` of hashes
    #[serde(rename = "savedObjectIndex")]
    pub saved_object_index: Value,
    /// the types the management page is allowed to show
    #[serde(rename = "allowedTypes")]
    pub allowed_types: Vec<String>,
    /// what version of itself each type's attributes are written at
    #[serde(rename = "migrationVersions")]
    pub migration_versions: std::collections::BTreeMap<String, Value>,
    /// what the management page shows for each type: the icon, where to edit
    #[serde(rename = "managementMeta")]
    pub management_meta: std::collections::BTreeMap<String, Value>,
}

impl Pinned {
    pub fn read(path: &std::path::Path, version: &str) -> Result<Pinned, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            format!(
                "no pin for OpenSearch Dashboards {version} at {}: {e}\n\
                 Run tools/osd_pin.py against a running one of that version.",
                path.display()
            )
        })?;
        let pinned: Pinned =
            serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
        if pinned.version != version {
            return Err(format!(
                "{} is the contract for {} and the distribution is {version}. \
                 A pin from one version in front of another's bundles serves a page \
                 naming files that are not there, and the browser's failure would say \
                 nothing about why.",
                path.display(),
                pinned.version
            ));
        }
        Ok(pinned)
    }
}
