//! Compile-time checks for the public API's threading and root-path contracts.

use std::panic::{RefUnwindSafe, UnwindSafe};

use rxls::{
    Cell, ContainerParseMode, CsvOptions, Error, FormulaEvaluation, FormulaRange, ParseProvenance,
    Range, RecoveryCode, ReportEvaluation, ReportFeatures, ReportProperties, ReportStats, Sheet,
    Workbook, WorkbookReport,
};

fn assert_send_sync_static<T: Send + Sync + 'static>() {}
fn assert_unwind_safe<T: UnwindSafe + RefUnwindSafe>() {}
fn assert_std_error<T: std::error::Error + Send + Sync + 'static>() {}

#[test]
fn owned_core_and_output_types_are_send_sync() {
    assert_send_sync_static::<Cell>();
    assert_send_sync_static::<Sheet>();
    assert_send_sync_static::<Workbook>();
    assert_send_sync_static::<WorkbookReport>();
    assert_send_sync_static::<ReportEvaluation>();
    assert_send_sync_static::<CsvOptions>();
    assert_send_sync_static::<FormulaEvaluation>();
    assert_send_sync_static::<ContainerParseMode>();
    assert_send_sync_static::<RecoveryCode>();
    assert_send_sync_static::<ParseProvenance>();
    assert_std_error::<Error>();

    #[cfg(feature = "xlsx")]
    {
        assert_send_sync_static::<rxls::Spreadsheet>();
        assert_std_error::<rxls::WriteError>();
    }
}

#[test]
fn immutable_core_views_are_send_sync_and_unwind_safe() {
    assert_send_sync_static::<Range<'static>>();
    assert_send_sync_static::<FormulaRange<'static>>();
    assert_unwind_safe::<Workbook>();
    assert_unwind_safe::<Range<'static>>();
}

#[test]
fn public_return_types_are_nameable_from_the_crate_root() {
    let _: Option<ReportStats> = None;
    let _: Option<ReportProperties> = None;
    let _: Option<ReportFeatures> = None;
    let _: Option<rxls::RangeRowUsedCells<'static, 'static>> = None;
    let _: Option<rxls::FormulaRangeRowUsedCells<'static, 'static>> = None;
    let _: Option<ContainerParseMode> = None;
    let _: Option<RecoveryCode> = None;
    let _: Option<ParseProvenance> = None;
    assert_eq!(rxls::MAX_PARSE_RECOVERY_CODES, 16);
    let _ = rxls::CommentAuthor::from("reviewer");
}

#[test]
fn authored_workbook_provenance_is_not_parsed_and_maps_partial_state() {
    let mut workbook = Workbook::new();
    let provenance = workbook.parse_provenance();
    assert_eq!(provenance.container, ContainerParseMode::NotApplicable);
    assert!(provenance.recoveries().is_empty());
    assert!(!provenance.recoveries_truncated());
    assert!(!provenance.partial);
    assert!(!provenance.is_recovered());

    workbook.text_truncated = true;
    let provenance = workbook.parse_provenance();
    assert!(provenance.partial);
    assert!(provenance.recoveries().is_empty());
}
