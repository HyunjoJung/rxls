//! Bounded wire serialization for same-pass sheet interaction geometry.

use std::io::{self, Write};

use rxls_render::{CellInteractionRegion, Fixed, InteractiveRenderOutput, FIXED_UNITS_PER_PIXEL};
use serde::ser::{SerializeSeq, Serializer};
use serde::Serialize;

use super::FacadeError;

#[derive(Serialize)]
struct Response<'a> {
    svg: &'a str,
    interaction: Geometry<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Geometry<'a> {
    schema_version: u8,
    width: f64,
    height: f64,
    cells: Cells<'a>,
}

struct Cells<'a>(&'a [CellInteractionRegion]);

impl Serialize for Cells<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for cell in self.0 {
            sequence.serialize_element(&(
                cell.source.row,
                cell.source.col,
                pixels(cell.rect.x),
                pixels(cell.rect.y),
                pixels(cell.rect.width),
                pixels(cell.rect.height),
            ))?;
        }
        sequence.end()
    }
}

fn pixels(value: Fixed) -> f64 {
    value.raw() as f64 / FIXED_UNITS_PER_PIXEL as f64
}

pub(super) fn serialize(
    output: InteractiveRenderOutput,
    limit: u64,
) -> Result<String, FacadeError> {
    let response = Response {
        svg: &output.output.svg,
        interaction: Geometry {
            schema_version: 1,
            width: pixels(output.output.scene.width),
            height: pixels(output.output.scene.height),
            cells: Cells(&output.cells),
        },
    };
    let mut writer = BoundedWriter {
        bytes: Vec::new(),
        limit,
        exceeded: None,
    };
    if serde_json::to_writer(&mut writer, &response).is_err() {
        if let Some(actual) = writer.exceeded {
            return Err(FacadeError::limit("outputBytes", limit, actual));
        }
        return Err(FacadeError::simple(
            "serialization_failed",
            "interactive sheet could not be serialized",
            "output",
        ));
    }
    String::from_utf8(writer.bytes).map_err(|_| {
        FacadeError::simple(
            "serialization_failed",
            "interactive sheet JSON is not UTF-8",
            "output",
        )
    })
}

/// Reject before extending the buffer beyond the shared SVG/geometry JSON cap.
struct BoundedWriter {
    bytes: Vec<u8>,
    limit: u64,
    exceeded: Option<u64>,
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let actual = (self.bytes.len() as u64)
            .checked_add(bytes.len() as u64)
            .unwrap_or(u64::MAX);
        if actual > self.limit {
            self.exceeded = Some(actual);
            return Err(io::Error::other("interactive output byte limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_writer_rejects_before_growing_past_limit() {
        let mut writer = BoundedWriter {
            bytes: Vec::new(),
            limit: 3,
            exceeded: None,
        };
        writer.write_all(b"abc").unwrap();
        assert!(writer.write_all(b"d").is_err());
        assert_eq!(writer.bytes, b"abc");
        assert_eq!(writer.exceeded, Some(4));
    }
}
