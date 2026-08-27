//! The spreadsheet data model and authoring API.
//!
//! This module holds the typed value types ([`Cell`], [`Color`], [`Font`], …),
//! the worksheet/workbook containers ([`Sheet`], [`Workbook`]), and the authoring
//! builder methods (`Sheet::write`, `Workbook::add_sheet`, …). The reader
//! populates these types; the writer serializes them. See the crate root for the
//! format dispatch and the `.xls` reader internals.

#[cfg(test)]
use std::collections::{BTreeMap, BTreeSet};

mod cell;
mod print;
mod range;
mod sheet;
mod style;
mod workbook;
mod worksheet;

use cell::{display_text, display_text_with_num_fmt};
pub use cell::{
    excel_serial_to_datetime, Cell, CellErrorType, Data, DataRef, DataType, ExcelDateTime,
    HeaderRow,
};
#[cfg(feature = "chrono")]
pub use cell::{excel_serial_to_duration, excel_serial_to_naive_datetime};

#[cfg(test)]
use print::MAX_HEADER_FOOTER_BYTES;
pub use print::{
    HeaderFooterKind, HeaderFooterMetadata, PageSetup, PrintFidelity, PrintLoss, PrintLossKind,
    PrintMetadata, PrintPageOrder,
};
#[cfg(all(feature = "serde", feature = "chrono"))]
pub use range::{
    deserialize_as_date_1900_or_none, deserialize_as_date_1900_or_string,
    deserialize_as_date_1904_or_none, deserialize_as_date_1904_or_string,
    deserialize_as_datetime_1900_or_none, deserialize_as_datetime_1900_or_string,
    deserialize_as_datetime_1904_or_none, deserialize_as_datetime_1904_or_string,
    deserialize_as_duration_or_none, deserialize_as_duration_or_string,
    deserialize_as_time_1900_or_none, deserialize_as_time_1900_or_string,
    deserialize_as_time_1904_or_none, deserialize_as_time_1904_or_string,
};
#[cfg(feature = "serde")]
pub use range::{
    deserialize_as_f64_or_none, deserialize_as_f64_or_string, deserialize_as_i64_or_none,
    deserialize_as_i64_or_string, DeError, RangeDeserializer, RangeDeserializerBuilder,
};
pub use range::{
    Dimensions, FormulaRange, FormulaRangeRow, FormulaRangeRowCells, FormulaRangeRowUsedCells,
    FormulaRangeRows, Range, RangeRow, RangeRowCells, RangeRowUsedCells, RangeRows,
};
pub(crate) use sheet::CellEntry;
pub use sheet::{DisplayCell, Sheet};
#[cfg(test)]
use sheet::{DisplayCellIndexEntry, RenderStyleRange, RenderStyleRangeIndex};
pub use style::{
    Alignment, Border, BorderStyle, CellProtection, CellStyle, Color, Fill, Font, Format,
    FormatAlign, FormatBorder, FormatPattern, FormatScript, HAlign, StyleFidelity, StyleLoss,
    StyleLossKind, TextRun, VAlign,
};
pub(crate) use style::{CellStyleOverlay, TableStyleApplication};
#[cfg(any(feature = "xlsx", test))]
pub(crate) use style::{TableStyleDefinition, TableStyleRegion};
pub use workbook::{DocProperties, LocalDefinedName, Reader, Workbook, WorkbookMetadata};
#[cfg(any(feature = "xlsx", feature = "ods", test))]
pub(crate) use worksheet::parse_decimal_ratio_u64;
#[cfg(any(feature = "ods", test))]
pub(crate) use worksheet::parse_decimal_scaled_u32;
pub(crate) use worksheet::OoxmlImplicitColumnWidth;
pub use worksheet::{
    CfRule, Chart, ChartBarDirection, ChartCachedPoint, ChartFrameFill, ChartFrameStyleLossKind,
    ChartKind, ChartMarkerSymbol, ChartSeriesCache, ChartSeriesStyle, ChartSeriesStyleLossKind,
    ChartTextStyle, ChartTextStyles, ChartUnsupportedReason, Comment, CommentAuthor, CondFormat,
    ConditionalFormatMetadata, DataValidation, DrawingAnchorBehavior, DrawingCrop, DrawingMetadata,
    DrawingObjectKind, DvKind, DvOp, Image, ImageFmt, ImportedAxisMeasure, OoxmlImplicitRowHeight,
    Picture, ProtectionOptions, Series, SheetMetadata, SheetType, SheetView, SheetVisible,
    Sparkline, SparklineKind, Table, WorksheetMetadata, XlsbDefaultColumnWidth,
};

#[cfg(test)]
mod tests;
