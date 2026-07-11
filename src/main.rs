use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
#[cfg(any(feature = "xlsx", feature = "ods"))]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rxls::{
    Cell, DocProperties, Error, SheetType, SheetView, SheetVisible, Workbook, WorkbookReport,
};

fn main() -> ExitCode {
    let mut args = env::args();
    let program = args.next().unwrap_or_else(|| "rxls".to_string());
    let Some(command) = args.next() else {
        print_usage(&program);
        return ExitCode::from(64);
    };

    match command.as_str() {
        "info" => {
            let Some(path) = args.next() else {
                eprintln!("usage: {program} info <file>");
                return ExitCode::from(64);
            };
            if args.next().is_some() {
                eprintln!("usage: {program} info <file>");
                return ExitCode::from(64);
            }
            info(&path)
        }
        "dump" => match parse_sheet_limit_args(&program, "dump", &mut args) {
            Ok((path, sheet, limit)) => dump(&path, sheet, limit),
            Err(code) => code,
        },
        "csv" => match parse_csv_args(&program, &mut args) {
            Ok((path, sheet, delimiter)) => csv(&path, sheet, delimiter),
            Err(code) => code,
        },
        "sheet" => match parse_sheet_args(&program, "sheet", &mut args) {
            Ok((path, sheet_index)) => inspect_sheet(&path, sheet_index),
            Err(code) => code,
        },
        "formula" => match parse_sheet_limit_args(&program, "formula", &mut args) {
            Ok((path, sheet, limit)) => formula(&path, sheet, limit),
            Err(code) => code,
        },
        "diagnose" => {
            let Some(path) = args.next() else {
                eprintln!("usage: {program} diagnose <file>");
                return ExitCode::from(64);
            };
            if args.next().is_some() {
                eprintln!("usage: {program} diagnose <file>");
                return ExitCode::from(64);
            }
            diagnose(&path)
        }
        "metadata" => {
            let Some(path) = args.next() else {
                eprintln!("usage: {program} metadata <file>");
                return ExitCode::from(64);
            };
            if args.next().is_some() {
                eprintln!("usage: {program} metadata <file>");
                return ExitCode::from(64);
            }
            metadata(&path)
        }
        "inspect-package" => match parse_limit_args(&program, "inspect-package", &mut args) {
            Ok((path, limit)) => inspect_package(&path, limit),
            Err(code) => code,
        },
        "inspect-output" => match parse_limit_args(&program, "inspect-output", &mut args) {
            Ok((path, limit)) => inspect_output(&path, limit),
            Err(code) => code,
        },
        "compare" => match parse_compare_args(&program, &mut args) {
            Ok((left, right, limit)) => compare(&left, &right, limit),
            Err(code) => code,
        },
        "corpus-report" => match parse_limit_args(&program, "corpus-report", &mut args) {
            Ok((path, limit)) => corpus_report(&path, limit),
            Err(code) => code,
        },
        "fixture-report" => match parse_limit_args(&program, "fixture-report", &mut args) {
            Ok((path, limit)) => fixture_report(&path, limit),
            Err(code) => code,
        },
        "-V" | "--version" | "version" => {
            if args.next().is_some() {
                eprintln!("usage: {program} --version");
                return ExitCode::from(64);
            }
            println!("rxls {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "-h" | "--help" | "help" => {
            print_usage(&program);
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("{program}: unknown command {other:?}");
            print_usage(&program);
            ExitCode::from(64)
        }
    }
}

fn info(path: &str) -> ExitCode {
    let (bytes, workbook) = match read_workbook(path) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };

    let metadata = workbook.metadata();
    println!("rxls info");
    println!("path: {path}");
    println!("format: {}", detect_format(path, &bytes));
    println!("sheets: {}", metadata.sheets.len());
    println!("date1904: {}", metadata.date1904);
    println!("partial: {}", metadata.text_truncated);
    println!("defined_names: {}", metadata.defined_names.len());
    if let Some(title) = metadata.properties.title.as_deref() {
        println!("title: {title}");
    }

    for (idx, sheet) in metadata.sheets.iter().enumerate() {
        let dimensions = workbook
            .sheets
            .get(idx)
            .and_then(|sheet| sheet.dimensions())
            .map(format_dimensions)
            .unwrap_or_else(|| "empty".to_string());
        let cell_count = workbook
            .sheets
            .get(idx)
            .map(|sheet| sheet.cells().count())
            .unwrap_or(0);
        println!(
            "sheet[{idx}]: {} type={} visible={} dimensions={} cells={}",
            sheet.name,
            sheet_type_name(sheet.typ),
            visible_name(sheet.visible),
            dimensions,
            cell_count
        );
    }

    ExitCode::SUCCESS
}

fn dump(path: &str, sheet_index: usize, limit: usize) -> ExitCode {
    let (_bytes, workbook) = match read_workbook(path) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };

    let Some(sheet) = workbook.sheets.get(sheet_index) else {
        eprintln!(
            "sheet index {sheet_index} out of range (sheets={})",
            workbook.sheets.len()
        );
        return ExitCode::from(65);
    };

    println!("rxls dump");
    println!("path: {path}");
    println!("sheet: {sheet_index} {}", sheet.name);
    println!("limit: {limit} cells");

    let mut printed = 0usize;
    let mut truncated = false;
    'rows: for (row, cells) in sheet.rows() {
        for (col, cell) in cells {
            if printed >= limit {
                truncated = true;
                break 'rows;
            }
            let value = cell_value(cell);
            print!(
                "{}\t{}\t{}",
                a1(row, col),
                cell_kind(cell),
                escape_dump_text(&value)
            );
            if let Some(display) = sheet.formatted(row, col) {
                if display != value {
                    print!("\tdisplay={}", escape_dump_text(display));
                }
            }
            println!();
            printed += 1;
        }
    }
    println!("truncated: {truncated}");

    ExitCode::SUCCESS
}

fn csv(path: &str, sheet_index: usize, delimiter: char) -> ExitCode {
    let (_bytes, workbook) = match read_workbook(path) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };

    if workbook.sheets.get(sheet_index).is_none() {
        eprintln!(
            "sheet index {sheet_index} out of range (sheets={})",
            workbook.sheets.len()
        );
        return ExitCode::from(65);
    }
    let Some(csv) = workbook.to_csv_with_delimiter(sheet_index, delimiter) else {
        eprintln!("sheet index {sheet_index} is not a worksheet");
        return ExitCode::from(65);
    };

    print!("{csv}");
    if !csv.is_empty() {
        println!();
    }
    ExitCode::SUCCESS
}

fn inspect_sheet(path: &str, sheet_index: usize) -> ExitCode {
    let (_bytes, workbook) = match read_workbook(path) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };

    let Some(sheet) = workbook.sheets.get(sheet_index) else {
        eprintln!(
            "sheet index {sheet_index} out of range (sheets={})",
            workbook.sheets.len()
        );
        return ExitCode::from(65);
    };

    let dimensions = sheet
        .dimensions()
        .map(format_dimensions)
        .unwrap_or_else(|| "empty".to_string());

    println!("rxls sheet");
    println!("path: {path}");
    println!("sheet: {sheet_index} {}", sheet.name);
    println!("type: {}", sheet_type_name(sheet.sheet_type()));
    println!("visible: {}", visible_name(sheet.visible()));
    println!("dimensions: {dimensions}");
    println!("cells: {}", sheet.cells().count());
    println!("merged_ranges: {}", sheet.merged_ranges().len());
    println!("hyperlinks: {}", sheet.hyperlinks().len());
    println!("comments: {}", sheet.comments().len());
    println!("tables: {}", sheet.tables().len());
    println!("data_validations: {}", sheet.data_validations().len());
    println!("conditional_formats: {}", sheet.conditional_formats().len());
    println!("autofilter: {}", sheet.autofilter_range().is_some());
    println!("page_setup: {}", sheet.page_setup().is_some());
    println!("images: {}", sheet.images().len());
    println!("charts: {}", sheet.charts().len());
    println!("sparklines: {}", sheet.sparklines().len());
    print_indexed_summaries("merged_range", merged_range_summaries(sheet));
    print_indexed_summaries("hyperlink", hyperlink_summaries(sheet));
    print_indexed_summaries("comment", comment_summaries(sheet));
    print_indexed_summaries("table", table_summaries(sheet));
    print_indexed_summaries("data_validation", data_validation_summaries(sheet));
    print_indexed_summaries("conditional_format", conditional_format_summaries(sheet));
    if let Some(autofilter) = sheet.autofilter_range() {
        println!("autofilter_range: {}", format_dimensions(autofilter));
    }
    if let Some(page_setup) = sheet.page_setup() {
        println!("page_setup_detail: {}", page_setup_summary(page_setup));
    }
    print_indexed_summaries("image", image_summaries(sheet));
    print_indexed_summaries("chart", chart_summaries(sheet));
    print_indexed_summaries("sparkline", sparkline_summaries(sheet));
    println!("sheet_view: {}", sheet_view_summary(sheet.sheet_view()));
    println!("tab_color: {}", optional_color_summary(sheet.tab_color()));
    println!("print_options: {}", print_options_summary(sheet));
    println!("protection: {}", protection_summary(sheet));
    println!(
        "row_outline: {}",
        row_outline_summary(sheet.row_outline_levels())
    );
    println!(
        "col_outline: {}",
        col_outline_summary(sheet.col_outline_levels())
    );
    println!(
        "collapsed_rows: {}",
        collapsed_rows_summary(sheet.collapsed_rows())
    );
    println!("outline_summary: {}", outline_summary(sheet));

    ExitCode::SUCCESS
}

fn print_indexed_summaries(label: &str, summaries: Vec<String>) {
    for (index, summary) in summaries.into_iter().enumerate() {
        println!("{label}[{index}]: {summary}");
    }
}

fn formula(path: &str, sheet_index: usize, limit: usize) -> ExitCode {
    let (_bytes, workbook) = match read_workbook(path) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };

    let Some(sheet) = workbook.sheets.get(sheet_index) else {
        eprintln!(
            "sheet index {sheet_index} out of range (sheets={})",
            workbook.sheets.len()
        );
        return ExitCode::from(65);
    };
    let Some(formulas) = workbook.worksheet_formula_at(sheet_index) else {
        eprintln!("sheet index {sheet_index} is not a worksheet");
        return ExitCode::from(65);
    };

    println!("rxls formula");
    println!("path: {path}");
    println!("sheet: {sheet_index} {}", sheet.name);
    println!("limit: {limit} formulas");

    let mut cells = formulas.used_cells_abs().peekable();
    for (row, col, formula) in cells.by_ref().take(limit) {
        let cached = match sheet.cell(row, col) {
            Some(Cell::Formula { cached, .. }) => compare_cell_value(Some(cached.as_ref())),
            cell => compare_cell_value(cell),
        };
        println!(
            "{}\t{}\tcached={}",
            a1(row, col),
            escape_dump_text(formula),
            cached
        );
    }
    let truncated = cells.peek().is_some();
    println!("truncated: {truncated}");

    ExitCode::SUCCESS
}

fn diagnose(path: &str) -> ExitCode {
    let (bytes, workbook) = match read_workbook(path) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };

    #[cfg(feature = "xlsx")]
    let report =
        WorkbookReport::from_workbook_with_package(detect_format(path, &bytes), &workbook, &bytes);
    #[cfg(not(feature = "xlsx"))]
    let report = WorkbookReport::from_workbook(detect_format(path, &bytes), &workbook);
    println!("{}", report.to_json());
    ExitCode::SUCCESS
}

fn metadata(path: &str) -> ExitCode {
    let (_bytes, workbook) = match read_workbook(path) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };

    let metadata = workbook.metadata();
    let counts = workbook_r3_metadata_counts(&workbook);
    println!("rxls metadata");
    println!("path: {path}");
    println!("date1904: {}", metadata.date1904);
    println!("partial: {}", metadata.text_truncated);
    println!("structure_protected: {}", metadata.structure_protected);
    if let Some(index) = metadata.active_sheet {
        println!("active_sheet: {index}");
    }
    if let Some(name) = metadata.active_sheet_name {
        println!("active_sheet_name: {}", escape_dump_text(name));
    }
    print_property("title", metadata.properties.title.as_deref());
    print_property("subject", metadata.properties.subject.as_deref());
    print_property("creator", metadata.properties.creator.as_deref());
    print_property("keywords", metadata.properties.keywords.as_deref());
    print_property("description", metadata.properties.description.as_deref());
    print_property(
        "last_modified_by",
        metadata.properties.last_modified_by.as_deref(),
    );
    print_property("company", metadata.properties.company.as_deref());
    print_property("created", metadata.properties.created.as_deref());

    for (name, refers_to) in metadata.defined_names {
        println!(
            "defined_name: {}={}",
            escape_dump_text(name),
            escape_dump_text(refers_to)
        );
    }

    for (idx, sheet) in metadata.sheets.iter().enumerate() {
        println!(
            "sheet[{idx}]: {} type={} visible={}",
            sheet.name,
            sheet_type_name(sheet.typ),
            visible_name(sheet.visible)
        );
        if let Some(sheet) = workbook.sheets.get(idx) {
            println!(
                "sheet_detail[{idx}].sheet_view: {}",
                sheet_view_summary(sheet.sheet_view())
            );
            println!(
                "sheet_detail[{idx}].tab_color: {}",
                optional_color_summary(sheet.tab_color())
            );
            println!(
                "sheet_detail[{idx}].print_options: {}",
                print_options_summary(sheet)
            );
            println!(
                "sheet_detail[{idx}].protection: {}",
                protection_summary(sheet)
            );
            println!(
                "sheet_detail[{idx}].row_outline: {}",
                row_outline_summary(sheet.row_outline_levels())
            );
            println!(
                "sheet_detail[{idx}].col_outline: {}",
                col_outline_summary(sheet.col_outline_levels())
            );
            println!(
                "sheet_detail[{idx}].collapsed_rows: {}",
                collapsed_rows_summary(sheet.collapsed_rows())
            );
            println!(
                "sheet_detail[{idx}].outline_summary: {}",
                outline_summary(sheet)
            );
        }
    }

    println!("metadata_merged_ranges: {}", counts.merged_ranges);
    println!("metadata_hyperlinks: {}", counts.hyperlinks);
    println!("metadata_comments: {}", counts.comments);
    println!("metadata_tables: {}", counts.tables);
    println!("metadata_data_validations: {}", counts.data_validations);
    println!(
        "metadata_conditional_formats: {}",
        counts.conditional_formats
    );
    println!("metadata_autofilters: {}", counts.autofilters);
    println!("metadata_page_setups: {}", counts.page_setups);
    println!("metadata_images: {}", counts.images);
    println!("metadata_charts: {}", counts.charts);
    println!("metadata_sheet_views: {}", counts.sheet_views);
    println!("metadata_tab_colors: {}", counts.tab_colors);
    println!("metadata_print_options: {}", counts.print_options);
    println!("metadata_sparklines: {}", counts.sparklines);

    ExitCode::SUCCESS
}

struct WorkbookR3MetadataCounts {
    merged_ranges: usize,
    hyperlinks: usize,
    comments: usize,
    tables: usize,
    data_validations: usize,
    conditional_formats: usize,
    autofilters: usize,
    page_setups: usize,
    images: usize,
    charts: usize,
    sheet_views: usize,
    tab_colors: usize,
    print_options: usize,
    sparklines: usize,
}

fn workbook_r3_metadata_counts(workbook: &Workbook) -> WorkbookR3MetadataCounts {
    WorkbookR3MetadataCounts {
        merged_ranges: workbook
            .sheets
            .iter()
            .map(|sheet| sheet.merged_ranges().len())
            .sum(),
        hyperlinks: workbook
            .sheets
            .iter()
            .map(|sheet| sheet.hyperlinks().len())
            .sum(),
        comments: workbook
            .sheets
            .iter()
            .map(|sheet| sheet.comments().len())
            .sum(),
        tables: workbook
            .sheets
            .iter()
            .map(|sheet| sheet.tables().len())
            .sum(),
        data_validations: workbook
            .sheets
            .iter()
            .map(|sheet| sheet.data_validations().len())
            .sum(),
        conditional_formats: workbook
            .sheets
            .iter()
            .map(|sheet| sheet.conditional_formats().len())
            .sum(),
        autofilters: workbook
            .sheets
            .iter()
            .filter(|sheet| sheet.autofilter_range().is_some())
            .count(),
        page_setups: workbook
            .sheets
            .iter()
            .filter(|sheet| sheet.page_setup().is_some())
            .count(),
        images: workbook
            .sheets
            .iter()
            .map(|sheet| sheet.images().len())
            .sum(),
        charts: workbook
            .sheets
            .iter()
            .map(|sheet| sheet.charts().len())
            .sum(),
        sheet_views: workbook
            .sheets
            .iter()
            .filter(|sheet| sheet.sheet_view() != SheetView::default())
            .count(),
        tab_colors: workbook
            .sheets
            .iter()
            .filter(|sheet| sheet.tab_color().is_some())
            .count(),
        print_options: workbook
            .sheets
            .iter()
            .filter(|sheet| sheet.print_gridlines() || sheet.print_headings())
            .count(),
        sparklines: workbook
            .sheets
            .iter()
            .map(|sheet| sheet.sparklines().len())
            .sum(),
    }
}

fn compare(left_path: &str, right_path: &str, limit: usize) -> ExitCode {
    let (left_bytes, left) = match read_workbook(left_path) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let (right_bytes, right) = match read_workbook(right_path) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };

    println!("rxls compare");
    println!("left: {left_path}");
    println!("right: {right_path}");
    println!("left_format: {}", detect_format(left_path, &left_bytes));
    println!("right_format: {}", detect_format(right_path, &right_bytes));
    println!("limit: {limit} differences");

    let mut comparison = Comparison::new(limit);
    compare_workbooks(&mut comparison, &left, &right);

    println!("differences: {}", comparison.differences);
    println!("truncated: {}", comparison.truncated);
    println!("equal: {}", comparison.differences == 0);

    if comparison.differences == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn corpus_report(manifest_path: &str, limit: usize) -> ExitCode {
    let manifest = match fs::read_to_string(manifest_path) {
        Ok(manifest) => manifest,
        Err(err) => {
            eprintln!("corpus-report {manifest_path}: read manifest: {err}");
            return ExitCode::from(66);
        }
    };
    let entries = match parse_corpus_manifest(&manifest) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("corpus-report {manifest_path}: {err}");
            return ExitCode::from(65);
        }
    };
    let manifest_dir = Path::new(manifest_path)
        .parent()
        .unwrap_or_else(|| Path::new("."));

    let mut by_ext = BTreeMap::<String, CorpusExtStats>::new();
    let mut by_failure_kind = BTreeMap::<String, usize>::new();
    let mut by_failure_decision = BTreeMap::<String, usize>::new();
    let mut by_failure_evidence = BTreeMap::<String, usize>::new();
    let mut failures = Vec::<CorpusFailure>::new();
    let mut eligible_files = 0usize;
    let mut opened = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;

    for entry in &entries {
        if entry.status.as_deref() == Some("failed") {
            skipped += 1;
            continue;
        }
        let Some(local_path) = entry.local_path.as_deref() else {
            skipped += 1;
            continue;
        };
        let ext = corpus_ext(&entry.path, local_path);
        if !matches!(ext.as_str(), ".ods" | ".xls" | ".xlsb" | ".xlsm" | ".xlsx") {
            skipped += 1;
            continue;
        }

        eligible_files += 1;
        let stats = by_ext.entry(ext.clone()).or_default();
        stats.files += 1;

        let workbook_path = corpus_local_path(manifest_dir, local_path);
        let label = corpus_label(entry, local_path);
        match fs::read(&workbook_path) {
            Ok(bytes) => match Workbook::open(&bytes) {
                Ok(_) => {
                    opened += 1;
                    stats.opened += 1;
                }
                Err(err) => {
                    failed += 1;
                    stats.failed += 1;
                    let kind = corpus_failure_kind(&err);
                    let decision = corpus_failure_decision(kind);
                    let container = corpus_container_kind(&bytes);
                    let extension_mismatch = corpus_extension_mismatch(&ext, container);
                    let evidence = corpus_failure_evidence(kind, container, extension_mismatch);
                    *by_failure_kind.entry(kind.to_string()).or_default() += 1;
                    *by_failure_decision.entry(decision.to_string()).or_default() += 1;
                    *by_failure_evidence.entry(evidence.to_string()).or_default() += 1;
                    failures.push(CorpusFailure {
                        ext,
                        label,
                        kind: kind.to_string(),
                        decision: decision.to_string(),
                        evidence: evidence.to_string(),
                        container: container.to_string(),
                        extension_mismatch,
                        error: format!("parse: {err}"),
                    });
                }
            },
            Err(err) => {
                failed += 1;
                stats.failed += 1;
                let kind = "read_error";
                let decision = corpus_failure_decision(kind);
                let evidence = corpus_failure_evidence(kind, "unreadable", false);
                *by_failure_kind.entry("read_error".to_string()).or_default() += 1;
                *by_failure_decision.entry(decision.to_string()).or_default() += 1;
                *by_failure_evidence.entry(evidence.to_string()).or_default() += 1;
                failures.push(CorpusFailure {
                    ext,
                    label,
                    kind: kind.to_string(),
                    decision: decision.to_string(),
                    evidence: evidence.to_string(),
                    container: "unreadable".to_string(),
                    extension_mismatch: false,
                    error: format!("read: {err}"),
                });
            }
        }
    }

    println!("rxls corpus-report");
    println!("manifest: {manifest_path}");
    println!("manifest_files: {}", entries.len());
    println!("eligible_files: {eligible_files}");
    println!("opened: {opened}");
    println!("failed: {failed}");
    println!("skipped: {skipped}");
    println!("limit: {limit} failures");
    for (ext, stats) in by_ext {
        println!(
            "by_ext: {ext} files={} opened={} failed={}",
            stats.files, stats.opened, stats.failed
        );
    }
    for (kind, count) in by_failure_kind {
        println!("by_failure_kind: {kind} failed={count}");
    }
    for (decision, count) in by_failure_decision {
        println!("by_failure_decision: {decision} failed={count}");
    }
    for (evidence, count) in by_failure_evidence {
        println!("by_failure_evidence: {evidence} failed={count}");
    }
    for failure in failures.iter().take(limit) {
        println!(
            "failure: {} {} kind={} decision={} evidence={} container={} extension_mismatch={} {}",
            failure.ext,
            escape_dump_text(&failure.label),
            failure.kind,
            failure.decision,
            failure.evidence,
            failure.container,
            failure.extension_mismatch,
            escape_dump_text(&failure.error)
        );
    }
    println!("truncated: {}", failures.len() > limit);

    ExitCode::SUCCESS
}

fn fixture_report(manifest_path: &str, limit: usize) -> ExitCode {
    let manifest = match fs::read_to_string(manifest_path) {
        Ok(manifest) => manifest,
        Err(err) => {
            eprintln!("fixture-report {manifest_path}: read manifest: {err}");
            return ExitCode::from(66);
        }
    };
    let entries = match parse_fixture_manifest(&manifest) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("fixture-report {manifest_path}: {err}");
            return ExitCode::from(65);
        }
    };
    let manifest_dir = Path::new(manifest_path)
        .parent()
        .unwrap_or_else(|| Path::new("."));

    let mut by_format = BTreeMap::<String, FixtureFormatStats>::new();
    let mut coverage_tags = BTreeSet::<String>::new();
    let mut rows = Vec::<FixtureReportRow>::new();
    let mut opened = 0usize;
    let mut hash_ok = 0usize;
    let mut oracle_entries = 0usize;
    let mut oracle_ok = 0usize;
    let mut oracle_failed = 0usize;
    let mut failed = 0usize;

    for entry in &entries {
        for tag in &entry.covers {
            coverage_tags.insert(tag.clone());
        }

        let stats = by_format.entry(entry.format.clone()).or_default();
        stats.files += 1;

        let fixture_path = manifest_relative_path(manifest_dir, &entry.path);
        let mut row_hash_ok = false;
        let mut row_open_ok = false;
        let mut row_oracle_status = FixtureOracleStatus::Missing;
        let mut actual_oracle = None;
        let mut errors = Vec::<String>::new();
        if entry.oracle.is_some() {
            oracle_entries += 1;
        }

        match fs::read(&fixture_path) {
            Ok(bytes) => {
                let actual_hash = sha256_hex(&bytes);
                row_hash_ok = actual_hash.eq_ignore_ascii_case(&entry.sha256);
                if row_hash_ok {
                    hash_ok += 1;
                    stats.hash_ok += 1;
                } else {
                    errors.push(format!("hash={actual_hash} expected={}", entry.sha256));
                }

                match Workbook::open(&bytes) {
                    Ok(workbook) => {
                        row_open_ok = true;
                        opened += 1;
                        stats.opened += 1;
                        if let Some(expected_oracle) = entry.oracle.as_ref() {
                            let actual = fixture_actual_oracle(&workbook);
                            if &actual == expected_oracle {
                                oracle_ok += 1;
                                stats.oracle_ok += 1;
                                row_oracle_status = FixtureOracleStatus::Ok;
                            } else {
                                oracle_failed += 1;
                                row_oracle_status = FixtureOracleStatus::Mismatch;
                                errors.push(format!(
                                    "oracle: expected {} actual {}",
                                    expected_oracle.summary(),
                                    actual.summary()
                                ));
                            }
                            actual_oracle = Some(actual);
                        }
                    }
                    Err(err) => {
                        if entry.oracle.is_some() {
                            oracle_failed += 1;
                            row_oracle_status = FixtureOracleStatus::Skipped;
                        }
                        errors.push(format!("parse: {err}"));
                    }
                }
            }
            Err(err) => {
                if entry.oracle.is_some() {
                    oracle_failed += 1;
                    row_oracle_status = FixtureOracleStatus::Skipped;
                }
                errors.push(format!("read: {err}"));
            }
        }

        if !(row_hash_ok && row_open_ok && row_oracle_status.is_ok_or_missing()) {
            failed += 1;
            stats.failed += 1;
        }

        rows.push(FixtureReportRow {
            path: entry.path.clone(),
            format: entry.format.clone(),
            covers: entry.covers.clone(),
            hash_ok: row_hash_ok,
            open_ok: row_open_ok,
            oracle_status: row_oracle_status,
            actual_oracle,
            error: if errors.is_empty() {
                None
            } else {
                Some(errors.join("; "))
            },
        });
    }

    println!("rxls fixture-report");
    println!("manifest: {manifest_path}");
    println!("manifest_entries: {}", entries.len());
    println!("opened: {opened}");
    println!("hash_ok: {hash_ok}");
    println!("oracle_entries: {oracle_entries}");
    println!("oracle_ok: {oracle_ok}");
    println!("oracle_failed: {oracle_failed}");
    println!("failed: {failed}");
    println!("coverage_tags: {}", coverage_tags.len());
    println!("limit: {limit} fixtures");
    for (format, stats) in by_format {
        println!(
            "by_format: {format} files={} opened={} hash_ok={} oracle_ok={} failed={}",
            stats.files, stats.opened, stats.hash_ok, stats.oracle_ok, stats.failed
        );
    }
    for row in rows.iter().take(limit) {
        let hash = if row.hash_ok { "ok" } else { "mismatch" };
        let open = if row.open_ok { "ok" } else { "failed" };
        let covers = if row.covers.is_empty() {
            "none".to_string()
        } else {
            row.covers.join(",")
        };
        print!(
            "fixture: {} {} hash={hash} open={open} oracle={} covers={}",
            row.format,
            escape_dump_text(&row.path),
            row.oracle_status.as_str(),
            escape_dump_text(&covers)
        );
        if let Some(error) = row.error.as_deref() {
            print!(" error={}", escape_dump_text(error));
        }
        println!();
    }
    for row in rows.iter().take(limit) {
        if let Some(oracle) = row.actual_oracle.as_ref() {
            println!(
                "oracle: {} {} {}",
                row.format,
                escape_dump_text(&row.path),
                oracle.summary()
            );
        }
    }
    println!("truncated: {}", rows.len() > limit);

    ExitCode::SUCCESS
}

fn corpus_failure_kind(err: &Error) -> &'static str {
    match err {
        Error::NotOle2 => "not_ole2",
        Error::LegacyBiff => "legacy_biff",
        Error::Cfb(_) => "cfb_io",
        Error::InvalidCfb(_) => "invalid_cfb",
        Error::MissingWorkbook => "missing_workbook",
        Error::Biff(_) => "malformed_biff",
        Error::Zip(_) => "invalid_zip",
        Error::Xml(_) => "malformed_xml",
        Error::Encrypted => "unsupported_encrypted_workbook",
        Error::EncryptedPackage => "unsupported_encrypted_ooxml",
        Error::EncryptedOpenDocument => "unsupported_encrypted_opendocument",
        Error::NoText => "no_text",
        Error::SheetOutOfRange => "sheet_out_of_range",
    }
}

fn corpus_failure_decision(kind: &str) -> &'static str {
    match kind {
        "invalid_cfb" | "invalid_zip" | "not_ole2" => "excluded_malformed_container",
        "legacy_biff" => "unsupported_legacy_biff",
        "unsupported_encrypted_workbook"
        | "unsupported_encrypted_ooxml"
        | "unsupported_encrypted_opendocument" => "unsupported_encrypted",
        "read_error" => "needs_io_triage",
        _ => "needs_parser_triage",
    }
}

fn corpus_container_kind(bytes: &[u8]) -> &'static str {
    const OLE2_MAGIC: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    if bytes.starts_with(OLE2_MAGIC) {
        "ole2"
    } else if bytes.starts_with(b"PK") {
        "zip"
    } else {
        "unknown"
    }
}

fn corpus_extension_mismatch(ext: &str, container: &str) -> bool {
    let expected = match ext {
        ".xls" => "ole2",
        ".ods" | ".xlsb" | ".xlsm" | ".xlsx" => "zip",
        _ => "unknown",
    };
    expected != "unknown" && container != expected
}

fn corpus_failure_evidence(kind: &str, container: &str, extension_mismatch: bool) -> &'static str {
    match (kind, container, extension_mismatch) {
        ("invalid_zip", "zip", true) => "zip_signature_misleading_extension",
        ("invalid_zip", "zip", false) => "zip_signature_corrupt_container",
        ("invalid_cfb", "ole2", _) => "ole2_signature_corrupt_container",
        ("not_ole2", _, _) => "signature_mismatch_or_unknown_container",
        ("read_error", _, _) => "read_error",
        ("legacy_biff", "ole2", _) => "ole2_legacy_biff_stream",
        ("unsupported_encrypted_workbook", "ole2", _) => "ole2_encrypted_workbook",
        ("unsupported_encrypted_ooxml", _, _) => "encrypted_ooxml_package",
        ("unsupported_encrypted_opendocument", "zip", _) => "encrypted_opendocument_package",
        _ => "parser_or_support_classification",
    }
}

fn compare_workbooks(comparison: &mut Comparison, left: &Workbook, right: &Workbook) {
    let left_metadata = left.metadata();
    let right_metadata = right.metadata();

    if left_metadata.date1904 != right_metadata.date1904 {
        comparison.difference(format!(
            "workbook.date1904 left={} right={}",
            left_metadata.date1904, right_metadata.date1904
        ));
    }
    if left_metadata.text_truncated != right_metadata.text_truncated {
        comparison.difference(format!(
            "workbook.partial left={} right={}",
            left_metadata.text_truncated, right_metadata.text_truncated
        ));
    }
    if left_metadata.structure_protected != right_metadata.structure_protected {
        comparison.difference(format!(
            "workbook.structure_protected left={} right={}",
            left_metadata.structure_protected, right_metadata.structure_protected
        ));
    }
    if left_metadata.active_sheet != right_metadata.active_sheet {
        comparison.difference(format!(
            "workbook.active_sheet left={} right={}",
            optional_usize(left_metadata.active_sheet),
            optional_usize(right_metadata.active_sheet)
        ));
    }
    if left_metadata.active_sheet_name != right_metadata.active_sheet_name {
        comparison.difference(format!(
            "workbook.active_sheet_name left={} right={}",
            optional_text(left_metadata.active_sheet_name),
            optional_text(right_metadata.active_sheet_name)
        ));
    }
    compare_document_properties(
        comparison,
        left_metadata.properties,
        right_metadata.properties,
    );
    compare_defined_names(
        comparison,
        left_metadata.defined_names,
        right_metadata.defined_names,
    );
    if left_metadata.sheets.len() != right_metadata.sheets.len() {
        comparison.difference(format!(
            "workbook.sheets left={} right={}",
            left_metadata.sheets.len(),
            right_metadata.sheets.len()
        ));
    }

    let sheet_count = left_metadata.sheets.len().max(right_metadata.sheets.len());
    for index in 0..sheet_count {
        match (
            left_metadata.sheets.get(index),
            right_metadata.sheets.get(index),
        ) {
            (Some(left_sheet), Some(right_sheet)) => {
                if left_sheet.name != right_sheet.name {
                    comparison.difference(format!(
                        "sheet[{index}].name left={} right={}",
                        escape_dump_text(&left_sheet.name),
                        escape_dump_text(&right_sheet.name)
                    ));
                }
                if left_sheet.typ != right_sheet.typ {
                    comparison.difference(format!(
                        "sheet[{index}].type left={} right={}",
                        sheet_type_name(left_sheet.typ),
                        sheet_type_name(right_sheet.typ)
                    ));
                }
                if left_sheet.visible != right_sheet.visible {
                    comparison.difference(format!(
                        "sheet[{index}].visible left={} right={}",
                        visible_name(left_sheet.visible),
                        visible_name(right_sheet.visible)
                    ));
                }
            }
            (Some(left_sheet), None) => comparison.difference(format!(
                "sheet[{index}] left=present:{} right=missing",
                escape_dump_text(&left_sheet.name)
            )),
            (None, Some(right_sheet)) => comparison.difference(format!(
                "sheet[{index}] left=missing right=present:{}",
                escape_dump_text(&right_sheet.name)
            )),
            (None, None) => {}
        }
    }

    for index in 0..left.sheets.len().min(right.sheets.len()) {
        compare_sheet_metadata(comparison, index, &left.sheets[index], &right.sheets[index]);
        compare_sheet_cells(comparison, index, &left.sheets[index], &right.sheets[index]);
    }
}

fn optional_usize(value: Option<usize>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "none".to_string())
}

fn optional_text(value: Option<&str>) -> String {
    value
        .map(escape_dump_text)
        .unwrap_or_else(|| "none".to_string())
}

fn compare_document_properties(
    comparison: &mut Comparison,
    left: &DocProperties,
    right: &DocProperties,
) {
    compare_document_property(
        comparison,
        "title",
        left.title.as_deref(),
        right.title.as_deref(),
    );
    compare_document_property(
        comparison,
        "subject",
        left.subject.as_deref(),
        right.subject.as_deref(),
    );
    compare_document_property(
        comparison,
        "creator",
        left.creator.as_deref(),
        right.creator.as_deref(),
    );
    compare_document_property(
        comparison,
        "keywords",
        left.keywords.as_deref(),
        right.keywords.as_deref(),
    );
    compare_document_property(
        comparison,
        "description",
        left.description.as_deref(),
        right.description.as_deref(),
    );
    compare_document_property(
        comparison,
        "last_modified_by",
        left.last_modified_by.as_deref(),
        right.last_modified_by.as_deref(),
    );
    compare_document_property(
        comparison,
        "company",
        left.company.as_deref(),
        right.company.as_deref(),
    );
    compare_document_property(
        comparison,
        "created",
        left.created.as_deref(),
        right.created.as_deref(),
    );
}

fn compare_document_property(
    comparison: &mut Comparison,
    field: &str,
    left: Option<&str>,
    right: Option<&str>,
) {
    if left != right {
        comparison.difference(format!(
            "workbook.property.{field} left={} right={}",
            optional_text(left),
            optional_text(right)
        ));
    }
}

fn compare_defined_names(
    comparison: &mut Comparison,
    left: &[(String, String)],
    right: &[(String, String)],
) {
    if left.len() != right.len() {
        comparison.difference(format!(
            "workbook.defined_names left={} right={}",
            left.len(),
            right.len()
        ));
    }
    for index in 0..left.len().max(right.len()) {
        let left = left
            .get(index)
            .map(defined_name_summary)
            .unwrap_or_else(|| "missing".to_string());
        let right = right
            .get(index)
            .map(defined_name_summary)
            .unwrap_or_else(|| "missing".to_string());
        if left != right {
            comparison.difference(format!(
                "workbook.defined_name[{index}] left={left} right={right}"
            ));
        }
    }
}

fn defined_name_summary((name, refers_to): &(String, String)) -> String {
    format!("{}={}", escape_dump_text(name), escape_dump_text(refers_to))
}

fn compare_sheet_metadata(
    comparison: &mut Comparison,
    sheet_index: usize,
    left: &rxls::Sheet,
    right: &rxls::Sheet,
) {
    compare_count(
        comparison,
        sheet_index,
        "merged_ranges",
        left.merged_ranges().len(),
        right.merged_ranges().len(),
    );
    compare_count(
        comparison,
        sheet_index,
        "hyperlinks",
        left.hyperlinks().len(),
        right.hyperlinks().len(),
    );
    compare_count(
        comparison,
        sheet_index,
        "comments",
        left.comments().len(),
        right.comments().len(),
    );
    compare_count(
        comparison,
        sheet_index,
        "tables",
        left.tables().len(),
        right.tables().len(),
    );
    compare_count(
        comparison,
        sheet_index,
        "data_validations",
        left.data_validations().len(),
        right.data_validations().len(),
    );
    compare_count(
        comparison,
        sheet_index,
        "conditional_formats",
        left.conditional_formats().len(),
        right.conditional_formats().len(),
    );
    compare_count(
        comparison,
        sheet_index,
        "images",
        left.images().len(),
        right.images().len(),
    );
    compare_count(
        comparison,
        sheet_index,
        "charts",
        left.charts().len(),
        right.charts().len(),
    );
    compare_count(
        comparison,
        sheet_index,
        "sparklines",
        left.sparklines().len(),
        right.sparklines().len(),
    );
    compare_bool(
        comparison,
        sheet_index,
        "autofilter",
        left.autofilter_range().is_some(),
        right.autofilter_range().is_some(),
    );
    compare_bool(
        comparison,
        sheet_index,
        "page_setup",
        left.page_setup().is_some(),
        right.page_setup().is_some(),
    );
    compare_metadata_entries(
        comparison,
        sheet_index,
        "merged_range",
        merged_range_summaries(left),
        merged_range_summaries(right),
    );
    compare_metadata_entries(
        comparison,
        sheet_index,
        "hyperlink",
        hyperlink_summaries(left),
        hyperlink_summaries(right),
    );
    compare_metadata_entries(
        comparison,
        sheet_index,
        "comment",
        comment_summaries(left),
        comment_summaries(right),
    );
    compare_metadata_entries(
        comparison,
        sheet_index,
        "table",
        table_summaries(left),
        table_summaries(right),
    );
    compare_metadata_entries(
        comparison,
        sheet_index,
        "data_validation",
        data_validation_summaries(left),
        data_validation_summaries(right),
    );
    compare_metadata_entries(
        comparison,
        sheet_index,
        "conditional_format",
        conditional_format_summaries(left),
        conditional_format_summaries(right),
    );
    compare_metadata_entries(
        comparison,
        sheet_index,
        "autofilter",
        optional_range_summary(left.autofilter_range()),
        optional_range_summary(right.autofilter_range()),
    );
    compare_metadata_entries(
        comparison,
        sheet_index,
        "page_setup",
        optional_page_setup_summary(left.page_setup()),
        optional_page_setup_summary(right.page_setup()),
    );
    compare_sheet_view(
        comparison,
        sheet_index,
        left.sheet_view(),
        right.sheet_view(),
    );
    compare_tab_color(comparison, sheet_index, left.tab_color(), right.tab_color());
    compare_print_options(comparison, sheet_index, left, right);
    compare_protection(comparison, sheet_index, left, right);
    compare_outline(comparison, sheet_index, left, right);
    compare_metadata_entries(
        comparison,
        sheet_index,
        "image",
        image_summaries(left),
        image_summaries(right),
    );
    compare_metadata_entries(
        comparison,
        sheet_index,
        "chart",
        chart_summaries(left),
        chart_summaries(right),
    );
    compare_metadata_entries(
        comparison,
        sheet_index,
        "sparkline",
        sparkline_summaries(left),
        sparkline_summaries(right),
    );
}

fn compare_count(
    comparison: &mut Comparison,
    sheet_index: usize,
    field: &str,
    left: usize,
    right: usize,
) {
    if left != right {
        comparison.difference(format!(
            "sheet[{sheet_index}].{field} left={left} right={right}"
        ));
    }
}

fn compare_bool(
    comparison: &mut Comparison,
    sheet_index: usize,
    field: &str,
    left: bool,
    right: bool,
) {
    if left != right {
        comparison.difference(format!(
            "sheet[{sheet_index}].{field} left={left} right={right}"
        ));
    }
}

fn compare_metadata_entries(
    comparison: &mut Comparison,
    sheet_index: usize,
    field: &str,
    left: Vec<String>,
    right: Vec<String>,
) {
    for index in 0..left.len().max(right.len()) {
        let left = left.get(index).map(String::as_str).unwrap_or("missing");
        let right = right.get(index).map(String::as_str).unwrap_or("missing");
        if left != right {
            comparison.difference(format!(
                "sheet[{sheet_index}].{field}[{index}] left={left} right={right}"
            ));
        }
    }
}

fn compare_sheet_view(
    comparison: &mut Comparison,
    sheet_index: usize,
    left: SheetView,
    right: SheetView,
) {
    if left != right {
        comparison.difference(format!(
            "sheet[{sheet_index}].sheet_view left={} right={}",
            sheet_view_summary(left),
            sheet_view_summary(right)
        ));
    }
}

fn compare_tab_color(
    comparison: &mut Comparison,
    sheet_index: usize,
    left: Option<rxls::Color>,
    right: Option<rxls::Color>,
) {
    if left != right {
        comparison.difference(format!(
            "sheet[{sheet_index}].tab_color left={} right={}",
            optional_color_summary(left),
            optional_color_summary(right)
        ));
    }
}

fn compare_print_options(
    comparison: &mut Comparison,
    sheet_index: usize,
    left: &rxls::Sheet,
    right: &rxls::Sheet,
) {
    if left.print_gridlines() != right.print_gridlines()
        || left.print_headings() != right.print_headings()
    {
        comparison.difference(format!(
            "sheet[{sheet_index}].print_options left={} right={}",
            print_options_summary(left),
            print_options_summary(right)
        ));
    }
}

fn compare_protection(
    comparison: &mut Comparison,
    sheet_index: usize,
    left: &rxls::Sheet,
    right: &rxls::Sheet,
) {
    if left.is_protected() != right.is_protected()
        || left.protection_options() != right.protection_options()
    {
        comparison.difference(format!(
            "sheet[{sheet_index}].protection left={} right={}",
            protection_summary(left),
            protection_summary(right)
        ));
    }
}

fn compare_outline(
    comparison: &mut Comparison,
    sheet_index: usize,
    left: &rxls::Sheet,
    right: &rxls::Sheet,
) {
    if left.row_outline_levels() != right.row_outline_levels() {
        comparison.difference(format!(
            "sheet[{sheet_index}].row_outline left={} right={}",
            row_outline_summary(left.row_outline_levels()),
            row_outline_summary(right.row_outline_levels())
        ));
    }
    if left.col_outline_levels() != right.col_outline_levels() {
        comparison.difference(format!(
            "sheet[{sheet_index}].col_outline left={} right={}",
            col_outline_summary(left.col_outline_levels()),
            col_outline_summary(right.col_outline_levels())
        ));
    }
    if left.collapsed_rows() != right.collapsed_rows() {
        comparison.difference(format!(
            "sheet[{sheet_index}].collapsed_rows left={} right={}",
            collapsed_rows_summary(left.collapsed_rows()),
            collapsed_rows_summary(right.collapsed_rows())
        ));
    }
    if left.outline_summary_below() != right.outline_summary_below()
        || left.outline_summary_right() != right.outline_summary_right()
    {
        comparison.difference(format!(
            "sheet[{sheet_index}].outline_summary left={} right={}",
            outline_summary(left),
            outline_summary(right)
        ));
    }
}

fn merged_range_summaries(sheet: &rxls::Sheet) -> Vec<String> {
    sheet
        .merged_ranges()
        .iter()
        .map(|&range| format_dimensions(range))
        .collect()
}

fn hyperlink_summaries(sheet: &rxls::Sheet) -> Vec<String> {
    sheet
        .hyperlinks()
        .iter()
        .map(|(row, col, url)| format!("{}:{}", a1(*row, *col), escape_dump_text(url)))
        .collect()
}

fn comment_summaries(sheet: &rxls::Sheet) -> Vec<String> {
    sheet
        .comments()
        .iter()
        .map(|comment| {
            format!(
                "{}:{}:{}",
                a1(comment.row, comment.col),
                escape_dump_text(comment.author.as_deref().unwrap_or("")),
                escape_dump_text(&comment.text)
            )
        })
        .collect()
}

fn table_summaries(sheet: &rxls::Sheet) -> Vec<String> {
    sheet
        .tables()
        .iter()
        .map(|table| {
            format!(
                "{} {} [{}] style={}",
                escape_dump_text(&table.name),
                format_dimensions(table.range),
                table
                    .columns
                    .iter()
                    .map(|column| escape_dump_text(column))
                    .collect::<Vec<_>>()
                    .join(","),
                escape_dump_text(table.style.as_deref().unwrap_or(""))
            )
        })
        .collect()
}

fn data_validation_summaries(sheet: &rxls::Sheet) -> Vec<String> {
    sheet
        .data_validations()
        .iter()
        .map(|dv| {
            let mut summary = format!(
                "{} {} {} {}",
                format_dimensions(dv.sqref),
                dv_kind_name(dv.kind),
                dv_op_name(dv.operator),
                escape_dump_text(&dv.formula1)
            );
            if let Some(formula2) = dv.formula2.as_deref() {
                summary.push(' ');
                summary.push_str(&escape_dump_text(formula2));
            }
            summary.push_str(&format!(
                " allow_blank={} show_input={} show_error={} prompt={} error={}",
                dv.allow_blank,
                dv.show_input_message,
                dv.show_error_message,
                validation_message_summary(dv.prompt.as_ref()),
                validation_message_summary(dv.error.as_ref())
            ));
            summary
        })
        .collect()
}

fn validation_message_summary(value: Option<&(String, String)>) -> String {
    value
        .map(|(title, message)| {
            format!("{}:{}", escape_dump_text(title), escape_dump_text(message))
        })
        .unwrap_or_default()
}

fn conditional_format_summaries(sheet: &rxls::Sheet) -> Vec<String> {
    sheet
        .conditional_formats()
        .iter()
        .map(|cf| {
            format!(
                "{} {}",
                format_dimensions(cf.sqref),
                cf_rule_summary(&cf.rule)
            )
        })
        .collect()
}

fn optional_range_summary(range: Option<(u32, u16, u32, u16)>) -> Vec<String> {
    range.map(format_dimensions).into_iter().collect()
}

fn optional_page_setup_summary(page_setup: Option<&rxls::PageSetup>) -> Vec<String> {
    page_setup.map(page_setup_summary).into_iter().collect()
}

fn sheet_view_summary(view: SheetView) -> String {
    format!(
        "freeze={} hide_gridlines={} zoom={} show_headers={} right_to_left={}",
        view.freeze
            .map(|(row, col)| format!("{row},{col}"))
            .unwrap_or_default(),
        view.hide_gridlines,
        view.zoom.map(|value| value.to_string()).unwrap_or_default(),
        view.show_headers
            .map(|value| value.to_string())
            .unwrap_or_else(|| "none".to_string()),
        view.right_to_left
    )
}

fn optional_color_summary(color: Option<rxls::Color>) -> String {
    color
        .map(color_summary)
        .unwrap_or_else(|| "none".to_string())
}

fn print_options_summary(sheet: &rxls::Sheet) -> String {
    format!(
        "gridlines={} headings={}",
        sheet.print_gridlines(),
        sheet.print_headings()
    )
}

fn protection_summary(sheet: &rxls::Sheet) -> String {
    format!(
        "protected={} options={}",
        sheet.is_protected(),
        sheet
            .protection_options()
            .map(protection_options_summary)
            .unwrap_or_else(|| "none".to_string())
    )
}

fn protection_options_summary(options: rxls::ProtectionOptions) -> String {
    let mut names = Vec::new();
    if options.sort {
        names.push("sort");
    }
    if options.auto_filter {
        names.push("auto_filter");
    }
    if options.format_cells {
        names.push("format_cells");
    }
    if options.format_columns {
        names.push("format_columns");
    }
    if options.format_rows {
        names.push("format_rows");
    }
    if options.insert_columns {
        names.push("insert_columns");
    }
    if options.insert_rows {
        names.push("insert_rows");
    }
    if options.insert_hyperlinks {
        names.push("insert_hyperlinks");
    }
    if options.delete_columns {
        names.push("delete_columns");
    }
    if options.delete_rows {
        names.push("delete_rows");
    }
    if options.pivot_tables {
        names.push("pivot_tables");
    }
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(",")
    }
}

fn row_outline_summary(levels: &BTreeMap<u32, u8>) -> String {
    if levels.is_empty() {
        return "none".to_string();
    }
    levels
        .iter()
        .map(|(row, level)| format!("{}:{level}", row + 1))
        .collect::<Vec<_>>()
        .join(",")
}

fn col_outline_summary(levels: &BTreeMap<u16, u8>) -> String {
    if levels.is_empty() {
        return "none".to_string();
    }
    levels
        .iter()
        .map(|(col, level)| format!("{}:{level}", column_label(*col)))
        .collect::<Vec<_>>()
        .join(",")
}

fn collapsed_rows_summary(rows: &BTreeSet<u32>) -> String {
    if rows.is_empty() {
        return "none".to_string();
    }
    rows.iter()
        .map(|row| (row + 1).to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn outline_summary(sheet: &rxls::Sheet) -> String {
    format!(
        "below={},right={}",
        sheet.outline_summary_below(),
        sheet.outline_summary_right()
    )
}

fn image_summaries(sheet: &rxls::Sheet) -> Vec<String> {
    sheet
        .images()
        .iter()
        .map(|image| {
            let to = image
                .to
                .map(|(row, col)| a1(row, col))
                .unwrap_or_else(|| "auto".to_string());
            format!(
                "{}:{}->{} bytes={} digest={}",
                image_format_name(image.format),
                a1(image.from.0, image.from.1),
                to,
                image.data.len(),
                image_data_digest(&image.data)
            )
        })
        .collect()
}

fn image_data_digest(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn chart_summaries(sheet: &rxls::Sheet) -> Vec<String> {
    sheet
        .charts()
        .iter()
        .map(|chart| {
            let series = chart
                .series
                .iter()
                .map(chart_series_summary)
                .collect::<Vec<_>>()
                .join(";");
            format!(
                "{} title={} x_axis={} y_axis={} series=[{}] legend={} labels={} anchor={}->{}",
                chart_kind_name(chart.kind),
                chart
                    .title
                    .as_deref()
                    .map(escape_dump_text)
                    .unwrap_or_default(),
                chart
                    .x_axis_title
                    .as_deref()
                    .map(escape_dump_text)
                    .unwrap_or_default(),
                chart
                    .y_axis_title
                    .as_deref()
                    .map(escape_dump_text)
                    .unwrap_or_default(),
                series,
                chart.legend,
                chart.data_labels,
                a1(chart.from.0, chart.from.1),
                a1(chart.to.0, chart.to.1)
            )
        })
        .collect()
}

fn chart_series_summary(series: &rxls::Series) -> String {
    format!(
        "name={} categories={} values={} bubble={}",
        series
            .name
            .as_deref()
            .map(escape_dump_text)
            .unwrap_or_default(),
        series
            .categories
            .as_deref()
            .map(escape_dump_text)
            .unwrap_or_default(),
        escape_dump_text(&series.values),
        series
            .bubble_sizes
            .as_deref()
            .map(escape_dump_text)
            .unwrap_or_default()
    )
}

fn sparkline_summaries(sheet: &rxls::Sheet) -> Vec<String> {
    sheet
        .sparklines()
        .iter()
        .map(|sparkline| {
            format!(
                "{} {} {}",
                a1(sparkline.location.0, sparkline.location.1),
                sparkline_kind_name(sparkline.kind),
                escape_dump_text(&sparkline.range)
            )
        })
        .collect()
}

fn dv_kind_name(kind: rxls::DvKind) -> &'static str {
    match kind {
        rxls::DvKind::List => "list",
        rxls::DvKind::Whole => "whole",
        rxls::DvKind::Decimal => "decimal",
        rxls::DvKind::Date => "date",
        rxls::DvKind::Time => "time",
        rxls::DvKind::TextLength => "text-length",
        rxls::DvKind::Custom => "custom",
    }
}

fn dv_op_name(op: rxls::DvOp) -> &'static str {
    match op {
        rxls::DvOp::Between => "between",
        rxls::DvOp::NotBetween => "not-between",
        rxls::DvOp::Equal => "equal",
        rxls::DvOp::NotEqual => "not-equal",
        rxls::DvOp::GreaterThan => "greater-than",
        rxls::DvOp::LessThan => "less-than",
        rxls::DvOp::GreaterThanOrEqual => "greater-than-or-equal",
        rxls::DvOp::LessThanOrEqual => "less-than-or-equal",
    }
}

fn cf_rule_summary(rule: &rxls::CfRule) -> String {
    match rule {
        rxls::CfRule::CellIs {
            op,
            formula1,
            formula2,
            fill,
        } => format!(
            "cell-is {} {} {} fill={}",
            dv_op_name(*op),
            escape_dump_text(formula1),
            formula2
                .as_deref()
                .map(escape_dump_text)
                .unwrap_or_default(),
            color_summary(*fill)
        ),
        rxls::CfRule::ColorScale2 { min, max } => {
            format!(
                "color-scale-2 {} {}",
                color_summary(*min),
                color_summary(*max)
            )
        }
        rxls::CfRule::ColorScale3 { min, mid, max } => format!(
            "color-scale-3 {} {} {}",
            color_summary(*min),
            color_summary(*mid),
            color_summary(*max)
        ),
        rxls::CfRule::DataBar { color } => format!("data-bar {}", color_summary(*color)),
        rxls::CfRule::TopBottom {
            rank,
            bottom,
            percent,
            fill,
        } => format!(
            "top-bottom rank={rank} bottom={bottom} percent={percent} fill={}",
            color_summary(*fill)
        ),
        rxls::CfRule::AboveAverage { below, fill } => {
            format!("above-average below={below} fill={}", color_summary(*fill))
        }
        rxls::CfRule::DuplicateValues { unique, fill } => {
            format!(
                "duplicate-values unique={unique} fill={}",
                color_summary(*fill)
            )
        }
        rxls::CfRule::Expression { formula, fill } => format!(
            "expression {} fill={}",
            escape_dump_text(formula),
            color_summary(*fill)
        ),
    }
}

fn page_setup_summary(page_setup: &rxls::PageSetup) -> String {
    format!(
        "landscape={} margins={} print_area={} repeat_rows={} repeat_cols={} fit={}x{} scale={} first_page={} centered={}x{} paper={} header={} footer={}",
        page_setup.landscape,
        page_margins_summary(page_setup.margins),
        page_setup
            .print_area
            .map(format_dimensions)
            .unwrap_or_default(),
        page_setup
            .repeat_rows
            .map(|(first, last)| format!("{}:{}", first + 1, last + 1))
            .unwrap_or_default(),
        page_setup
            .repeat_cols
            .map(|(first, last)| format!("{}:{}", column_label(first), column_label(last)))
            .unwrap_or_default(),
        page_setup
            .fit_to_width
            .map(|value| value.to_string())
            .unwrap_or_default(),
        page_setup
            .fit_to_height
            .map(|value| value.to_string())
            .unwrap_or_default(),
        page_setup
            .scale
            .map(|value| value.to_string())
            .unwrap_or_default(),
        page_setup
            .first_page_number
            .map(|value| value.to_string())
            .unwrap_or_default(),
        page_setup.center_horizontally,
        page_setup.center_vertically,
        page_setup
            .paper_size
            .map(|value| value.to_string())
            .unwrap_or_default(),
        page_setup
            .header
            .as_deref()
            .map(escape_dump_text)
            .unwrap_or_default(),
        page_setup
            .footer
            .as_deref()
            .map(escape_dump_text)
            .unwrap_or_default()
    )
}

fn page_margins_summary(margins: Option<(f64, f64, f64, f64, f64, f64)>) -> String {
    margins
        .map(|(left, right, top, bottom, header, footer)| {
            [left, right, top, bottom, header, footer]
                .into_iter()
                .map(number_text)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default()
}

fn image_format_name(format: rxls::ImageFmt) -> &'static str {
    match format {
        rxls::ImageFmt::Png => "png",
        rxls::ImageFmt::Jpeg => "jpeg",
    }
}

fn chart_kind_name(kind: rxls::ChartKind) -> &'static str {
    match kind {
        rxls::ChartKind::Bar => "bar",
        rxls::ChartKind::Line => "line",
        rxls::ChartKind::Pie => "pie",
        rxls::ChartKind::Scatter => "scatter",
        rxls::ChartKind::Area => "area",
        rxls::ChartKind::Doughnut => "doughnut",
        rxls::ChartKind::Radar => "radar",
        rxls::ChartKind::Bubble => "bubble",
    }
}

fn sparkline_kind_name(kind: rxls::SparklineKind) -> &'static str {
    match kind {
        rxls::SparklineKind::Line => "line",
        rxls::SparklineKind::Column => "column",
        rxls::SparklineKind::WinLoss => "win-loss",
    }
}

fn color_summary(color: rxls::Color) -> String {
    let [r, g, b] = color.as_rgb();
    format!("{r:02X}{g:02X}{b:02X}")
}

fn compare_sheet_cells(
    comparison: &mut Comparison,
    sheet_index: usize,
    left: &rxls::Sheet,
    right: &rxls::Sheet,
) {
    let mut coords = BTreeSet::new();
    for (row, col, _cell) in left.cells() {
        coords.insert((row, col));
    }
    for (row, col, _cell) in right.cells() {
        coords.insert((row, col));
    }

    for (row, col) in coords {
        let left_cell = left.cell(row, col);
        let right_cell = right.cell(row, col);
        if left_cell != right_cell {
            comparison.difference(format!(
                "sheet[{sheet_index}].{}!{} left={} right={}",
                escape_dump_text(&left.name),
                a1(row, col),
                compare_cell_value(left_cell),
                compare_cell_value(right_cell)
            ));
        } else {
            let left_display = left.formatted(row, col);
            let right_display = right.formatted(row, col);
            if left_display != right_display {
                comparison.difference(format!(
                    "sheet[{sheet_index}].{}!{}.display left={} right={}",
                    escape_dump_text(&left.name),
                    a1(row, col),
                    optional_text(left_display),
                    optional_text(right_display)
                ));
            }
        }
    }
}

#[cfg(any(feature = "xlsx", feature = "ods"))]
fn inspect_package(path: &str, limit: usize) -> ExitCode {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("read {path}: {err}");
            return ExitCode::from(66);
        }
    };

    if !bytes.starts_with(b"PK") {
        eprintln!("inspect-package {path}: not a ZIP spreadsheet package");
        return ExitCode::from(65);
    }

    let summary = match inspect_package_bytes("inspect-package", path, &bytes) {
        Ok(summary) => summary,
        Err(code) => return code,
    };

    println!("rxls inspect-package");
    println!("path: {path}");
    println!("format: {}", detect_format(path, &bytes));
    println!("parts: {}", summary.parts.len());
    println!("compressed_bytes: {}", summary.compressed_bytes);
    println!("uncompressed_bytes: {}", summary.uncompressed_bytes);
    println!("relationships: {}", summary.relationships);

    for part in summary.parts.iter().take(limit) {
        println!(
            "part: {} compressed={} uncompressed={}",
            escape_dump_text(&part.name),
            part.compressed_bytes,
            part.uncompressed_bytes
        );
    }
    println!("truncated: {}", summary.parts.len() > limit);

    ExitCode::SUCCESS
}

#[cfg(not(any(feature = "xlsx", feature = "ods")))]
fn inspect_package(path: &str, _limit: usize) -> ExitCode {
    eprintln!("inspect-package {path}: requires a ZIP spreadsheet feature");
    ExitCode::from(69)
}

#[cfg(feature = "xlsx")]
fn inspect_output(path: &str, limit: usize) -> ExitCode {
    let (source_bytes, workbook) = match read_workbook(path) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let output = workbook.to_xlsx();
    let summary = match inspect_package_bytes("inspect-output", "generated writer output", &output)
    {
        Ok(summary) => summary,
        Err(code) => return code,
    };
    let readback = match Workbook::open(&output) {
        Ok(workbook) => workbook,
        Err(err) => {
            eprintln!("inspect-output generated writer output: parse generated xlsx: {err}");
            return ExitCode::from(1);
        }
    };
    let readback_metadata = readback.metadata();
    let readback_cells = readback
        .sheets
        .iter()
        .map(|sheet| sheet.cells().count())
        .sum::<usize>();
    let readback_merged_ranges = readback
        .sheets
        .iter()
        .map(|sheet| sheet.merged_ranges().len())
        .sum::<usize>();
    let readback_hyperlinks = readback
        .sheets
        .iter()
        .map(|sheet| sheet.hyperlinks().len())
        .sum::<usize>();
    let readback_comments = readback
        .sheets
        .iter()
        .map(|sheet| sheet.comments().len())
        .sum::<usize>();
    let readback_tables = readback
        .sheets
        .iter()
        .map(|sheet| sheet.tables().len())
        .sum::<usize>();
    let readback_data_validations = readback
        .sheets
        .iter()
        .map(|sheet| sheet.data_validations().len())
        .sum::<usize>();
    let readback_conditional_formats = readback
        .sheets
        .iter()
        .map(|sheet| sheet.conditional_formats().len())
        .sum::<usize>();
    let readback_autofilters = readback
        .sheets
        .iter()
        .filter(|sheet| sheet.autofilter_range().is_some())
        .count();
    let readback_page_setups = readback
        .sheets
        .iter()
        .filter(|sheet| sheet.page_setup().is_some())
        .count();
    let readback_images = readback
        .sheets
        .iter()
        .map(|sheet| sheet.images().len())
        .sum::<usize>();
    let readback_charts = readback
        .sheets
        .iter()
        .map(|sheet| sheet.charts().len())
        .sum::<usize>();
    let readback_sparklines = readback
        .sheets
        .iter()
        .map(|sheet| sheet.sparklines().len())
        .sum::<usize>();

    println!("rxls inspect-output");
    println!("path: {path}");
    println!("source_format: {}", detect_format(path, &source_bytes));
    println!("output_format: xlsx");
    println!("output_bytes: {}", output.len());
    println!("parts: {}", summary.parts.len());
    println!(
        "worksheet_parts: {}",
        count_parts_with_prefix(&summary.parts, "xl/worksheets/sheet")
    );
    println!(
        "drawing_parts: {}",
        count_parts_with_prefix(&summary.parts, "xl/drawings/drawing")
    );
    println!(
        "chart_parts: {}",
        count_parts_with_prefix(&summary.parts, "xl/charts/chart")
    );
    println!(
        "table_parts: {}",
        count_parts_with_prefix(&summary.parts, "xl/tables/table")
    );
    println!(
        "shared_strings_part: {}",
        has_part(&summary.parts, "xl/sharedStrings.xml")
    );
    println!("styles_part: {}", has_part(&summary.parts, "xl/styles.xml"));
    println!(
        "workbook_rels_part: {}",
        has_part(&summary.parts, "xl/_rels/workbook.xml.rels")
    );
    println!("relationships: {}", summary.relationships);
    println!("readback_sheets: {}", readback_metadata.sheets.len());
    println!("readback_cells: {readback_cells}");
    println!(
        "readback_defined_names: {}",
        readback_metadata.defined_names.len()
    );
    println!("readback_partial: {}", readback_metadata.text_truncated);
    println!("readback_merged_ranges: {readback_merged_ranges}");
    println!("readback_hyperlinks: {readback_hyperlinks}");
    println!("readback_comments: {readback_comments}");
    println!("readback_tables: {readback_tables}");
    println!("readback_data_validations: {readback_data_validations}");
    println!("readback_conditional_formats: {readback_conditional_formats}");
    println!("readback_autofilters: {readback_autofilters}");
    println!("readback_page_setups: {readback_page_setups}");
    println!("readback_images: {readback_images}");
    println!("readback_charts: {readback_charts}");
    println!("readback_sparklines: {readback_sparklines}");

    let mut semantic_comparison = Comparison::new_with_label(limit, "semantic_difference");
    compare_workbooks(&mut semantic_comparison, &workbook, &readback);
    println!("semantic_differences: {}", semantic_comparison.differences);
    println!("semantic_truncated: {}", semantic_comparison.truncated);
    println!("semantic_equal: {}", semantic_comparison.differences == 0);

    for part in summary.parts.iter().take(limit) {
        println!(
            "part: {} compressed={} uncompressed={}",
            escape_dump_text(&part.name),
            part.compressed_bytes,
            part.uncompressed_bytes
        );
    }
    println!("truncated: {}", summary.parts.len() > limit);

    ExitCode::SUCCESS
}

#[cfg(not(feature = "xlsx"))]
fn inspect_output(path: &str, _limit: usize) -> ExitCode {
    eprintln!("inspect-output {path}: requires the xlsx writer feature");
    ExitCode::from(69)
}

fn read_workbook(path: &str) -> Result<(Vec<u8>, Workbook), ExitCode> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("read {path}: {err}");
            return Err(ExitCode::from(66));
        }
    };
    let workbook = match Workbook::open(&bytes) {
        Ok(workbook) => workbook,
        Err(err) => {
            eprintln!("parse {path}: {err}");
            return Err(ExitCode::from(1));
        }
    };
    Ok((bytes, workbook))
}

fn parse_sheet_limit_args(
    program: &str,
    command: &str,
    args: &mut impl Iterator<Item = String>,
) -> Result<(String, usize, usize), ExitCode> {
    let Some(path) = args.next() else {
        eprintln!("usage: {program} {command} <file> [--sheet N] [--limit N]");
        return Err(ExitCode::from(64));
    };

    let mut sheet = 0usize;
    let mut limit = 100usize;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sheet" => {
                let Some(value) = args.next() else {
                    eprintln!("{program} {command}: --sheet requires an index");
                    return Err(ExitCode::from(64));
                };
                sheet = match value.parse() {
                    Ok(value) => value,
                    Err(_) => {
                        eprintln!("{program} {command}: invalid --sheet value {value:?}");
                        return Err(ExitCode::from(64));
                    }
                };
            }
            "--limit" => {
                let Some(value) = args.next() else {
                    eprintln!("{program} {command}: --limit requires a count");
                    return Err(ExitCode::from(64));
                };
                limit = match value.parse() {
                    Ok(value) => value,
                    Err(_) => {
                        eprintln!("{program} {command}: invalid --limit value {value:?}");
                        return Err(ExitCode::from(64));
                    }
                };
            }
            _ => {
                eprintln!("{program} {command}: unknown option {arg:?}");
                return Err(ExitCode::from(64));
            }
        }
    }

    Ok((path, sheet, limit))
}

fn parse_sheet_args(
    program: &str,
    command: &str,
    args: &mut impl Iterator<Item = String>,
) -> Result<(String, usize), ExitCode> {
    let Some(path) = args.next() else {
        eprintln!("usage: {program} {command} <file> [--sheet N]");
        return Err(ExitCode::from(64));
    };

    let mut sheet = 0usize;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sheet" => {
                let Some(value) = args.next() else {
                    eprintln!("{program} {command}: --sheet requires an index");
                    return Err(ExitCode::from(64));
                };
                sheet = match value.parse() {
                    Ok(value) => value,
                    Err(_) => {
                        eprintln!("{program} {command}: invalid --sheet value {value:?}");
                        return Err(ExitCode::from(64));
                    }
                };
            }
            _ => {
                eprintln!("{program} {command}: unknown option {arg:?}");
                return Err(ExitCode::from(64));
            }
        }
    }

    Ok((path, sheet))
}

fn parse_csv_args(
    program: &str,
    args: &mut impl Iterator<Item = String>,
) -> Result<(String, usize, char), ExitCode> {
    let Some(path) = args.next() else {
        eprintln!("usage: {program} csv <file> [--sheet N] [--delimiter C]");
        return Err(ExitCode::from(64));
    };

    let mut sheet = 0usize;
    let mut delimiter = ',';
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sheet" => {
                let Some(value) = args.next() else {
                    eprintln!("{program} csv: --sheet requires an index");
                    return Err(ExitCode::from(64));
                };
                sheet = match value.parse() {
                    Ok(value) => value,
                    Err(_) => {
                        eprintln!("{program} csv: invalid --sheet value {value:?}");
                        return Err(ExitCode::from(64));
                    }
                };
            }
            "--delimiter" => {
                let Some(value) = args.next() else {
                    eprintln!("{program} csv: --delimiter requires one character");
                    return Err(ExitCode::from(64));
                };
                delimiter = match parse_csv_delimiter(&value) {
                    Some(delimiter) => delimiter,
                    None => {
                        eprintln!("{program} csv: invalid --delimiter value {value:?}");
                        return Err(ExitCode::from(64));
                    }
                };
            }
            _ => {
                eprintln!("{program} csv: unknown option {arg:?}");
                return Err(ExitCode::from(64));
            }
        }
    }

    Ok((path, sheet, delimiter))
}

fn parse_csv_delimiter(value: &str) -> Option<char> {
    let delimiter = match value {
        "\\t" | "tab" => '\t',
        _ => {
            let mut chars = value.chars();
            let delimiter = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            delimiter
        }
    };
    if matches!(delimiter, '\n' | '\r') {
        None
    } else {
        Some(delimiter)
    }
}

fn parse_limit_args(
    program: &str,
    command: &str,
    args: &mut impl Iterator<Item = String>,
) -> Result<(String, usize), ExitCode> {
    let Some(path) = args.next() else {
        eprintln!("usage: {program} {command} <file> [--limit N]");
        return Err(ExitCode::from(64));
    };

    let mut limit = 100usize;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--limit" => {
                let Some(value) = args.next() else {
                    eprintln!("{program} {command}: --limit requires a count");
                    return Err(ExitCode::from(64));
                };
                limit = match value.parse() {
                    Ok(value) => value,
                    Err(_) => {
                        eprintln!("{program} {command}: invalid --limit value {value:?}");
                        return Err(ExitCode::from(64));
                    }
                };
            }
            _ => {
                eprintln!("{program} {command}: unknown option {arg:?}");
                return Err(ExitCode::from(64));
            }
        }
    }

    Ok((path, limit))
}

fn parse_compare_args(
    program: &str,
    args: &mut impl Iterator<Item = String>,
) -> Result<(String, String, usize), ExitCode> {
    let Some(left) = args.next() else {
        eprintln!("usage: {program} compare <left> <right> [--limit N]");
        return Err(ExitCode::from(64));
    };
    let Some(right) = args.next() else {
        eprintln!("usage: {program} compare <left> <right> [--limit N]");
        return Err(ExitCode::from(64));
    };

    let mut limit = 100usize;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--limit" => {
                let Some(value) = args.next() else {
                    eprintln!("{program} compare: --limit requires a count");
                    return Err(ExitCode::from(64));
                };
                limit = match value.parse() {
                    Ok(value) => value,
                    Err(_) => {
                        eprintln!("{program} compare: invalid --limit value {value:?}");
                        return Err(ExitCode::from(64));
                    }
                };
            }
            _ => {
                eprintln!("{program} compare: unknown option {arg:?}");
                return Err(ExitCode::from(64));
            }
        }
    }

    Ok((left, right, limit))
}

fn print_usage(program: &str) {
    eprintln!("usage: {program} <command> <file>");
    eprintln!("commands:");
    eprintln!("  --version              print binary version");
    eprintln!("  info <file>            summarize workbook sheets and metadata");
    eprintln!("  dump <file>            print bounded sheet cells");
    eprintln!("  csv <file>             export one sheet as CSV");
    eprintln!("  sheet <file>           summarize one sheet metadata surface");
    eprintln!("  formula <file>         print bounded formula cells");
    eprintln!("  diagnose <file>        print JSON workbook diagnostics");
    eprintln!("  metadata <file>        print workbook metadata");
    eprintln!("  inspect-package <file> inspect ZIP package parts");
    eprintln!("  inspect-output <file>  inspect generated writer .xlsx parts");
    eprintln!("  compare <left> <right> compare workbook structure and cells");
    eprintln!("  corpus-report <file>   summarize a public corpus manifest");
    eprintln!("  fixture-report <file>  validate a committed fixture manifest");
}

#[derive(Default)]
struct CorpusExtStats {
    files: usize,
    opened: usize,
    failed: usize,
}

struct CorpusFailure {
    ext: String,
    label: String,
    kind: String,
    decision: String,
    evidence: String,
    container: String,
    extension_mismatch: bool,
    error: String,
}

struct CorpusEntry {
    path: String,
    local_path: Option<String>,
    status: Option<String>,
}

#[derive(Default)]
struct FixtureFormatStats {
    files: usize,
    opened: usize,
    hash_ok: usize,
    oracle_ok: usize,
    failed: usize,
}

struct FixtureEntry {
    path: String,
    format: String,
    sha256: String,
    covers: Vec<String>,
    oracle: Option<FixtureOracle>,
}

struct FixtureReportRow {
    path: String,
    format: String,
    covers: Vec<String>,
    hash_ok: bool,
    open_ok: bool,
    oracle_status: FixtureOracleStatus,
    actual_oracle: Option<FixtureOracle>,
    error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FixtureOracle {
    sheets: usize,
    cells: usize,
    defined_names: usize,
    hidden_sheets: usize,
    merged_ranges: usize,
    hyperlinks: usize,
    comments: usize,
    tables: usize,
    data_validations: usize,
    autofilters: usize,
    page_setups: usize,
    images: usize,
    sheet_views: usize,
    tab_colors: usize,
    print_options: usize,
    sparklines: usize,
}

impl FixtureOracle {
    fn summary(&self) -> String {
        format!(
            "sheets={} cells={} defined_names={} hidden_sheets={} merged_ranges={} hyperlinks={} comments={} tables={} data_validations={} autofilters={} page_setups={} images={} sheet_views={} tab_colors={} print_options={} sparklines={}",
            self.sheets,
            self.cells,
            self.defined_names,
            self.hidden_sheets,
            self.merged_ranges,
            self.hyperlinks,
            self.comments,
            self.tables,
            self.data_validations,
            self.autofilters,
            self.page_setups,
            self.images,
            self.sheet_views,
            self.tab_colors,
            self.print_options,
            self.sparklines
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FixtureOracleStatus {
    Missing,
    Ok,
    Mismatch,
    Skipped,
}

impl FixtureOracleStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Ok => "ok",
            Self::Mismatch => "mismatch",
            Self::Skipped => "skipped",
        }
    }

    fn is_ok_or_missing(self) -> bool {
        matches!(self, Self::Ok | Self::Missing)
    }
}

fn parse_corpus_manifest(manifest: &str) -> Result<Vec<CorpusEntry>, String> {
    let files = json_array_for_key(manifest, "files")
        .ok_or_else(|| "manifest missing top-level files array".to_string())?;
    let objects = json_objects_in_array(files)?;
    let entries = objects
        .into_iter()
        .map(|object| CorpusEntry {
            path: json_string_field(object, "path").unwrap_or_default(),
            local_path: json_string_field(object, "local_path"),
            status: json_string_field(object, "status"),
        })
        .collect();
    Ok(entries)
}

fn parse_fixture_manifest(manifest: &str) -> Result<Vec<FixtureEntry>, String> {
    let fixtures =
        json_root_array(manifest).ok_or_else(|| "manifest missing top-level array".to_string())?;
    let objects = json_objects_in_array(fixtures)?;
    let mut entries = Vec::with_capacity(objects.len());
    for (index, object) in objects.into_iter().enumerate() {
        entries.push(FixtureEntry {
            path: json_string_field(object, "path")
                .ok_or_else(|| format!("fixture[{index}] missing path"))?,
            format: json_string_field(object, "format")
                .ok_or_else(|| format!("fixture[{index}] missing format"))?,
            sha256: json_string_field(object, "sha256")
                .ok_or_else(|| format!("fixture[{index}] missing sha256"))?,
            covers: json_string_array_field(object, "covers")
                .ok_or_else(|| format!("fixture[{index}] missing covers"))?,
            oracle: json_object_field(object, "oracle")
                .map(|oracle| parse_fixture_oracle(oracle, index))
                .transpose()?,
        });
    }
    Ok(entries)
}

fn parse_fixture_oracle(object: &str, index: usize) -> Result<FixtureOracle, String> {
    Ok(FixtureOracle {
        sheets: json_usize_field(object, "sheets")
            .ok_or_else(|| format!("fixture[{index}] oracle missing sheets"))?,
        cells: json_usize_field(object, "cells")
            .ok_or_else(|| format!("fixture[{index}] oracle missing cells"))?,
        defined_names: json_usize_field(object, "defined_names")
            .ok_or_else(|| format!("fixture[{index}] oracle missing defined_names"))?,
        hidden_sheets: json_usize_field(object, "hidden_sheets")
            .ok_or_else(|| format!("fixture[{index}] oracle missing hidden_sheets"))?,
        merged_ranges: json_usize_field(object, "merged_ranges")
            .ok_or_else(|| format!("fixture[{index}] oracle missing merged_ranges"))?,
        hyperlinks: json_usize_field(object, "hyperlinks")
            .ok_or_else(|| format!("fixture[{index}] oracle missing hyperlinks"))?,
        comments: json_usize_field(object, "comments")
            .ok_or_else(|| format!("fixture[{index}] oracle missing comments"))?,
        tables: json_usize_field(object, "tables")
            .ok_or_else(|| format!("fixture[{index}] oracle missing tables"))?,
        data_validations: json_usize_field(object, "data_validations")
            .ok_or_else(|| format!("fixture[{index}] oracle missing data_validations"))?,
        autofilters: json_usize_field(object, "autofilters")
            .ok_or_else(|| format!("fixture[{index}] oracle missing autofilters"))?,
        page_setups: json_usize_field(object, "page_setups")
            .ok_or_else(|| format!("fixture[{index}] oracle missing page_setups"))?,
        images: json_usize_field(object, "images")
            .ok_or_else(|| format!("fixture[{index}] oracle missing images"))?,
        sheet_views: json_usize_field(object, "sheet_views")
            .ok_or_else(|| format!("fixture[{index}] oracle missing sheet_views"))?,
        tab_colors: json_usize_field(object, "tab_colors")
            .ok_or_else(|| format!("fixture[{index}] oracle missing tab_colors"))?,
        print_options: json_usize_field(object, "print_options")
            .ok_or_else(|| format!("fixture[{index}] oracle missing print_options"))?,
        sparklines: json_usize_field(object, "sparklines")
            .ok_or_else(|| format!("fixture[{index}] oracle missing sparklines"))?,
    })
}

fn json_root_array(json: &str) -> Option<&str> {
    let array_start = json.find('[')?;
    let array_end = matching_json_bracket(json, array_start, '[', ']')?;
    Some(&json[array_start + 1..array_end])
}

fn json_array_for_key<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let key_start = json.find(&format!("\"{key}\""))?;
    let array_start = key_start + json[key_start..].find('[')?;
    let array_end = matching_json_bracket(json, array_start, '[', ']')?;
    Some(&json[array_start + 1..array_end])
}

fn matching_json_bracket(json: &str, start: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, ch) in json[start..].char_indices() {
        let absolute = start + index;
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            _ if ch == open => depth += 1,
            _ if ch == close => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(absolute);
                }
            }
            _ => {}
        }
    }
    None
}

fn json_objects_in_array(array: &str) -> Result<Vec<&str>, String> {
    let mut objects = Vec::new();
    let mut start = None;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (index, ch) in array.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    start = Some(index);
                }
                depth += 1;
            }
            '}' => {
                depth = depth.checked_sub(1).ok_or_else(|| {
                    "manifest files array has an unmatched object close".to_string()
                })?;
                if depth == 0 {
                    let start = start.take().ok_or_else(|| {
                        "manifest files array has an object without a start".to_string()
                    })?;
                    objects.push(&array[start..=index]);
                }
            }
            _ => {}
        }
    }

    if in_string || depth != 0 {
        return Err("manifest files array has an unterminated object".to_string());
    }

    Ok(objects)
}

fn json_string_field(object: &str, key: &str) -> Option<String> {
    let key_start = object.find(&format!("\"{key}\""))?;
    let after_key = &object[key_start..];
    let colon = after_key.find(':')?;
    let value = after_key[colon + 1..].trim_start();
    parse_json_string(value)
}

fn json_string_array_field(object: &str, key: &str) -> Option<Vec<String>> {
    let key_start = object.find(&format!("\"{key}\""))?;
    let after_key = &object[key_start..];
    let colon = after_key.find(':')?;
    let value_start = key_start + colon + 1;
    let value = object[value_start..].trim_start();
    if !value.starts_with('[') {
        return None;
    }
    let leading_whitespace = object[value_start..].len() - value.len();
    let array_start = value_start + leading_whitespace;
    let array_end = matching_json_bracket(object, array_start, '[', ']')?;
    parse_json_string_array(&object[array_start + 1..array_end])
}

fn json_object_field<'a>(object: &'a str, key: &str) -> Option<&'a str> {
    let key_start = object.find(&format!("\"{key}\""))?;
    let after_key = &object[key_start..];
    let colon = after_key.find(':')?;
    let value_start = key_start + colon + 1;
    let value = object[value_start..].trim_start();
    if !value.starts_with('{') {
        return None;
    }
    let leading_whitespace = object[value_start..].len() - value.len();
    let object_start = value_start + leading_whitespace;
    let object_end = matching_json_bracket(object, object_start, '{', '}')?;
    Some(&object[object_start + 1..object_end])
}

fn json_usize_field(object: &str, key: &str) -> Option<usize> {
    let key_start = object.find(&format!("\"{key}\""))?;
    let after_key = &object[key_start..];
    let colon = after_key.find(':')?;
    let value = after_key[colon + 1..].trim_start();
    let digits_len = value
        .bytes()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits_len == 0 {
        return None;
    }
    value[..digits_len].parse().ok()
}

fn parse_json_string_array(array: &str) -> Option<Vec<String>> {
    let mut values = Vec::new();
    let mut index = 0usize;
    let mut expect_value = true;

    loop {
        while index < array.len() {
            let ch = array[index..].chars().next()?;
            if ch.is_whitespace() {
                index += ch.len_utf8();
            } else {
                break;
            }
        }
        if index >= array.len() {
            return Some(values);
        }

        if !expect_value {
            if array[index..].starts_with(',') {
                index += 1;
                expect_value = true;
                continue;
            }
            return None;
        }

        let (value, consumed) = parse_json_string_with_len(&array[index..])?;
        values.push(value);
        index += consumed;
        expect_value = false;
    }
}

fn parse_json_string(value: &str) -> Option<String> {
    parse_json_string_with_len(value).map(|(parsed, _)| parsed)
}

fn parse_json_string_with_len(value: &str) -> Option<(String, usize)> {
    if !value.starts_with('"') {
        return None;
    }
    let mut index = 1usize;
    let mut parsed = String::new();
    while index < value.len() {
        let ch = value[index..].chars().next()?;
        index += ch.len_utf8();
        match ch {
            '"' => return Some((parsed, index)),
            '\\' => {
                let escaped = value[index..].chars().next()?;
                index += escaped.len_utf8();
                match escaped {
                    '"' => parsed.push('"'),
                    '\\' => parsed.push('\\'),
                    '/' => parsed.push('/'),
                    'b' => parsed.push('\u{0008}'),
                    'f' => parsed.push('\u{000c}'),
                    'n' => parsed.push('\n'),
                    'r' => parsed.push('\r'),
                    't' => parsed.push('\t'),
                    'u' => {
                        let end = index.checked_add(4)?;
                        if end > value.len() {
                            return None;
                        }
                        let hex = &value[index..end];
                        if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                            return None;
                        }
                        index = end;
                        let code = u32::from_str_radix(hex, 16).ok()?;
                        parsed.push(char::from_u32(code)?);
                    }
                    other => parsed.push(other),
                }
            }
            other => parsed.push(other),
        }
    }
    None
}

fn manifest_relative_path(manifest_dir: &Path, local_path: &str) -> PathBuf {
    let path = PathBuf::from(local_path);
    if path.is_absolute() || path.exists() {
        return path;
    }
    let from_manifest = manifest_dir.join(&path);
    if from_manifest.exists() {
        from_manifest
    } else {
        path
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut hash = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (bytes.len() as u64) * 8;
    let mut padded = bytes.to_vec();
    padded.push(0x80);
    while (padded.len() + 8) % 64 != 0 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks_exact(64) {
        let mut words = [0u32; 64];
        for (index, word) in words.iter_mut().take(16).enumerate() {
            let start = index * 4;
            *word = u32::from_be_bytes([
                chunk[start],
                chunk[start + 1],
                chunk[start + 2],
                chunk[start + 3],
            ]);
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }

        let mut a = hash[0];
        let mut b = hash[1];
        let mut c = hash[2];
        let mut d = hash[3];
        let mut e = hash[4];
        let mut f = hash[5];
        let mut g = hash[6];
        let mut h = hash[7];

        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        hash[0] = hash[0].wrapping_add(a);
        hash[1] = hash[1].wrapping_add(b);
        hash[2] = hash[2].wrapping_add(c);
        hash[3] = hash[3].wrapping_add(d);
        hash[4] = hash[4].wrapping_add(e);
        hash[5] = hash[5].wrapping_add(f);
        hash[6] = hash[6].wrapping_add(g);
        hash[7] = hash[7].wrapping_add(h);
    }

    let mut digest = String::with_capacity(64);
    for word in hash {
        write!(&mut digest, "{word:08x}").expect("write SHA-256 digest");
    }
    digest
}

fn fixture_actual_oracle(workbook: &Workbook) -> FixtureOracle {
    let metadata = workbook.metadata();
    FixtureOracle {
        sheets: metadata.sheets.len(),
        cells: workbook
            .sheets
            .iter()
            .map(|sheet| sheet.cells().count())
            .sum(),
        defined_names: metadata.defined_names.len(),
        hidden_sheets: metadata
            .sheets
            .iter()
            .filter(|sheet| sheet.visible != SheetVisible::Visible)
            .count(),
        merged_ranges: workbook
            .sheets
            .iter()
            .map(|sheet| sheet.merged_ranges().len())
            .sum(),
        hyperlinks: workbook
            .sheets
            .iter()
            .map(|sheet| sheet.hyperlinks().len())
            .sum(),
        comments: workbook
            .sheets
            .iter()
            .map(|sheet| sheet.comments().len())
            .sum(),
        tables: workbook
            .sheets
            .iter()
            .map(|sheet| sheet.tables().len())
            .sum(),
        data_validations: workbook
            .sheets
            .iter()
            .map(|sheet| sheet.data_validations().len())
            .sum(),
        autofilters: workbook
            .sheets
            .iter()
            .filter(|sheet| sheet.autofilter_range().is_some())
            .count(),
        page_setups: workbook
            .sheets
            .iter()
            .filter(|sheet| sheet.page_setup().is_some())
            .count(),
        images: workbook
            .sheets
            .iter()
            .map(|sheet| sheet.images().len())
            .sum(),
        sheet_views: workbook
            .sheets
            .iter()
            .filter(|sheet| sheet.sheet_view() != SheetView::default())
            .count(),
        tab_colors: workbook
            .sheets
            .iter()
            .filter(|sheet| sheet.tab_color().is_some())
            .count(),
        print_options: workbook
            .sheets
            .iter()
            .filter(|sheet| sheet.print_gridlines() || sheet.print_headings())
            .count(),
        sparklines: workbook
            .sheets
            .iter()
            .map(|sheet| sheet.sparklines().len())
            .sum(),
    }
}

fn corpus_ext(manifest_path: &str, local_path: &str) -> String {
    let ext = Path::new(manifest_path)
        .extension()
        .or_else(|| Path::new(local_path).extension())
        .and_then(|ext| ext.to_str())
        .unwrap_or("unknown")
        .to_ascii_lowercase();
    format!(".{ext}")
}

fn corpus_local_path(manifest_dir: &Path, local_path: &str) -> PathBuf {
    manifest_relative_path(manifest_dir, local_path)
}

fn corpus_label(entry: &CorpusEntry, local_path: &str) -> String {
    if !entry.path.is_empty() {
        entry.path.clone()
    } else {
        Path::new(local_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(local_path)
            .to_string()
    }
}

fn detect_format(path: &str, bytes: &[u8]) -> &'static str {
    const OLE2_MAGIC: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    if bytes.starts_with(OLE2_MAGIC) {
        return "xls";
    }

    let extension = Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase());
    match extension.as_deref() {
        Some("xlsx") => "xlsx",
        Some("xlsm") => "xlsm",
        Some("xlsb") => "xlsb",
        Some("ods") => "ods",
        Some("xls") => "xls",
        _ if bytes.starts_with(b"PK") => "zip-spreadsheet",
        _ => "unknown",
    }
}

fn sheet_type_name(sheet_type: SheetType) -> &'static str {
    match sheet_type {
        SheetType::WorkSheet => "worksheet",
        SheetType::DialogSheet => "dialog",
        SheetType::MacroSheet => "macro",
        SheetType::ChartSheet => "chart",
        SheetType::Vba => "vba",
    }
}

fn visible_name(visible: SheetVisible) -> &'static str {
    match visible {
        SheetVisible::Visible => "visible",
        SheetVisible::Hidden => "hidden",
        SheetVisible::VeryHidden => "very-hidden",
    }
}

fn print_property(label: &str, value: Option<&str>) {
    if let Some(value) = value {
        println!("{label}: {}", escape_dump_text(value));
    }
}

fn cell_kind(cell: &Cell) -> &'static str {
    match cell {
        Cell::Text(_) => "text",
        Cell::Number(_) => "number",
        Cell::Date(_) => "date",
        Cell::Bool(_) => "bool",
        Cell::Error(_) => "error",
        Cell::Formula { .. } => "formula",
    }
}

fn cell_value(cell: &Cell) -> String {
    match cell {
        Cell::Text(value) => value.clone(),
        Cell::Number(value) | Cell::Date(value) => number_text(*value),
        Cell::Bool(value) => if *value { "TRUE" } else { "FALSE" }.to_string(),
        Cell::Error(value) => value.clone(),
        Cell::Formula { formula, .. } => format!("={formula}"),
    }
}

fn compare_cell_value(cell: Option<&Cell>) -> String {
    match cell {
        Some(Cell::Formula { formula, cached }) => format!(
            "formula:{} cached={}",
            escape_dump_text(&format!("={formula}")),
            compare_cell_value(Some(cached.as_ref()))
        ),
        Some(cell) => format!(
            "{}:{}",
            cell_kind(cell),
            escape_dump_text(&cell_value(cell))
        ),
        None => "missing".to_string(),
    }
}

fn number_text(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

fn escape_dump_text(value: &str) -> String {
    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn format_dimensions((r0, c0, r1, c1): (u32, u16, u32, u16)) -> String {
    format!("{}:{}", a1(r0, c0), a1(r1, c1))
}

fn a1(row: u32, col: u16) -> String {
    format!("{}{}", column_label(col), row + 1)
}

fn column_label(col: u16) -> String {
    let mut n = usize::from(col);
    let mut chars = Vec::new();
    loop {
        chars.push(char::from(b'A' + (n % 26) as u8));
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    chars.iter().rev().collect()
}

#[cfg(any(feature = "xlsx", feature = "ods"))]
struct PackagePart {
    name: String,
    compressed_bytes: u64,
    uncompressed_bytes: u64,
}

#[cfg(any(feature = "xlsx", feature = "ods"))]
struct PackageSummary {
    parts: Vec<PackagePart>,
    compressed_bytes: u64,
    uncompressed_bytes: u64,
    relationships: usize,
}

#[cfg(any(feature = "xlsx", feature = "ods"))]
fn inspect_package_bytes(
    command: &str,
    label: &str,
    bytes: &[u8],
) -> Result<PackageSummary, ExitCode> {
    let mut archive = match zip::ZipArchive::new(std::io::Cursor::new(bytes)) {
        Ok(archive) => archive,
        Err(err) => {
            eprintln!("{command} {label}: invalid ZIP package: {err}");
            return Err(ExitCode::from(65));
        }
    };

    let mut parts = Vec::with_capacity(archive.len());
    let mut relationships = 0usize;
    for index in 0..archive.len() {
        let mut file = match archive.by_index(index) {
            Ok(file) => file,
            Err(err) => {
                eprintln!("{command} {label}: read ZIP entry {index}: {err}");
                return Err(ExitCode::from(65));
            }
        };
        let name = file.name().to_string();
        let compressed_bytes = file.compressed_size();
        let uncompressed_bytes = file.size();
        if name.ends_with(".rels") {
            let mut rels_xml = Vec::new();
            if let Err(err) = file.read_to_end(&mut rels_xml) {
                eprintln!("{command} {label}: read relationships {name}: {err}");
                return Err(ExitCode::from(65));
            }
            relationships += String::from_utf8_lossy(&rels_xml)
                .matches("<Relationship ")
                .count();
        }
        parts.push(PackagePart {
            name,
            compressed_bytes,
            uncompressed_bytes,
        });
    }
    parts.sort_by(|left, right| left.name.cmp(&right.name));

    let compressed_bytes = parts.iter().map(|part| part.compressed_bytes).sum::<u64>();
    let uncompressed_bytes = parts
        .iter()
        .map(|part| part.uncompressed_bytes)
        .sum::<u64>();

    Ok(PackageSummary {
        parts,
        compressed_bytes,
        uncompressed_bytes,
        relationships,
    })
}

#[cfg(any(feature = "xlsx", feature = "ods"))]
fn count_parts_with_prefix(parts: &[PackagePart], prefix: &str) -> usize {
    parts
        .iter()
        .filter(|part| part.name.starts_with(prefix) && part.name.ends_with(".xml"))
        .count()
}

#[cfg(any(feature = "xlsx", feature = "ods"))]
fn has_part(parts: &[PackagePart], name: &str) -> bool {
    parts.iter().any(|part| part.name == name)
}

struct Comparison {
    label: &'static str,
    limit: usize,
    differences: usize,
    printed: usize,
    truncated: bool,
}

impl Comparison {
    fn new(limit: usize) -> Self {
        Self::new_with_label(limit, "difference")
    }

    fn new_with_label(limit: usize, label: &'static str) -> Self {
        Self {
            label,
            limit,
            differences: 0,
            printed: 0,
            truncated: false,
        }
    }

    fn difference(&mut self, difference: String) {
        self.differences += 1;
        if self.printed < self.limit {
            println!("{}: {difference}", self.label);
            self.printed += 1;
        } else {
            self.truncated = true;
        }
    }
}
