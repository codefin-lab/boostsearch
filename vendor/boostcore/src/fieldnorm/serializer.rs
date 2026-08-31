use std::io;
use std::io::Write;

use crate::directory::{CompositeWrite, WritePtr};
use crate::schema::Field;

/// The fieldnorms serializer is in charge of
/// the serialization of field norms for all fields.
pub struct FieldNormsSerializer {
    composite_write: CompositeWrite,
}

impl FieldNormsSerializer {
    /// Constructor
    pub fn from_write(write: WritePtr) -> io::Result<FieldNormsSerializer> {
        // just making room for the pointer to header.
        let composite_write = CompositeWrite::wrap(write);
        Ok(FieldNormsSerializer { composite_write })
    }

    /// Serialize the given field
    pub fn serialize_field(&mut self, field: Field, fieldnorms_data: &[u8]) -> io::Result<()> {
        let write = self.composite_write.for_field(field);
        write.write_all(fieldnorms_data)?;
        write.flush()?;
        Ok(())
    }

    /// Serialize the per-path norms of one JSON field.
    ///
    /// They go in the same file, under the same field, at index 1: a header
    /// naming the paths in order, then one byte per document per path.
    ///
    /// ```text
    /// [num_paths: u32][num_docs: u32]
    /// num_paths x ([len: u32][path bytes])
    /// num_paths x (num_docs bytes)
    /// ```
    pub fn serialize_json_paths(
        &mut self,
        field: Field,
        paths: &[(&str, &[u8])],
    ) -> io::Result<()> {
        let num_docs = paths.iter().map(|(_, norms)| norms.len()).max().unwrap_or(0);
        let write = self.composite_write.for_field_with_idx(field, 1);
        write.write_all(&(paths.len() as u32).to_le_bytes())?;
        write.write_all(&(num_docs as u32).to_le_bytes())?;
        for (path, _) in paths {
            write.write_all(&(path.len() as u32).to_le_bytes())?;
            write.write_all(path.as_bytes())?;
        }
        for (_, norms) in paths {
            write.write_all(norms)?;
            // a path a later document never had still needs its byte
            for _ in norms.len()..num_docs {
                write.write_all(&[0u8])?;
            }
        }
        write.flush()?;
        Ok(())
    }

    /// Clean up / flush / close
    pub fn close(self) -> io::Result<()> {
        self.composite_write.close()?;
        Ok(())
    }
}
