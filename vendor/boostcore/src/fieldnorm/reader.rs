use std::sync::Arc;

use super::{fieldnorm_to_id, id_to_fieldnorm};
use crate::directory::{CompositeFile, FileSlice, OwnedBytes};
use crate::schema::{Field, Schema};
use crate::space_usage::PerFieldSpaceUsage;
use crate::DocId;

/// Reader for the fieldnorm (for each document, the number of tokens indexed in the
/// field) of all indexed fields in the index.
///
/// Each fieldnorm is approximately compressed over one byte. We refer to this byte as
/// `fieldnorm_id`.
/// The mapping from `fieldnorm` to `fieldnorm_id` is given by monotonic.
#[derive(Clone)]
pub struct FieldNormReaders {
    data: Arc<CompositeFile>,
}

impl FieldNormReaders {
    /// Creates a field norm reader.
    pub fn open(file: FileSlice) -> crate::Result<FieldNormReaders> {
        let data = CompositeFile::open(&file)?;
        Ok(FieldNormReaders {
            data: Arc::new(data),
        })
    }

    /// Returns the FieldNormReader for a specific field.
    pub fn get_field(&self, field: Field) -> crate::Result<Option<FieldNormReader>> {
        if let Some(file) = self.data.open_read(field) {
            let fieldnorm_reader = FieldNormReader::open(file)?;
            Ok(Some(fieldnorm_reader))
        } else {
            Ok(None)
        }
    }

    /// The JSON paths this field has per-path norms for, in the order they
    /// were written.
    pub fn json_paths(&self, field: Field) -> crate::Result<Vec<String>> {
        let Some(file) = self.data.open_read_with_idx(field, 1) else {
            return Ok(Vec::new());
        };
        let header = file.read_bytes()?;
        let bytes: &[u8] = header.as_slice();
        if bytes.len() < 8 {
            return Ok(Vec::new());
        }
        let num_paths = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
        let mut paths = Vec::with_capacity(num_paths);
        let mut cursor = 8;
        for _ in 0..num_paths {
            if cursor + 4 > bytes.len() {
                break;
            }
            let len = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
            cursor += 4;
            if cursor + len > bytes.len() {
                break;
            }
            match std::str::from_utf8(&bytes[cursor..cursor + len]) {
                Ok(path) => paths.push(path.to_string()),
                Err(_) => break,
            }
            cursor += len;
        }
        Ok(paths)
    }

    /// The `FieldNormReader` for one path of a JSON field.
    ///
    /// `path` is the path as it is spelled inside a term: segments separated by
    /// `JSON_PATH_SEGMENT_SEP`, without the end-of-path byte. Returns `None`
    /// when the field has no per-path norms, or none for this path.
    pub fn get_json_path(&self, field: Field, path: &[u8]) -> crate::Result<Option<FieldNormReader>> {
        let Some(file) = self.data.open_read_with_idx(field, 1) else {
            return Ok(None);
        };
        let header = file.read_bytes()?;
        let bytes: &[u8] = header.as_slice();
        if bytes.len() < 8 {
            return Ok(None);
        }
        let read_u32 = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let num_paths = read_u32(0) as usize;
        let num_docs = read_u32(4) as usize;
        let mut cursor = 8;
        let mut found: Option<usize> = None;
        for i in 0..num_paths {
            if cursor + 4 > bytes.len() {
                return Ok(None);
            }
            let len = read_u32(cursor) as usize;
            cursor += 4;
            if cursor + len > bytes.len() {
                return Ok(None);
            }
            if &bytes[cursor..cursor + len] == path {
                found = Some(i);
            }
            cursor += len;
        }
        let Some(index) = found else {
            return Ok(None);
        };
        let start = cursor + index * num_docs;
        Ok(Some(FieldNormReader::open(
            file.slice(start..start + num_docs),
        )?))
    }

    /// Return a break down of the space usage per field.
    pub fn space_usage(&self, schema: &Schema) -> PerFieldSpaceUsage {
        self.data.space_usage(schema)
    }

    /// Returns a handle to inner file
    pub fn get_inner_file(&self) -> Arc<CompositeFile> {
        self.data.clone()
    }
}

/// Reads the fieldnorm associated with a document.
///
/// The [fieldnorm](FieldNormReader::fieldnorm) represents the length associated with
/// a given Field of a given document.
#[derive(Clone)]
pub struct FieldNormReader(ReaderImplEnum);

impl From<ReaderImplEnum> for FieldNormReader {
    fn from(reader_enum: ReaderImplEnum) -> FieldNormReader {
        FieldNormReader(reader_enum)
    }
}

#[derive(Clone)]
enum ReaderImplEnum {
    FromData(OwnedBytes),
    Const {
        num_docs: u32,
        fieldnorm_id: u8,
        fieldnorm: u32,
    },
}

impl FieldNormReader {
    /// Creates a `FieldNormReader` with a constant fieldnorm.
    ///
    /// The fieldnorm will be subjected to compression as if it was coming
    /// from an array-backed fieldnorm reader.
    pub fn constant(num_docs: u32, fieldnorm: u32) -> FieldNormReader {
        let fieldnorm_id = fieldnorm_to_id(fieldnorm);
        let fieldnorm = id_to_fieldnorm(fieldnorm_id);
        ReaderImplEnum::Const {
            num_docs,
            fieldnorm_id,
            fieldnorm,
        }
        .into()
    }

    /// Opens a field norm reader given its file.
    pub fn open(fieldnorm_file: FileSlice) -> crate::Result<Self> {
        let data = fieldnorm_file.read_bytes()?;
        Ok(FieldNormReader::new(data))
    }

    fn new(data: OwnedBytes) -> Self {
        ReaderImplEnum::FromData(data).into()
    }

    /// Returns the number of documents in this segment.
    pub fn num_docs(&self) -> u32 {
        match &self.0 {
            ReaderImplEnum::FromData(data) => data.len() as u32,
            ReaderImplEnum::Const { num_docs, .. } => *num_docs,
        }
    }

    /// Returns the `fieldnorm` associated with a doc id.
    /// The fieldnorm is a value approximating the number
    /// of tokens in a given field of the `doc_id`.
    ///
    /// It is imprecise, and equal or lower than
    /// the actual number of tokens.
    ///
    /// The fieldnorm is effectively decoded from the
    /// `fieldnorm_id` by doing a simple table lookup.
    pub fn fieldnorm(&self, doc_id: DocId) -> u32 {
        match &self.0 {
            ReaderImplEnum::FromData(data) => {
                let fieldnorm_id = data.as_slice()[doc_id as usize];
                id_to_fieldnorm(fieldnorm_id)
            }
            ReaderImplEnum::Const { fieldnorm, .. } => *fieldnorm,
        }
    }

    /// Returns the `fieldnorm_id` associated with a document.
    #[inline]
    pub fn fieldnorm_id(&self, doc_id: DocId) -> u8 {
        match &self.0 {
            ReaderImplEnum::FromData(data) => {
                let fieldnorm_id = data.as_slice()[doc_id as usize];
                fieldnorm_id
            }
            ReaderImplEnum::Const { fieldnorm_id, .. } => *fieldnorm_id,
        }
    }

    /// Converts a `fieldnorm_id` into a fieldnorm.
    #[inline]
    pub fn id_to_fieldnorm(id: u8) -> u32 {
        id_to_fieldnorm(id)
    }

    /// Converts a `fieldnorm` into a `fieldnorm_id`.
    /// (This function is not injective).
    #[inline]
    pub fn fieldnorm_to_id(fieldnorm: u32) -> u8 {
        fieldnorm_to_id(fieldnorm)
    }

    #[cfg(test)]
    pub(crate) fn for_test(field_norms: &[u32]) -> FieldNormReader {
        let field_norms_id = field_norms
            .iter()
            .cloned()
            .map(FieldNormReader::fieldnorm_to_id)
            .collect::<Vec<u8>>();
        let field_norms_data = OwnedBytes::new(field_norms_id);
        FieldNormReader::new(field_norms_data)
    }
}

#[cfg(test)]
mod tests {
    use crate::fieldnorm::FieldNormReader;

    #[test]
    fn test_from_fieldnorms_array() {
        let fieldnorms = &[1, 2, 3, 4, 1_000_000];
        let fieldnorm_reader = FieldNormReader::for_test(fieldnorms);
        assert_eq!(fieldnorm_reader.num_docs(), 5);
        assert_eq!(fieldnorm_reader.fieldnorm(0), 1);
        assert_eq!(fieldnorm_reader.fieldnorm(1), 2);
        assert_eq!(fieldnorm_reader.fieldnorm(2), 3);
        assert_eq!(fieldnorm_reader.fieldnorm(3), 4);
        assert_eq!(fieldnorm_reader.fieldnorm(4), 983_064);
    }

    #[test]
    fn test_const_fieldnorm_reader_small_fieldnorm_id() {
        let fieldnorm_reader = FieldNormReader::constant(1_000_000u32, 10u32);
        assert_eq!(fieldnorm_reader.num_docs(), 1_000_000u32);
        assert_eq!(fieldnorm_reader.fieldnorm(0u32), 10u32);
        assert_eq!(fieldnorm_reader.fieldnorm_id(0u32), 10u8);
    }

    #[test]
    fn test_const_fieldnorm_reader_large_fieldnorm_id() {
        let fieldnorm_reader = FieldNormReader::constant(1_000_000u32, 300u32);
        assert_eq!(fieldnorm_reader.num_docs(), 1_000_000u32);
        assert_eq!(fieldnorm_reader.fieldnorm(0u32), 280u32);
        assert_eq!(fieldnorm_reader.fieldnorm_id(0u32), 72u8);
    }
}
