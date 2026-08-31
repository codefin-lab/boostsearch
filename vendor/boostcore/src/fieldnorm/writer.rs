use std::cmp::Ordering;
use std::collections::HashMap;
use std::{io, iter};

use super::{fieldnorm_to_id, FieldNormsSerializer};
use crate::schema::{Field, Schema};
use crate::DocId;

/// The `FieldNormsWriter` is in charge of tracking the fieldnorm byte
/// of each document for each field with field norms.
///
/// `FieldNormsWriter` stores a `Vec<u8>` for each tracked field, using a
/// byte per document per field.
pub struct FieldNormsWriter {
    fieldnorms_buffers: Vec<Option<Vec<u8>>>,
    /// The same byte per document, but per path of a JSON field, keyed by the
    /// field and the unordered id the path was given while indexing.
    ///
    /// A JSON field holds what a flat schema would spread over many fields, and
    /// one length for all of them makes a document look long because a path it
    /// was not searched on is long. Lucene's norm is per field; this is the
    /// same thing, one level down.
    json_buffers: HashMap<(u32, u32), Vec<u8>>,
}

impl FieldNormsWriter {
    /// Returns the fields that should have field norms computed
    /// according to the given schema.
    pub(crate) fn fields_with_fieldnorm(schema: &Schema) -> Vec<Field> {
        schema
            .fields()
            .filter_map(|(field, field_entry)| {
                if field_entry.is_indexed() && field_entry.has_fieldnorms() {
                    Some(field)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    }

    /// Initialize with state for tracking the field norm fields
    /// specified in the schema.
    pub fn for_schema(schema: &Schema) -> FieldNormsWriter {
        let mut fieldnorms_buffers: Vec<Option<Vec<u8>>> = iter::repeat_with(|| None)
            .take(schema.num_fields())
            .collect();
        for field in FieldNormsWriter::fields_with_fieldnorm(schema) {
            fieldnorms_buffers[field.field_id() as usize] = Some(Vec::with_capacity(1_000));
        }
        FieldNormsWriter {
            fieldnorms_buffers,
            json_buffers: HashMap::new(),
        }
    }

    /// The memory used inclusive childs
    pub fn mem_usage(&self) -> usize {
        self.fieldnorms_buffers
            .iter()
            .flatten()
            .map(|buf| buf.capacity())
            .sum::<usize>()
            + self.json_buffers.values().map(|buf| buf.capacity()).sum::<usize>()
    }
    /// Ensure that all documents in 0..max_doc have a byte associated with them
    /// in each of the fieldnorm vectors.
    ///
    /// Will extend with 0-bytes for documents that have not been seen.
    pub fn fill_up_to_max_doc(&mut self, max_doc: DocId) {
        for fieldnorms_buffer_opt in self.fieldnorms_buffers.iter_mut() {
            if let Some(fieldnorms_buffer) = fieldnorms_buffer_opt.as_mut() {
                fieldnorms_buffer.resize(max_doc as usize, 0u8);
            }
        }
        for buffer in self.json_buffers.values_mut() {
            buffer.resize(max_doc as usize, 0u8);
        }
    }

    /// Set the fieldnorm byte for the given document for the given field.
    ///
    /// Will internally convert the u32 `fieldnorm` value to the appropriate byte
    /// to approximate the field norm in less space.
    ///
    /// * doc       - the document id
    /// * field     - the field being set
    /// * fieldnorm - the number of terms present in document `doc` in field `field`
    pub fn record(&mut self, doc: DocId, field: Field, fieldnorm: u32) {
        if let Some(fieldnorm_buffer) = self
            .fieldnorms_buffers
            .get_mut(field.field_id() as usize)
            .and_then(Option::as_mut)
        {
            match fieldnorm_buffer.len().cmp(&(doc as usize)) {
                Ordering::Less => {
                    // we fill intermediary `DocId` as  having a fieldnorm of 0.
                    fieldnorm_buffer.resize(doc as usize, 0u8);
                }
                Ordering::Equal => {}
                Ordering::Greater => {
                    panic!("Cannot register a given fieldnorm twice")
                }
            }
            fieldnorm_buffer.push(fieldnorm_to_id(fieldnorm));
        }
    }

    /// Set the fieldnorm byte for one path of a JSON field.
    ///
    /// A document that has nothing under the path keeps the 0 it was filled
    /// with, which reads back as a length of 0.
    pub fn record_json_path(&mut self, doc: DocId, field: Field, path_id: u32, fieldnorm: u32) {
        if self
            .fieldnorms_buffers
            .get(field.field_id() as usize)
            .and_then(Option::as_ref)
            .is_none()
        {
            return;
        }
        let buffer = self
            .json_buffers
            .entry((field.field_id(), path_id))
            .or_default();
        if buffer.len() <= doc as usize {
            buffer.resize(doc as usize + 1, 0u8);
        }
        buffer[doc as usize] = fieldnorm_to_id(fieldnorm);
    }

    /// Serialize the seen fieldnorm values to the serializer for all fields.
    ///
    /// `paths` names the JSON paths by the unordered id they were given while
    /// indexing, so the per-path norms can be written under the path a query
    /// will ask for them by.
    pub fn serialize(
        &self,
        mut fieldnorms_serializer: FieldNormsSerializer,
        paths: &[&str],
    ) -> io::Result<()> {
        for (field, fieldnorms_buffer) in self.fieldnorms_buffers.iter().enumerate().filter_map(
            |(field_id, fieldnorms_buffer_opt)| {
                fieldnorms_buffer_opt.as_ref().map(|fieldnorms_buffer| {
                    (Field::from_field_id(field_id as u32), fieldnorms_buffer)
                })
            },
        ) {
            fieldnorms_serializer.serialize_field(field, fieldnorms_buffer)?;
        }
        let mut per_field: HashMap<u32, Vec<(&str, &[u8])>> = HashMap::new();
        for ((field_id, path_id), buffer) in &self.json_buffers {
            let Some(path) = paths.get(*path_id as usize) else { continue };
            per_field
                .entry(*field_id)
                .or_default()
                .push((path, &buffer[..]));
        }
        for (field_id, mut paths_norms) in per_field {
            paths_norms.sort_unstable_by_key(|(path, _)| *path);
            fieldnorms_serializer
                .serialize_json_paths(Field::from_field_id(field_id), &paths_norms)?;
        }
        fieldnorms_serializer.close()?;
        Ok(())
    }
}
