//! Aggregations: the ones BoostCore parses, and the ones this engine walks
//! itself a bucket at a time.

mod matrix;
pub(crate) use matrix::*;
mod bucket;
pub(crate) use bucket::*;
mod composite;
pub(crate) use composite::*;
mod format;
pub(crate) use format::*;
mod histogram;
pub(crate) use histogram::*;
mod metric;
pub(crate) use metric::*;
mod pipeline;
pub(crate) use pipeline::*;
mod plan;
pub(crate) use plan::*;

/// The field types a published mapping names: the same walk the aggregations
/// use, for the parts of the API that read a mapping they do not hold.
pub fn published_field_types(
    mappings: &serde_json::Value,
    prefix: &str,
    out: &mut std::collections::HashMap<String, String>,
) {
    crate::search::aggs::plan::published_types(mappings, prefix, out)
}
