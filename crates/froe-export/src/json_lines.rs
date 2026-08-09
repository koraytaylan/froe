//! The JSON lines sink: one JSON object per node.
//!
//! Each node becomes one line:
//!
//! ```json
//! {"path":"/content","properties":{"jcr:primaryType":"nt:unstructured","title":"Hello"}}
//! ```
//!
//! Inline binaries appear as `{"binary_length":N}` and external binaries
//! as `{"binary_reference":"..."}` — binary *content* is never embedded,
//! which keeps the export fast and the output line-oriented.

use std::io::Write;

use crate::export::{ExportSink, ExportedNode};
use crate::json::{append_json_string, append_json_values};

/// An [`ExportSink`] writing one JSON object per node to a writer.
pub struct JsonLinesSink<W: Write> {
    writer: W,
    line_buffer: String,
}

impl<W: Write> JsonLinesSink<W> {
    /// Creates a sink writing to `writer`. Wrap files in a
    /// [`std::io::BufWriter`]; the sink issues one write per line.
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            line_buffer: String::with_capacity(1024),
        }
    }
}

impl<W: Write> ExportSink for JsonLinesSink<W> {
    fn write_node(&mut self, node: &ExportedNode<'_>) -> froe::Result<()> {
        self.line_buffer.clear();
        self.line_buffer.push_str("{\"path\":");
        append_json_string(&mut self.line_buffer, node.path);
        self.line_buffer.push_str(",\"properties\":{");
        for (position, property) in node.properties.iter().enumerate() {
            if position > 0 {
                self.line_buffer.push(',');
            }
            append_json_string(&mut self.line_buffer, &property.name);
            self.line_buffer.push(':');
            append_json_values(&mut self.line_buffer, &property.values);
        }
        self.line_buffer.push_str("}}\n");
        self.writer.write_all(self.line_buffer.as_bytes())?;
        Ok(())
    }

    fn finish(&mut self) -> froe::Result<()> {
        self.writer.flush()?;
        Ok(())
    }
}
