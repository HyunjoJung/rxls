//! Worksheet metadata and feature value objects.

mod drawing;
mod features;
mod metadata;

#[cfg(any(feature = "xlsx", feature = "ods", test))]
pub(crate) use drawing::parse_decimal_ratio_u64;
#[cfg(any(feature = "ods", test))]
pub(crate) use drawing::parse_decimal_scaled_u32;
pub(crate) use drawing::OoxmlImplicitColumnWidth;
pub use drawing::{
    ChartBarDirection, ChartCachedPoint, ChartFrameFill, ChartFrameStyleLossKind,
    ChartMarkerSymbol, ChartSeriesCache, ChartSeriesStyle, ChartSeriesStyleLossKind,
    ChartTextStyle, ChartTextStyles, ChartUnsupportedReason, DrawingAnchorBehavior, DrawingCrop,
    DrawingMetadata, DrawingObjectKind, ImportedAxisMeasure, OoxmlImplicitRowHeight,
    XlsbDefaultColumnWidth,
};
pub use features::{
    CfRule, Chart, ChartKind, Comment, CommentAuthor, CondFormat, ConditionalFormatMetadata,
    DataValidation, DvKind, DvOp, Image, ImageFmt, Picture, ProtectionOptions, Series, Sparkline,
    SparklineKind, Table,
};
pub use metadata::{SheetMetadata, SheetType, SheetView, SheetVisible, WorksheetMetadata};
