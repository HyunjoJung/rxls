//! Cell style object model shared by readers and writers.

mod cell;
mod components;
mod format;
mod provenance;
mod table;

pub(crate) use cell::CellStyleOverlay;
pub use cell::{CellProtection, CellStyle};
pub use components::{
    Alignment, Border, BorderStyle, Color, Fill, Font, FormatPattern, FormatScript, HAlign,
    TextRun, VAlign,
};
pub use format::Format;
pub use provenance::{StyleFidelity, StyleLoss, StyleLossKind};
pub(crate) use table::TableStyleApplication;
#[cfg(any(feature = "xlsx", test))]
pub(crate) use table::{TableStyleDefinition, TableStyleRegion};

/// Writer alignment enum alias for format-builder APIs.
pub type FormatAlign = HAlign;

/// Writer border enum alias for format-builder APIs.
pub type FormatBorder = BorderStyle;
