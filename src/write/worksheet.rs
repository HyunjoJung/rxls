//! Per-sheet worksheet XML (`xl/worksheets/sheetN.xml`): the `<sheetData>` grid
//! plus all the post-grid blocks (merges, conditional formatting, data
//! validations, hyperlinks, page setup, drawing/table references).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::write::cell::{write_cell, write_rich_cell, CellWriteContext};
use crate::write::styles::StyleTable;
use crate::write::xml::{
    a1, esc_attr, esc_text, hex, NS_MAIN, NS_PKG_REL, NS_R, REL_COMMENTS, REL_DRAWING,
    REL_HYPERLINK, REL_TABLE, REL_VML_DRAWING, XML_DECL,
};
use crate::write::{MAX_COL, MAX_ROW};
use crate::{
    Cell, CellEntry, CellStyle, CfRule, Color, DataValidation, DvKind, DvOp, Sheet, Sparkline,
    SparklineKind,
};

fn dv_kind_str(k: DvKind) -> &'static str {
    match k {
        DvKind::List => "list",
        DvKind::Whole => "whole",
        DvKind::Decimal => "decimal",
        DvKind::Date => "date",
        DvKind::Time => "time",
        DvKind::TextLength => "textLength",
        DvKind::Custom => "custom",
    }
}
fn dv_op_str(o: DvOp) -> &'static str {
    match o {
        DvOp::Between => "between",
        DvOp::NotBetween => "notBetween",
        DvOp::Equal => "equal",
        DvOp::NotEqual => "notEqual",
        DvOp::GreaterThan => "greaterThan",
        DvOp::LessThan => "lessThan",
        DvOp::GreaterThanOrEqual => "greaterThanOrEqual",
        DvOp::LessThanOrEqual => "lessThanOrEqual",
    }
}

/// `true` if `(row, col)` lies inside a merged range but is **not** its top-left —
/// such cells must not be emitted (Excel repairs the file otherwise). O(merges)
/// per cell; no range expansion, so a giant merge can't blow up.
fn under_merge(merges: &[(u32, u16, u32, u16)], row: u32, col: u16) -> bool {
    merges.iter().any(|&(r0, c0, r1, c1)| {
        row >= r0 && row <= r1 && col >= c0 && col <= c1 && (row, col) != (r0, c0)
    })
}

fn consume_budget(budget: &mut usize, cost: usize) -> bool {
    if cost > *budget {
        *budget = 0;
        return false;
    }
    *budget -= cost;
    true
}

fn consume_optional_record_budget(
    budget: &mut usize,
    cost: usize,
    preserve_on_failure: bool,
) -> bool {
    if cost > *budget {
        if !preserve_on_failure {
            *budget = 0;
        }
        return false;
    }
    *budget -= cost;
    true
}

fn push_budgeted(out: &mut String, budget: &mut usize, xml: String) -> bool {
    if consume_budget(budget, xml.len()) {
        out.push_str(&xml);
        true
    } else {
        false
    }
}

fn sheet_relationships_wrapper_len() -> usize {
    format!(r#"{XML_DECL}<Relationships xmlns="{NS_PKG_REL}"></Relationships>"#).len()
}

fn hyperlink_relationship_xml_len(id: usize, url: &str) -> usize {
    format!(
        r#"<Relationship Id="rId{id}" Type="{REL_HYPERLINK}" Target="{}" TargetMode="External"/>"#,
        esc_attr(url)
    )
    .len()
}

fn sheet_relationship_xml_len(id: usize, ty: &str, target: &str) -> usize {
    format!(r#"<Relationship Id="rId{id}" Type="{ty}" Target="{target}"/>"#).len()
}

fn sheet_relationship_budget_cost(has_relationships: bool, record_cost: usize) -> usize {
    record_cost.saturating_add(if has_relationships {
        0
    } else {
        sheet_relationships_wrapper_len()
    })
}

fn needs_date_style(value: &Cell) -> bool {
    match value {
        Cell::Date(_) => true,
        Cell::Formula { cached, .. } => needs_date_style(cached),
        _ => false,
    }
}

fn write_blank_cell(out: &mut String, row: u32, col: u16, xf: u32, budget: &mut usize) -> bool {
    let ref_ = a1(row, col);
    let s = if xf != 0 {
        format!(r#" s="{xf}""#)
    } else {
        String::new()
    };
    let xml = format!(r#"<c r="{ref_}"{s}/>"#);
    if !consume_budget(budget, xml.len()) {
        return false;
    }
    out.push_str(&xml);
    true
}

#[derive(Clone, Copy)]
enum GridEntry<'a> {
    Value(&'a CellEntry),
    Blank {
        row: u32,
        col: u16,
        style: &'a CellStyle,
    },
}

impl<'a> GridEntry<'a> {
    fn col(self) -> u16 {
        match self {
            GridEntry::Value(entry) => entry.col,
            GridEntry::Blank { col, .. } => col,
        }
    }

    fn explicit_style(self) -> Option<&'a CellStyle> {
        match self {
            GridEntry::Value(entry) => entry.style.as_ref(),
            GridEntry::Blank { style, .. } => Some(style),
        }
    }
}

fn merged_style<'a>(
    default_style: Option<&'a CellStyle>,
    col_style: Option<&'a CellStyle>,
    row_style: Option<&'a CellStyle>,
    table_header_style: Option<&'a CellStyle>,
    explicit_style: Option<&'a CellStyle>,
) -> Option<CellStyle> {
    [
        default_style,
        col_style,
        row_style,
        table_header_style,
        explicit_style,
    ]
    .into_iter()
    .flatten()
    .fold(None, |acc: Option<CellStyle>, style| {
        Some(match acc {
            Some(base) => base.merge(style),
            None => style.clone(),
        })
    })
}

fn table_header_style(sheet: &Sheet, row: u32, col: u16) -> Option<&CellStyle> {
    sheet.tables.iter().find_map(|table| {
        let (r0, c0, _, c1) = table.range;
        if row == r0 && col >= c0 && col <= c1 {
            sheet.table_header_formats.get(&table.name)
        } else {
            None
        }
    })
}

fn data_validations_wrapper_len(count: usize) -> usize {
    format!(r#"<dataValidations count="{count}">"#).len() + "</dataValidations>".len()
}

fn hyperlinks_wrapper_len() -> usize {
    "<hyperlinks>".len() + "</hyperlinks>".len()
}

fn table_parts_wrapper_len(count: usize) -> usize {
    format!(r#"<tableParts count="{count}">"#).len() + "</tableParts>".len()
}

fn cols_wrapper_len() -> usize {
    "<cols>".len() + "</cols>".len()
}

fn push_col_record(body: &mut String, budget: &mut usize, emitted: &mut bool, xml: String) -> bool {
    let wrapper_cost = if *emitted { 0 } else { cols_wrapper_len() };
    if !consume_budget(budget, xml.len().saturating_add(wrapper_cost)) {
        return false;
    }
    body.push_str(&xml);
    *emitted = true;
    true
}

fn intern_conditional_dxf(
    styles: &mut StyleTable,
    fill: Color,
    budget: &mut usize,
    preserve_budget_on_failure: bool,
) -> Option<u32> {
    let budget_before = *budget;
    let dxf = styles.intern_dxf_with_budget(fill, budget);
    if dxf.is_none() && preserve_budget_on_failure {
        *budget = budget_before;
    }
    dxf
}

fn merge_cells_wrapper_len(count: usize) -> usize {
    format!(r#"<mergeCells count="{count}">"#).len() + "</mergeCells>".len()
}

fn data_validation_xml(dv: &DataValidation) -> String {
    let (r0, c0, r1, c1) = dv.sqref;
    let sq = format!(
        "{}:{}",
        a1(r0.min(MAX_ROW), c0.min(MAX_COL)),
        a1(r1.min(MAX_ROW), c1.min(MAX_COL))
    );
    let mut sx = String::new();
    sx.push_str(&format!(
        r#"<dataValidation type="{}""#,
        dv_kind_str(dv.kind)
    ));
    if !matches!(dv.kind, DvKind::List | DvKind::Custom) {
        sx.push_str(&format!(r#" operator="{}""#, dv_op_str(dv.operator)));
    }
    sx.push_str(&format!(
        r#" allowBlank="{}" showInputMessage="{}" showErrorMessage="{}" sqref="{sq}""#,
        u8::from(dv.allow_blank),
        u8::from(dv.show_input_message),
        u8::from(dv.show_error_message)
    ));
    if let Some((t, m)) = &dv.prompt {
        sx.push_str(&format!(
            r#" promptTitle="{}" prompt="{}""#,
            esc_attr(t),
            esc_attr(m)
        ));
    }
    if let Some((t, m)) = &dv.error {
        sx.push_str(&format!(
            r#" errorTitle="{}" error="{}""#,
            esc_attr(t),
            esc_attr(m)
        ));
    }
    sx.push('>');
    sx.push_str(&format!("<formula1>{}</formula1>", esc_text(&dv.formula1)));
    if let Some(f2) = &dv.formula2 {
        sx.push_str(&format!("<formula2>{}</formula2>", esc_text(f2)));
    }
    sx.push_str("</dataValidation>");
    sx
}

fn sparkline_kind_attr(kind: SparklineKind) -> &'static str {
    match kind {
        SparklineKind::Line => "",
        SparklineKind::Column => r#" type="column""#,
        SparklineKind::WinLoss => r#" type="stacked""#,
    }
}

const SPARKLINE_PREFIX: &str = r#"<extLst><ext xmlns:x14="http://schemas.microsoft.com/office/spreadsheetml/2009/9/main" uri="{05C60535-1F16-4fd2-B633-F4F36F0B64E0}"><x14:sparklineGroups xmlns:xm="http://schemas.microsoft.com/office/excel/2006/main">"#;
const SPARKLINE_SUFFIX: &str = r#"</x14:sparklineGroups></ext></extLst>"#;
const SPARKLINE_COLORS: &str = r#"<x14:colorSeries theme="4" tint="-0.499984740745262"/><x14:colorNegative theme="5"/><x14:colorAxis rgb="FF000000"/><x14:colorMarkers theme="4" tint="-0.499984740745262"/><x14:colorFirst theme="4" tint="0.39997558519241921"/><x14:colorLast theme="4" tint="0.39997558519241921"/><x14:colorHigh theme="4"/><x14:colorLow theme="4"/>"#;

fn escaped_text_len(s: &str) -> usize {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".len(),
            '<' => "&lt;".len(),
            '>' => "&gt;".len(),
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => 0,
            c if matches!(c as u32, 0xFFFE | 0xFFFF) => 0,
            c => c.len_utf8(),
        })
        .sum()
}

fn sparkline_group_len(sp: &Sparkline, sqref: &str) -> usize {
    format!(
        r#"<x14:sparklineGroup{} displayEmptyCellsAs="gap">"#,
        sparkline_kind_attr(sp.kind)
    )
    .len()
    .saturating_add(SPARKLINE_COLORS.len())
    .saturating_add(r#"<x14:sparklines><x14:sparkline><xm:f>"#.len())
    .saturating_add(escaped_text_len(&sp.range))
    .saturating_add(r#"</xm:f><xm:sqref>"#.len())
    .saturating_add(sqref.len())
    .saturating_add(r#"</xm:sqref></x14:sparkline></x14:sparklines></x14:sparklineGroup>"#.len())
}

fn push_sparkline_group(out: &mut String, sp: &Sparkline, sqref: &str) {
    out.push_str(&format!(
        r#"<x14:sparklineGroup{} displayEmptyCellsAs="gap">"#,
        sparkline_kind_attr(sp.kind)
    ));
    out.push_str(SPARKLINE_COLORS);
    out.push_str("<x14:sparklines><x14:sparkline><xm:f>");
    out.push_str(&esc_text(&sp.range));
    out.push_str("</xm:f><xm:sqref>");
    out.push_str(sqref);
    out.push_str("</xm:sqref></x14:sparkline></x14:sparklines></x14:sparklineGroup>");
}

fn sparklines_xml_with_budget(sheet: &Sheet, budget: &mut usize) -> String {
    let mut xml = String::new();
    let mut emitted = false;
    for sp in &sheet.sparklines {
        let (row, col) = sp.location;
        if row > MAX_ROW || col > MAX_COL {
            continue;
        }
        let sqref = a1(row, col);
        let wrapper_cost = if emitted {
            0
        } else {
            SPARKLINE_PREFIX
                .len()
                .saturating_add(SPARKLINE_SUFFIX.len())
        };
        let cost = sparkline_group_len(sp, &sqref).saturating_add(wrapper_cost);
        if !consume_budget(budget, cost) {
            break;
        }
        if !emitted {
            xml.push_str(SPARKLINE_PREFIX);
            emitted = true;
        }
        push_sparkline_group(&mut xml, sp, &sqref);
    }
    if emitted {
        xml.push_str(SPARKLINE_SUFFIX);
    }
    xml
}

/// Serialize one sheet's worksheet XML, returning the XML and the per-sheet
/// hyperlinks (`(a1 ref, url)`) that the caller turns into sheet rels.
///
/// Threads the workbook-wide interning state (`styles`, `sst`, `sst_idx`) and the
/// shared output `budget` so cells dedupe/format consistently and the total
/// emitted size stays bounded across every sheet.
pub(super) struct WorksheetXmlOptions<'a> {
    pub(super) is_active: bool,
    pub(super) sheet_num: usize,
    pub(super) has_drawing: bool,
    pub(super) has_comments: bool,
    pub(super) table_nums: &'a [usize],
}

pub(super) struct WorksheetXmlContext<'a> {
    pub(super) styles: &'a mut StyleTable,
    pub(super) sst: &'a mut Vec<String>,
    pub(super) sst_idx: &'a mut HashMap<String, usize>,
    pub(super) sst_count: &'a mut usize,
    pub(super) budget: &'a mut usize,
}

pub(super) struct WorksheetXmlRelationships {
    pub(super) links: Vec<(String, String)>,
    pub(super) has_drawing: bool,
    pub(super) has_comments: bool,
    pub(super) table_count: usize,
}

impl WorksheetXmlRelationships {
    pub(super) fn is_empty(&self) -> bool {
        self.links.is_empty() && !self.has_drawing && !self.has_comments && self.table_count == 0
    }
}

pub(super) fn worksheet_xml(
    sheet: &Sheet,
    opts: WorksheetXmlOptions<'_>,
    ctx: &mut WorksheetXmlContext<'_>,
) -> (String, WorksheetXmlRelationships) {
    let WorksheetXmlOptions {
        is_active,
        sheet_num,
        has_drawing,
        has_comments,
        table_nums,
    } = opts;
    let table_count = table_nums.len();
    let styles = &mut *ctx.styles;
    let sst = &mut *ctx.sst;
    let sst_idx = &mut *ctx.sst_idx;
    let budget = &mut *ctx.budget;

    // Dedup (last-write-wins) + sort by (row, col), dropping out-of-grid cells
    // and cells under a merged range (both make Excel "repair" the file).
    let mut dedup: BTreeMap<(u32, u16), &CellEntry> = BTreeMap::new();
    for e in &sheet.cells {
        if e.row > MAX_ROW || e.col > MAX_COL || under_merge(&sheet.merges, e.row, e.col) {
            continue;
        }
        dedup.insert((e.row, e.col), e);
    }
    let entries: Vec<&CellEntry> = dedup.into_values().collect();
    let mut grid: BTreeMap<(u32, u16), GridEntry<'_>> = BTreeMap::new();
    for entry in &entries {
        grid.insert((entry.row, entry.col), GridEntry::Value(entry));
    }
    for (&(row, col), style) in &sheet.blank_styles {
        if row > MAX_ROW || col > MAX_COL || under_merge(&sheet.merges, row, col) {
            continue;
        }
        grid.insert((row, col), GridEntry::Blank { row, col, style });
    }
    for table in &sheet.tables {
        let Some(style) = sheet.table_header_formats.get(&table.name) else {
            continue;
        };
        let (row, c0, _, c1) = table.range;
        if row > MAX_ROW || c0 > MAX_COL {
            continue;
        }
        for col in c0..=c1.min(MAX_COL) {
            if under_merge(&sheet.merges, row, col) {
                continue;
            }
            grid.entry((row, col))
                .or_insert(GridEntry::Blank { row, col, style });
        }
    }

    let mut links: Vec<(String, String)> = Vec::new();
    let mut sx = String::new();
    sx.push_str(XML_DECL);
    sx.push_str(&format!(
        r#"<worksheet xmlns="{NS_MAIN}" xmlns:r="{NS_R}">"#
    ));
    // <sheetPr> (first child): tab color + outline summary direction + fit-to-page
    // flag. Merge all three into one element (Excel rejects two <sheetPr>).
    let fit_to_page = sheet
        .page_setup
        .as_ref()
        .is_some_and(|p| p.fit_to_width.is_some() || p.fit_to_height.is_some());
    let outline_pr = !sheet.outline_summary_below || !sheet.outline_summary_right;
    if sheet.tab_color.is_some() || outline_pr || fit_to_page {
        let mut sheet_pr_xml = String::from("<sheetPr>");
        if let Some(tc) = sheet.tab_color {
            sheet_pr_xml.push_str(&format!(r#"<tabColor rgb="{}"/>"#, hex(tc)));
        }
        // <outlinePr> precedes <pageSetUpPr> in CT_SheetPr. Omit each attribute
        // at its default ("1") so only the changed direction is emitted.
        if outline_pr {
            sheet_pr_xml.push_str("<outlinePr");
            if !sheet.outline_summary_below {
                sheet_pr_xml.push_str(r#" summaryBelow="0""#);
            }
            if !sheet.outline_summary_right {
                sheet_pr_xml.push_str(r#" summaryRight="0""#);
            }
            sheet_pr_xml.push_str("/>");
        }
        if fit_to_page {
            sheet_pr_xml.push_str(r#"<pageSetUpPr fitToPage="1"/>"#);
        }
        sheet_pr_xml.push_str("</sheetPr>");
        push_budgeted(&mut sx, budget, sheet_pr_xml);
    }
    // Sheet view: gridline visibility, zoom, and frozen panes (before <sheetData>).
    let pane = sheet.freeze.and_then(|(r, c)| {
        let top_left = a1(r.min(MAX_ROW), c.min(MAX_COL));
        match (r > 0, c > 0) {
            (true, true) => Some(format!(
                r#"xSplit="{c}" ySplit="{r}" topLeftCell="{top_left}" activePane="bottomRight" state="frozen""#
            )),
            (true, false) => Some(format!(
                r#"ySplit="{r}" topLeftCell="{top_left}" activePane="bottomLeft" state="frozen""#
            )),
            (false, true) => Some(format!(
                r#"xSplit="{c}" topLeftCell="{top_left}" activePane="topRight" state="frozen""#
            )),
            (false, false) => None,
        }
    });
    if sheet.hide_gridlines
        || sheet.zoom.is_some()
        || pane.is_some()
        || is_active
        || sheet.show_headers == Some(false)
        || sheet.right_to_left
    {
        let mut attrs = String::new();
        if sheet.hide_gridlines {
            attrs.push_str(r#" showGridLines="0""#);
        }
        if sheet.show_headers == Some(false) {
            attrs.push_str(r#" showRowColHeaders="0""#);
        }
        if sheet.right_to_left {
            attrs.push_str(r#" rightToLeft="1""#);
        }
        if is_active {
            attrs.push_str(r#" tabSelected="1""#);
        }
        if let Some(z) = sheet.zoom {
            attrs.push_str(&format!(r#" zoomScale="{}""#, z.clamp(10, 400)));
        }
        let mut sheet_views_xml = format!(r#"<sheetViews><sheetView{attrs} workbookViewId="0">"#);
        if let Some(p) = &pane {
            sheet_views_xml.push_str(&format!(r#"<pane {p}/>"#));
        }
        sheet_views_xml.push_str("</sheetView></sheetViews>");
        push_budgeted(&mut sx, budget, sheet_views_xml);
    }
    // <sheetFormatPr> (after sheetViews, before <cols>): default row/col sizing.
    if sheet.default_row_height.is_some() || sheet.default_col_width.is_some() {
        let drh = sheet.default_row_height.unwrap_or(15.0);
        let mut sheet_format_xml = format!(r#"<sheetFormatPr defaultRowHeight="{drh}""#);
        if let Some(dcw) = sheet.default_col_width {
            sheet_format_xml.push_str(&format!(r#" defaultColWidth="{dcw}""#));
        }
        sheet_format_xml.push_str("/>");
        push_budgeted(&mut sx, budget, sheet_format_xml);
    }
    // Column widths (skip out-of-grid columns).
    let mut widths: BTreeMap<u16, f32> = sheet
        .col_widths
        .iter()
        .filter(|(col, _)| **col <= MAX_COL)
        .map(|(c, w)| (*c, *w))
        .collect();
    if sheet.autofit {
        // Estimate a width from the widest cell text per column (Excel width
        // units ≈ character count); an explicit width already in `widths` wins.
        // Measure the deduped, in-grid, non-under-merge cells (`entries`), not
        // raw `sheet.cells`, so dropped/overwritten cells don't inflate a column.
        let mut max_len: BTreeMap<u16, usize> = BTreeMap::new();
        for &e in &entries {
            let l = e.text.chars().count();
            let m = max_len.entry(e.col).or_insert(0);
            *m = (*m).max(l);
        }
        for (c, l) in max_len {
            let est = (l as f32 + 2.0).clamp(4.0, 255.0);
            widths.entry(c).or_insert(est);
        }
    }
    let default_style = sheet.default_format.as_ref();
    let default_style_xf = default_style
        .and_then(|style| styles.intern_with_budget(Some(style), false, budget))
        .filter(|xf| *xf != 0);
    let mut col_style_xfs: BTreeMap<u16, u32> = BTreeMap::new();
    for (&col, style) in sheet.col_formats.iter().filter(|(col, _)| **col <= MAX_COL) {
        let resolved = merged_style(default_style, Some(style), None, None, None);
        let Some(xf) = styles.intern_with_budget(resolved.as_ref(), false, budget) else {
            break;
        };
        if xf != 0 {
            col_style_xfs.insert(col, xf);
        }
    }
    // Columns needing a <col>: those with a width, outline level, or default style.
    let mut col_keys: BTreeSet<u16> = widths.keys().copied().collect();
    col_keys.extend(sheet.col_outline.keys().copied().filter(|c| *c <= MAX_COL));
    col_keys.extend(col_style_xfs.keys().copied());
    if default_style_xf.is_some() || !col_keys.is_empty() {
        let mut cols_body = String::new();
        let mut emitted_cols = false;
        let mut next_default_col = 0u16;
        for col in col_keys {
            if let Some(xf) = default_style_xf {
                if next_default_col < col {
                    let xml = format!(
                        r#"<col min="{}" max="{}" style="{xf}"/>"#,
                        next_default_col + 1,
                        col
                    );
                    if !push_col_record(&mut cols_body, budget, &mut emitted_cols, xml) {
                        break;
                    }
                }
            }
            let mut attrs = format!(r#" min="{0}" max="{0}""#, col + 1);
            if let Some(w) = widths.get(&col) {
                attrs.push_str(&format!(r#" width="{w}" customWidth="1""#));
            }
            if let Some(&lvl) = sheet.col_outline.get(&col) {
                attrs.push_str(&format!(r#" outlineLevel="{lvl}""#));
            }
            if let Some(xf) = col_style_xfs.get(&col).copied().or(default_style_xf) {
                attrs.push_str(&format!(r#" style="{xf}""#));
            }
            if !push_col_record(
                &mut cols_body,
                budget,
                &mut emitted_cols,
                format!("<col{attrs}/>"),
            ) {
                break;
            }
            next_default_col = col.saturating_add(1);
        }
        if let Some(xf) = default_style_xf {
            if next_default_col <= MAX_COL {
                let xml = format!(
                    r#"<col min="{}" max="{}" style="{xf}"/>"#,
                    next_default_col + 1,
                    MAX_COL + 1
                );
                push_col_record(&mut cols_body, budget, &mut emitted_cols, xml);
            }
        }
        if emitted_cols {
            let mut cols_xml = String::from("<cols>");
            cols_xml.push_str(&cols_body);
            cols_xml.push_str("</cols>");
            sx.push_str(&cols_xml);
        }
    }
    sx.push_str("<sheetData>");
    let mut rows: BTreeSet<u32> = grid.keys().map(|(row, _)| *row).collect();
    rows.extend(sheet.row_heights.keys().copied().filter(|r| *r <= MAX_ROW));
    rows.extend(sheet.row_outline.keys().copied().filter(|r| *r <= MAX_ROW));
    rows.extend(sheet.row_formats.keys().copied().filter(|r| *r <= MAX_ROW));
    rows.extend(
        sheet
            .collapsed_rows
            .iter()
            .copied()
            .filter(|r| *r <= MAX_ROW),
    );
    'rows: for row in rows {
        let mut row_attrs = format!(r#" r="{}""#, row + 1);
        if let Some(h) = sheet.row_heights.get(&row) {
            row_attrs.push_str(&format!(r#" ht="{h}" customHeight="1""#));
        }
        let row_style_xf = if let Some(style) = sheet.row_formats.get(&row) {
            let resolved = merged_style(default_style, None, Some(style), None, None);
            match styles.intern_with_budget(resolved.as_ref(), false, budget) {
                Some(xf) => Some(xf),
                None => {
                    if *budget == 0 {
                        break 'rows;
                    }
                    None
                }
            }
        } else {
            None
        };
        if let Some(xf) = row_style_xf.filter(|xf| *xf != 0) {
            row_attrs.push_str(&format!(r#" s="{xf}" customFormat="1""#));
        }
        if let Some(&lvl) = sheet.row_outline.get(&row) {
            row_attrs.push_str(&format!(r#" outlineLevel="{lvl}""#));
        }
        if sheet.collapsed_rows.contains(&row) {
            row_attrs.push_str(r#" collapsed="1" hidden="1""#);
        }
        let row_open = format!("<row{row_attrs}>");
        let row_close = "</row>";
        if !consume_budget(budget, row_open.len().saturating_add(row_close.len())) {
            break 'rows;
        }
        sx.push_str(&row_open);
        let row_entries: Vec<GridEntry<'_>> = grid
            .range((row, 0)..=(row, MAX_COL))
            .map(|(_, entry)| *entry)
            .collect();
        for entry in row_entries {
            let col_style = sheet.col_formats.get(&entry.col());
            let row_style = sheet.row_formats.get(&row);
            let header_style = table_header_style(sheet, row, entry.col());
            let resolved_style = merged_style(
                default_style,
                col_style,
                row_style,
                header_style,
                entry.explicit_style(),
            );
            match entry {
                GridEntry::Value(e) => {
                    let is_date = needs_date_style(&e.value);
                    let Some(xf) =
                        styles.intern_with_budget(resolved_style.as_ref(), is_date, budget)
                    else {
                        if *budget == 0 {
                            sx.push_str(row_close);
                            break 'rows;
                        }
                        continue;
                    };
                    let wrote = if let Some(runs) = sheet.rich.get(&(e.row, e.col)) {
                        let before = sx.len();
                        let wrote = write_rich_cell(&mut sx, e.row, e.col, runs, xf, *budget);
                        if wrote {
                            *budget = budget.saturating_sub(sx.len() - before);
                        } else {
                            *budget = 0;
                        }
                        wrote
                    } else {
                        let mut cell_ctx = CellWriteContext {
                            sst: &mut *sst,
                            sst_idx: &mut *sst_idx,
                            sst_count: &mut *ctx.sst_count,
                            budget: &mut *budget,
                        };
                        write_cell(&mut sx, e.row, e.col, &e.value, xf, &mut cell_ctx)
                    };
                    if wrote {
                        if let Some(url) = &e.hyperlink {
                            let relationship_cost = sheet_relationship_budget_cost(
                                !links.is_empty(),
                                hyperlink_relationship_xml_len(links.len() + 1, url),
                            );
                            if consume_budget(budget, relationship_cost) {
                                links.push((a1(e.row, e.col), url.clone()));
                            }
                        }
                    }
                }
                GridEntry::Blank { row, col, .. } => {
                    let Some(xf) =
                        styles.intern_with_budget(resolved_style.as_ref(), false, budget)
                    else {
                        if *budget == 0 {
                            sx.push_str(row_close);
                            break 'rows;
                        }
                        continue;
                    };
                    write_blank_cell(&mut sx, row, col, xf, budget);
                }
            }
            if *budget == 0 {
                sx.push_str(row_close);
                break 'rows; // allocation cap reached — stop emitting cells
            }
        }
        sx.push_str(row_close);
    }
    sx.push_str("</sheetData>");
    // Post-sheetData blocks in CT_Worksheet order: sheetProtection,
    // autoFilter, mergeCells, hyperlinks, then page setup.
    if sheet.protect {
        let protection_xml = match &sheet.protect_options {
            None => r#"<sheetProtection sheet="1" objects="1" scenarios="1"/>"#.to_string(),
            Some(o) => {
                // Allowance attributes whose `"1"`/absent value means *not
                // allowed*; emit `attr="0"` only for the actions `opts` permits.
                let mut xml = r#"<sheetProtection sheet="1" objects="1" scenarios="1""#.to_string();
                for (allowed, attr) in [
                    (o.sort, "sort"),
                    (o.auto_filter, "autoFilter"),
                    (o.format_cells, "formatCells"),
                    (o.format_columns, "formatColumns"),
                    (o.format_rows, "formatRows"),
                    (o.insert_columns, "insertColumns"),
                    (o.insert_rows, "insertRows"),
                    (o.insert_hyperlinks, "insertHyperlinks"),
                    (o.delete_columns, "deleteColumns"),
                    (o.delete_rows, "deleteRows"),
                    (o.pivot_tables, "pivotTables"),
                ] {
                    if allowed {
                        xml.push(' ');
                        xml.push_str(attr);
                        xml.push_str(r#"="0""#);
                    }
                }
                xml.push_str("/>");
                xml
            }
        };
        push_budgeted(&mut sx, budget, protection_xml);
    }
    if let Some((r0, c0, r1, c1)) = sheet.autofilter {
        push_budgeted(
            &mut sx,
            budget,
            format!(
                r#"<autoFilter ref="{}:{}"/>"#,
                a1(r0.min(MAX_ROW), c0.min(MAX_COL)),
                a1(r1.min(MAX_ROW), c1.min(MAX_COL))
            ),
        );
    }
    if !sheet.merges.is_empty() {
        let mut body = String::new();
        let mut emitted = 0usize;
        let mut wrapper_len = 0usize;
        for &(r0, c0, r1, c1) in &sheet.merges {
            let next_wrapper_len = merge_cells_wrapper_len(emitted + 1);
            let xml = format!(
                r#"<mergeCell ref="{}:{}"/>"#,
                a1(r0.min(MAX_ROW), c0.min(MAX_COL)),
                a1(r1.min(MAX_ROW), c1.min(MAX_COL))
            );
            let cost = xml
                .len()
                .saturating_add(next_wrapper_len.saturating_sub(wrapper_len));
            if !consume_budget(budget, cost) {
                break;
            }
            body.push_str(&xml);
            emitted += 1;
            wrapper_len = next_wrapper_len;
        }
        if emitted != 0 {
            sx.push_str(&format!(r#"<mergeCells count="{emitted}">"#));
            sx.push_str(&body);
            sx.push_str("</mergeCells>");
        }
    }
    // conditionalFormatting (CT order: after mergeCells, before dataValidations).
    let mut emitted_conditional_formats = false;
    for (pri, cf) in sheet.cond_formats.iter().enumerate() {
        let (r0, c0, r1, c1) = cf.sqref;
        let sq = format!(
            "{}:{}",
            a1(r0.min(MAX_ROW), c0.min(MAX_COL)),
            a1(r1.min(MAX_ROW), c1.min(MAX_COL))
        );
        let mut cf_xml = String::new();
        let p = pri + 1;
        cf_xml.push_str(&format!(r#"<conditionalFormatting sqref="{sq}">"#));
        match &cf.rule {
            CfRule::CellIs {
                op,
                formula1,
                formula2,
                fill,
            } => {
                let Some(dxf) =
                    intern_conditional_dxf(styles, *fill, budget, emitted_conditional_formats)
                else {
                    break;
                };
                cf_xml.push_str(&format!(
                    r#"<cfRule type="cellIs" dxfId="{dxf}" priority="{p}" operator="{}">"#,
                    dv_op_str(*op)
                ));
                cf_xml.push_str(&format!("<formula>{}</formula>", esc_text(formula1)));
                if let Some(f2) = formula2 {
                    cf_xml.push_str(&format!("<formula>{}</formula>", esc_text(f2)));
                }
                cf_xml.push_str("</cfRule>");
            }
            CfRule::ColorScale2 { min, max } => {
                cf_xml.push_str(&format!(
                    r#"<cfRule type="colorScale" priority="{p}"><colorScale><cfvo type="min"/><cfvo type="max"/><color rgb="{}"/><color rgb="{}"/></colorScale></cfRule>"#,
                    hex(*min), hex(*max)
                ));
            }
            CfRule::ColorScale3 { min, mid, max } => {
                cf_xml.push_str(&format!(
                    r#"<cfRule type="colorScale" priority="{p}"><colorScale><cfvo type="min"/><cfvo type="percentile" val="50"/><cfvo type="max"/><color rgb="{}"/><color rgb="{}"/><color rgb="{}"/></colorScale></cfRule>"#,
                    hex(*min), hex(*mid), hex(*max)
                ));
            }
            CfRule::DataBar { color } => {
                cf_xml.push_str(&format!(
                    r#"<cfRule type="dataBar" priority="{p}"><dataBar><cfvo type="min"/><cfvo type="max"/><color rgb="{}"/></dataBar></cfRule>"#,
                    hex(*color)
                ));
            }
            CfRule::TopBottom {
                rank,
                bottom,
                percent,
                fill,
            } => {
                let Some(dxf) =
                    intern_conditional_dxf(styles, *fill, budget, emitted_conditional_formats)
                else {
                    break;
                };
                let b = if *bottom { r#" bottom="1""# } else { "" };
                let pct = if *percent { r#" percent="1""# } else { "" };
                cf_xml.push_str(&format!(
                    r#"<cfRule type="top10" dxfId="{dxf}" priority="{p}" rank="{rank}"{b}{pct}/>"#
                ));
            }
            CfRule::AboveAverage { below, fill } => {
                let Some(dxf) =
                    intern_conditional_dxf(styles, *fill, budget, emitted_conditional_formats)
                else {
                    break;
                };
                let a = if *below { r#" aboveAverage="0""# } else { "" };
                cf_xml.push_str(&format!(
                    r#"<cfRule type="aboveAverage" dxfId="{dxf}" priority="{p}"{a}/>"#
                ));
            }
            CfRule::DuplicateValues { unique, fill } => {
                let Some(dxf) =
                    intern_conditional_dxf(styles, *fill, budget, emitted_conditional_formats)
                else {
                    break;
                };
                let ty = if *unique {
                    "uniqueValues"
                } else {
                    "duplicateValues"
                };
                cf_xml.push_str(&format!(
                    r#"<cfRule type="{ty}" dxfId="{dxf}" priority="{p}"/>"#
                ));
            }
            CfRule::Expression { formula, fill } => {
                let Some(dxf) =
                    intern_conditional_dxf(styles, *fill, budget, emitted_conditional_formats)
                else {
                    break;
                };
                cf_xml.push_str(&format!(
                    r#"<cfRule type="expression" dxfId="{dxf}" priority="{p}"><formula>{}</formula></cfRule>"#,
                    esc_text(formula)
                ));
            }
        }
        cf_xml.push_str("</conditionalFormatting>");
        if cf_xml.len() > *budget {
            if !emitted_conditional_formats {
                *budget = 0;
            }
            break;
        }
        *budget -= cf_xml.len();
        sx.push_str(&cf_xml);
        emitted_conditional_formats = true;
    }
    if !sheet.data_validations.is_empty() {
        let mut body = String::new();
        let mut emitted = 0usize;
        let mut wrapper_len = 0usize;
        for dv in &sheet.data_validations {
            let next_wrapper_len = data_validations_wrapper_len(emitted + 1);
            let xml = data_validation_xml(dv);
            let cost = xml
                .len()
                .saturating_add(next_wrapper_len.saturating_sub(wrapper_len));
            if cost > *budget {
                if emitted == 0 {
                    *budget = 0;
                }
                break;
            }
            *budget -= cost;
            body.push_str(&xml);
            emitted += 1;
            wrapper_len = next_wrapper_len;
        }
        if emitted != 0 {
            sx.push_str(&format!(r#"<dataValidations count="{emitted}">"#));
            sx.push_str(&body);
            sx.push_str("</dataValidations>");
        }
    }
    if !links.is_empty() {
        let mut body = String::new();
        let mut emitted = 0usize;
        for (j, (cref, _)) in links.iter().enumerate() {
            let wrapper_cost = if emitted == 0 {
                hyperlinks_wrapper_len()
            } else {
                0
            };
            let xml = format!(r#"<hyperlink ref="{cref}" r:id="rId{}"/>"#, j + 1);
            if !consume_budget(budget, xml.len().saturating_add(wrapper_cost)) {
                break;
            }
            body.push_str(&xml);
            emitted += 1;
        }
        if emitted != 0 {
            let mut hx = String::from("<hyperlinks>");
            hx.push_str(&body);
            hx.push_str("</hyperlinks>");
            sx.push_str(&hx);
            links.truncate(emitted);
        } else {
            links.clear();
        }
    }
    // <printOptions> (CT_Worksheet order: after hyperlinks, before pageMargins).
    // Centering comes from page_setup; gridlines/headings from the sheet — merge
    // into a single element (Excel rejects two printOptions).
    let center_h = sheet
        .page_setup
        .as_ref()
        .is_some_and(|p| p.center_horizontally);
    let center_v = sheet
        .page_setup
        .as_ref()
        .is_some_and(|p| p.center_vertically);
    if sheet.print_gridlines || sheet.print_headings || center_h || center_v {
        let mut po = String::new();
        if sheet.print_gridlines {
            po.push_str(r#" gridLines="1""#);
        }
        if sheet.print_headings {
            po.push_str(r#" headings="1""#);
        }
        if center_h {
            po.push_str(r#" horizontalCentered="1""#);
        }
        if center_v {
            po.push_str(r#" verticalCentered="1""#);
        }
        push_budgeted(&mut sx, budget, format!(r#"<printOptions{po}/>"#));
    }
    // Page setup (CT_Worksheet order: pageMargins, pageSetup, headerFooter).
    if let Some(ps) = &sheet.page_setup {
        let mut emitted_page_setup_record = false;
        let mut push_page_setup_record =
            |sx: &mut String, budget: &mut usize, xml: String| -> bool {
                if xml.len() > *budget {
                    if !emitted_page_setup_record {
                        *budget = 0;
                    }
                    return false;
                }
                *budget -= xml.len();
                sx.push_str(&xml);
                emitted_page_setup_record = true;
                true
            };
        let (l, r, t, b, h, f) = ps.margins.unwrap_or((0.7, 0.7, 0.75, 0.75, 0.3, 0.3));
        if push_page_setup_record(
            &mut sx,
            budget,
            format!(
                r#"<pageMargins left="{l}" right="{r}" top="{t}" bottom="{b}" header="{h}" footer="{f}"/>"#
            ),
        ) {
            let orient = if ps.landscape {
                "landscape"
            } else {
                "portrait"
            };
            let mut setup = format!(r#" orientation="{orient}""#);
            if let Some(p) = ps.paper_size {
                setup.push_str(&format!(r#" paperSize="{p}""#));
            }
            if let Some(s) = ps.scale {
                setup.push_str(&format!(r#" scale="{}""#, s.clamp(10, 400)));
            }
            if let Some(w) = ps.fit_to_width {
                setup.push_str(&format!(r#" fitToWidth="{w}""#));
            }
            if let Some(ht) = ps.fit_to_height {
                setup.push_str(&format!(r#" fitToHeight="{ht}""#));
            }
            if let Some(fpn) = ps.first_page_number {
                setup.push_str(&format!(
                    r#" firstPageNumber="{fpn}" useFirstPageNumber="1""#
                ));
            }
            if push_page_setup_record(&mut sx, budget, format!(r#"<pageSetup{setup}/>"#))
                && (ps.header.is_some() || ps.footer.is_some())
            {
                let mut header_footer_xml = String::from("<headerFooter>");
                if let Some(hd) = &ps.header {
                    header_footer_xml.push_str(&format!("<oddHeader>{}</oddHeader>", esc_text(hd)));
                }
                if let Some(ft) = &ps.footer {
                    header_footer_xml.push_str(&format!("<oddFooter>{}</oddFooter>", esc_text(ft)));
                }
                header_footer_xml.push_str("</headerFooter>");
                push_page_setup_record(&mut sx, budget, header_footer_xml);
            }
        }
    }
    // Rel-id allocation (mirrors `to_xlsx` in mod.rs): hyperlinks rId1..K, then
    // the drawing rel, then the comments + vmlDrawing rels, then the table rels.
    // <drawing> (images + charts) — references the sheet's drawing part; its
    // rel id follows the hyperlink rels.
    let mut rel_count = links.len();
    let mut has_sheet_relationships = rel_count != 0;
    let mut emitted_has_drawing = false;
    let mut emitted_has_comments = false;
    let mut emitted_table_count = 0usize;
    if has_drawing {
        let drawing_rid = rel_count + 1;
        let drawing_xml = format!(r#"<drawing r:id="rId{drawing_rid}"/>"#);
        let drawing_target = format!("../drawings/drawing{sheet_num}.xml");
        let relationship_cost =
            sheet_relationship_xml_len(drawing_rid, REL_DRAWING, &drawing_target);
        let cost = drawing_xml
            .len()
            .saturating_add(sheet_relationship_budget_cost(
                has_sheet_relationships,
                relationship_cost,
            ));
        if consume_optional_record_budget(budget, cost, has_sheet_relationships) {
            sx.push_str(&drawing_xml);
            emitted_has_drawing = true;
            rel_count += 1;
            has_sheet_relationships = true;
        }
    }
    // <legacyDrawing> (CT_Worksheet order: after pageSetup/headerFooter and the
    // drawing, before tableParts) — points at the vmlDrawing rel, which is the
    // second of the two comment rels (comments rel first, vmlDrawing rel second).
    if has_comments {
        let comments_rid = rel_count + 1;
        let vml_rid = rel_count + 2;
        let legacy_xml = format!(r#"<legacyDrawing r:id="rId{vml_rid}"/>"#);
        let comments_target = format!("../comments{sheet_num}.xml");
        let vml_target = format!("../drawings/vmlDrawing{sheet_num}.vml");
        let relationship_cost =
            sheet_relationship_xml_len(comments_rid, REL_COMMENTS, &comments_target)
                .saturating_add(sheet_relationship_xml_len(
                    vml_rid,
                    REL_VML_DRAWING,
                    &vml_target,
                ));
        let cost = legacy_xml
            .len()
            .saturating_add(sheet_relationship_budget_cost(
                has_sheet_relationships,
                relationship_cost,
            ));
        if consume_optional_record_budget(budget, cost, has_sheet_relationships) {
            sx.push_str(&legacy_xml);
            emitted_has_comments = true;
            rel_count += 2;
            has_sheet_relationships = true;
        }
    }
    // <tableParts> — rel ids follow the hyperlinks, drawing, and comment rels.
    if table_count != 0 {
        let base = rel_count;
        let mut body = String::new();
        let mut emitted = 0usize;
        let mut wrapper_len = 0usize;
        for table_num in table_nums {
            let rid = base + emitted + 1;
            let xml = format!(r#"<tablePart r:id="rId{rid}"/>"#);
            let target = format!("../tables/table{table_num}.xml");
            let relationship_cost = sheet_relationship_xml_len(rid, REL_TABLE, &target);
            let next_wrapper_len = table_parts_wrapper_len(emitted + 1);
            let sheet_relationship_wrapper_cost = if has_sheet_relationships || emitted != 0 {
                0
            } else {
                sheet_relationships_wrapper_len()
            };
            let cost = xml
                .len()
                .saturating_add(relationship_cost)
                .saturating_add(next_wrapper_len.saturating_sub(wrapper_len))
                .saturating_add(sheet_relationship_wrapper_cost);
            if !consume_optional_record_budget(
                budget,
                cost,
                has_sheet_relationships || emitted != 0,
            ) {
                break;
            }
            body.push_str(&xml);
            emitted += 1;
            wrapper_len = next_wrapper_len;
        }
        if emitted != 0 {
            let mut table_parts_xml = format!(r#"<tableParts count="{emitted}">"#);
            table_parts_xml.push_str(&body);
            table_parts_xml.push_str("</tableParts>");
            sx.push_str(&table_parts_xml);
            emitted_table_count = emitted;
        }
    }
    if !sheet.sparklines.is_empty() {
        let sparkline_xml = sparklines_xml_with_budget(sheet, budget);
        if !sparkline_xml.is_empty() {
            sx.push_str(&sparkline_xml);
        }
    }
    sx.push_str("</worksheet>");
    (
        sx,
        WorksheetXmlRelationships {
            links,
            has_drawing: emitted_has_drawing,
            has_comments: emitted_has_comments,
            table_count: emitted_table_count,
        },
    )
}
