use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::OnceLock;

use super::{
    display_text_with_num_fmt, Cell, CellErrorType, CellStyle, CellStyleOverlay, Chart, Color,
    Comment, CommentAuthor, CondFormat, ConditionalFormatMetadata, DataValidation, Dimensions,
    DrawingMetadata, Format, FormulaRange, HeaderFooterKind, Image, ImportedAxisMeasure,
    OoxmlImplicitColumnWidth, OoxmlImplicitRowHeight, PageSetup, PrintMetadata, ProtectionOptions,
    Range, SheetType, SheetView, SheetVisible, Sparkline, StyleFidelity, StyleLoss, Table,
    TableStyleApplication, TextRun, WorksheetMetadata, XlsbDefaultColumnWidth,
};
#[derive(Debug, Clone)]
pub(crate) struct CellEntry {
    pub(crate) row: u32,
    pub(crate) col: u16,
    pub(crate) value: Cell,
    /// Display text used by [`Sheet::to_text`] (e.g. `50%`, `TRUE`).
    pub(crate) text: String,
    /// Inline authoring style (`None` for cells produced by the reader). Read
    /// only by the `.xlsx` writer.
    #[cfg_attr(not(feature = "xlsx"), allow(dead_code))]
    pub(crate) style: Option<CellStyle>,
    /// Exact integral XLSX source font size for this cell's retained XF.
    ///
    /// This private provenance is absent for authored cells, other container
    /// formats, and ambiguous/fractional/invalid XLSX style records.
    pub(crate) xlsx_font_size_pt: Option<u16>,
    /// External hyperlink target (authoring). Read only by the `.xlsx` writer.
    #[cfg_attr(not(feature = "xlsx"), allow(dead_code))]
    pub(crate) hyperlink: Option<String>,
}

/// One deduplicated, display-ready worksheet cell.
///
/// Values follow Excel's last-write-wins coordinate semantics and carry the
/// retained display text, explicit cell style, rich runs, and hyperlink needed
/// by exporters and external rendering engines. Worksheet/column/row/table
/// style inheritance remains available through [`Sheet::resolved_cell_style`].
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct DisplayCell<'a> {
    /// Zero-based row index.
    pub row: u32,
    /// Zero-based column index.
    pub col: u16,
    /// Typed cell value.
    pub value: &'a Cell,
    /// Display-formatted text retained by the reader or authoring model.
    pub formatted: &'a str,
    /// Style stored directly on this cell, before inherited layers.
    pub explicit_style: Option<&'a CellStyle>,
    /// Retained rich-text runs, when present.
    pub rich_text: Option<&'a [TextRun]>,
    /// External hyperlink target retained for this coordinate.
    pub hyperlink: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DisplayCellIndexEntry {
    coordinate: u64,
    source_index: usize,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DisplayCellIndex {
    pub(super) cells: Vec<DisplayCellIndexEntry>,
    pub(super) read_hyperlinks: Vec<DisplayCellIndexEntry>,
}

impl DisplayCellIndex {
    fn packed_coordinate(row: u32, col: u16) -> u64 {
        (u64::from(row) << 16) | u64::from(col)
    }

    fn compact_last_by_coordinate(
        mut entries: Vec<DisplayCellIndexEntry>,
    ) -> Vec<DisplayCellIndexEntry> {
        entries.sort_unstable_by_key(|entry| (entry.coordinate, entry.source_index));
        let mut retained = 0_usize;
        for read in 0..entries.len() {
            let entry = entries[read];
            if retained > 0 && entries[retained - 1].coordinate == entry.coordinate {
                entries[retained - 1] = entry;
            } else {
                entries[retained] = entry;
                retained += 1;
            }
        }
        entries.truncate(retained);
        entries.shrink_to_fit();
        entries
    }

    fn source_index(&self, row: u32, col: u16) -> Option<usize> {
        let coordinate = Self::packed_coordinate(row, col);
        self.cells
            .binary_search_by_key(&coordinate, |entry| entry.coordinate)
            .ok()
            .map(|index| self.cells[index].source_index)
    }

    fn read_hyperlink_index(&self, row: u32, col: u16) -> Option<usize> {
        let coordinate = Self::packed_coordinate(row, col);
        self.read_hyperlinks
            .binary_search_by_key(&coordinate, |entry| entry.coordinate)
            .ok()
            .map(|index| self.read_hyperlinks[index].source_index)
    }

    fn source_indices_in_range(
        &self,
        first_row: u32,
        first_col: u16,
        last_row: u32,
        last_col: u16,
    ) -> impl Iterator<Item = usize> + '_ {
        let (start, end) = if first_row <= last_row && first_col <= last_col {
            let first = Self::packed_coordinate(first_row, 0);
            let last = Self::packed_coordinate(last_row, u16::MAX);
            (
                self.cells.partition_point(|entry| entry.coordinate < first),
                self.cells.partition_point(|entry| entry.coordinate <= last),
            )
        } else {
            (0, 0)
        };
        self.cells[start..end]
            .iter()
            .filter(move |entry| {
                let col = entry.coordinate as u16;
                col >= first_col && col <= last_col
            })
            .map(|entry| entry.source_index)
    }
}

/// One worksheet: a name, its non-empty cells, and layout/structure (authoring).
#[derive(Debug, Clone)]
pub struct Sheet {
    /// Sheet name as stored in the workbook globals (`BOUNDSHEET`).
    pub name: String,
    /// Whether this is an actual worksheet (vs. a chart/macro sheet).
    pub is_worksheet: bool,
    /// Style-retention fidelity for renderer/exporter diagnostics.
    pub(crate) style_fidelity: StyleFidelity,
    /// Date system used when formatting authored date cells. Workbook-created
    /// sheets inherit the workbook flag at creation time.
    pub(crate) display_date1904: bool,
    /// Parsed sheet type for metadata when the source format exposes it.
    pub(crate) sheet_type: Option<SheetType>,
    pub(crate) cells: Vec<CellEntry>,
    /// Lazily built compact last-write-wins coordinate index used by whole-sheet,
    /// range, and point display access. Readers finish populating `cells` before
    /// any public access; authoring writes invalidate this cache before appending
    /// a new record.
    pub(crate) display_cell_index: OnceLock<DisplayCellIndex>,
    /// Per-column widths in character units, populated by readers and authoring.
    pub(crate) col_widths: BTreeMap<u16, f32>,
    /// Raw XLSB `BrtColInfo.coldx` widths parallel to imported `col_widths`.
    pub(crate) xlsb_col_widths_256: BTreeMap<u16, u32>,
    /// Applicable XLSB sheet-wide width provenance, absent for other formats.
    pub(crate) xlsb_default_col_width: Option<XlsbDefaultColumnWidth>,
    /// Whether a BIFF worksheet omitted every valid sheet-wide width record
    /// and therefore uses Calc's fixed application default.
    pub(crate) biff_application_default_col_width: bool,
    /// Whether a BIFF worksheet omitted every valid sheet-wide row-height
    /// record and therefore uses Calc's fixed application default.
    pub(crate) biff_application_default_row_height: bool,
    /// Source column widths expressed in physical points when the format stores
    /// an absolute length (currently ODS). Renderers prefer these values over
    /// the compatibility character-unit projection in `col_widths`.
    pub(crate) physical_col_widths: BTreeMap<u16, f32>,
    /// Exact imported per-column source geometry used only by renderers.
    pub(crate) imported_column_axis_measures: BTreeMap<u16, ImportedAxisMeasure>,
    /// Exact imported sheet-wide column geometry used only by renderers.
    pub(crate) imported_default_column_axis_measure: Option<ImportedAxisMeasure>,
    /// Per-row heights in points, populated by readers and authoring.
    pub(crate) row_heights: BTreeMap<u32, f32>,
    /// Imported rows whose retained height is an automatic cached value.
    ///
    /// XLSX rows without `customHeight` and ODS rows whose resolved row style
    /// enables `style:use-optimal-row-height` remain eligible for automatic
    /// expansion. XLSB accepts `BrtRowHdr.miyRw` only when its `fUnsynced` flag
    /// is set and therefore retains only authoritative per-row heights.
    pub(crate) automatic_row_height_candidates: BTreeSet<u32>,
    /// Exact imported per-row source geometry used only by renderers.
    pub(crate) imported_row_axis_measures: BTreeMap<u32, ImportedAxisMeasure>,
    /// Exact imported sheet-wide row geometry used only by renderers.
    pub(crate) imported_default_row_axis_measure: Option<ImportedAxisMeasure>,
    /// Explicitly hidden columns.
    pub(crate) hidden_cols: BTreeSet<u16>,
    /// Explicitly hidden rows.
    pub(crate) hidden_rows: BTreeSet<u32>,
    /// OOXML sheet-wide default-hidden-row provenance. `Some(exceptions)`
    /// means unspecified rows are hidden and the bounded set contains rows
    /// explicitly present as visible in the source worksheet.
    pub(crate) default_hidden_row_exceptions: Option<BTreeSet<u32>>,
    /// Per-column default formats (authoring).
    pub(crate) col_formats: BTreeMap<u16, CellStyle>,
    /// Per-row default formats (authoring).
    pub(crate) row_formats: BTreeMap<u32, CellStyle>,
    /// Worksheet default format (authoring), applied below column/row/cell formats.
    pub(crate) default_format: Option<CellStyle>,
    /// Format-only blank cells (authoring), separate from typed reader cells.
    pub(crate) blank_styles: BTreeMap<(u32, u16), CellStyle>,
    /// Default row height in points (authoring); `<sheetFormatPr defaultRowHeight>`.
    pub(crate) default_row_height: Option<f32>,
    /// Whether a retained source default row height is an automatic baseline.
    pub(crate) automatic_default_row_height_candidate: bool,
    /// Default column width in character units (authoring).
    pub(crate) default_col_width: Option<f32>,
    /// OOXML-only provenance used to distinguish an absent application default
    /// from an explicit `defaultColWidth` and from `baseColWidth`.
    pub(crate) ooxml_implicit_col_width: OoxmlImplicitColumnWidth,
    /// Whether a present XLSX `sheetFormatPr` omitted `baseColWidth`, causing
    /// Calc's importer to apply the element's defaulted eight-digit base plus
    /// five screen pixels instead of the separate 8.5-digit constructor
    /// default used when the entire element is absent.
    pub(crate) ooxml_defaulted_base_col_width: bool,
    /// OOXML-only provenance used to distinguish an absent application default
    /// from an explicit `defaultRowHeight` / XLSB `miyDefRwHeight`.
    pub(crate) ooxml_implicit_row_height: OoxmlImplicitRowHeight,
    /// Exact integral XLSX Normal-style font size retained only when
    /// `cellXfs[0]` and the named/built-in Normal `cellStyleXf` resolve to the
    /// same source font. Other formats and ambiguous/invalid XLSX style tables
    /// retain `None`.
    pub(crate) xlsx_normal_font_size_pt: Option<u16>,
    /// Exact integral XLSB Normal-style font size retained only when the
    /// binary Fonts, CellStyleXFs, CellXFs, and Styles collections are
    /// structurally complete and their default font sources agree.
    pub(crate) xlsb_normal_font_size_pt: Option<u16>,
    /// Exact integral XLSB source font size parallel to each retained cell.
    ///
    /// Entries are absent for non-XLSB cells and for incomplete, malformed,
    /// fractional, out-of-range, or ambiguous binary style provenance.
    pub(crate) xlsb_cell_font_sizes_pt: Vec<Option<u16>>,
    /// Exact integral XLSB font sizes for retained row XF layers.
    pub(crate) xlsb_row_font_sizes_pt: BTreeMap<u32, u16>,
    /// Exact integral XLSB font sizes for retained column XF layers.
    pub(crate) xlsb_col_font_sizes_pt: BTreeMap<u16, u16>,
    /// Merged ranges `(r0, c0, r1, c1)` set when **authoring** (via
    /// [`Sheet::merge`]). The writer emits these as `<mergeCells>` and omits
    /// cells under them for OOXML conformance.
    pub(crate) merges: Vec<(u32, u16, u32, u16)>,
    /// Merged ranges discovered when **reading** a file (`.xls MERGECELLS` /
    /// `.xlsx <mergeCells>`). Kept separate from [`Self::merges`] so surfacing
    /// them via [`Sheet::merged_ranges`] never makes the writer drop the source's
    /// cells on a read→write — extraction stays full-fidelity.
    pub(crate) read_merges: Vec<(u32, u16, u32, u16)>,
    /// External hyperlinks discovered when **reading** a file (for example `.xlsx`
    /// worksheet rels or `.ods` `text:a` links). Each entry is `(row, col, url)`,
    /// 0-based. Kept separate from the per-cell authoring [`CellEntry::hyperlink`]
    /// used by the writer so surfacing them via [`Sheet::hyperlinks`] never
    /// disturbs authoring state.
    pub(crate) read_hyperlinks: Vec<(u32, u16, String)>,
    /// Frozen panes split at `(row, col)` (authoring).
    pub(crate) freeze: Option<(u32, u16)>,
    /// Autofilter range `(r0, c0, r1, c1)` (authoring).
    pub(crate) autofilter: Option<(u32, u16, u32, u16)>,
    /// Print/page setup (authoring).
    pub(crate) page_setup: Option<PageSetup>,
    /// Loss-aware source print metadata retained beside the compatibility setup.
    pub(crate) print_metadata: PrintMetadata,
    /// Sheet tab color (authoring).
    pub(crate) tab_color: Option<Color>,
    /// Worksheet protection (authoring): lock cells against editing.
    pub(crate) protect: bool,
    /// Granular protection allowances (authoring); only consulted when
    /// [`Self::protect`] is set. `None` = lock everything (the `protect()`
    /// default).
    pub(crate) protect_options: Option<ProtectionOptions>,
    /// Data validations (authoring): dropdowns / numeric constraints.
    pub(crate) data_validations: Vec<DataValidation>,
    /// Conditional formats (authoring).
    pub(crate) cond_formats: Vec<CondFormat>,
    /// Read-side rule priority, stop behavior, and differential styles aligned
    /// with `cond_formats` when available.
    pub(crate) cond_format_metadata: Vec<ConditionalFormatMetadata>,
    /// Embedded images (authoring).
    pub(crate) images: Vec<Image>,
    /// Charts (authoring).
    pub(crate) charts: Vec<Chart>,
    /// Read-side drawing geometry and accessibility metadata. Kept separate so
    /// the long-standing [`Image`] and [`Chart`] structs remain source-compatible.
    pub(crate) drawing_metadata: Vec<DrawingMetadata>,
    /// Aggregated source-style loss boundaries.
    pub(crate) style_losses: Vec<StyleLoss>,
    /// Sparklines (authoring): compact in-cell charts emitted as x14 worksheet
    /// extensions.
    pub(crate) sparklines: Vec<Sparkline>,
    /// Worksheet tables (authoring).
    pub(crate) tables: Vec<Table>,
    /// Per-table header row formats keyed by the authored table name.
    pub(crate) table_header_formats: BTreeMap<String, CellStyle>,
    /// Loss-aware table-region styles retained from OOXML table definitions.
    ///
    /// This is separate from `table_header_formats`: the latter is a stable
    /// authoring/public compatibility surface, while this sidecar retains the
    /// complete bounded read-side cascade without changing [`Table`]'s public
    /// struct shape.
    pub(crate) table_region_formats: BTreeMap<String, TableStyleApplication>,
    /// Sparse direct-cell overlays retained by readers that can distinguish a
    /// cell XF's explicitly applied components from its resolved base style.
    /// These overlays are applied after table styles.
    pub(crate) direct_cell_formats: BTreeMap<(u32, u16), CellStyleOverlay>,
    /// Legacy cell comments / notes (authoring).
    pub(crate) comments: Vec<Comment>,
    /// Rich (mixed-format) string cells (authoring): coordinate → runs. Emitted as
    /// an inline rich string; the plain concatenation also lives in `cells` so
    /// position/merge logic and readers see a value.
    pub(crate) rich: BTreeMap<(u32, u16), Vec<TextRun>>,
    /// Hide the worksheet gridlines (authoring).
    pub(crate) hide_gridlines: bool,
    /// Sheet zoom as a percentage, e.g. `150` (authoring).
    pub(crate) zoom: Option<u16>,
    /// Show the row and column headers in the sheet view (authoring). `None`
    /// leaves Excel's default (shown); `Some(false)` emits
    /// `<sheetView showRowColHeaders="0">`.
    pub(crate) show_headers: Option<bool>,
    /// Lay the sheet out right-to-left (authoring): `<sheetView rightToLeft="1">`.
    pub(crate) right_to_left: bool,
    /// Hide this worksheet in the workbook: `<sheet state="hidden">`. Set when
    /// authoring (via [`Sheet::hide`]) and on read from every format, so a
    /// read→write round-trip preserves visibility. Surfaced by [`Sheet::is_hidden`].
    pub(crate) hidden: bool,
    /// The worksheet is *very* hidden (`<sheet state="veryHidden">`) — only
    /// unhideable via VBA, not the Excel UI. Set when authoring via
    /// [`Sheet::hide_very`], populated by the reader (`.xlsx` `state`,
    /// `.xls`/`.xlsb` `hsState == 2`), and surfaced by
    /// [`Sheet::is_very_hidden`].
    pub(crate) very_hidden: bool,
    /// Auto-size column widths from cell text on write (authoring).
    pub(crate) autofit: bool,
    /// Row outline (grouping) levels (authoring): row → outline depth.
    pub(crate) row_outline: BTreeMap<u32, u8>,
    /// Column outline (grouping) levels (authoring): col → outline depth.
    pub(crate) col_outline: BTreeMap<u16, u8>,
    /// Print the gridlines on the printed page (authoring).
    pub(crate) print_gridlines: bool,
    /// Print the row and column headings on the printed page (authoring).
    pub(crate) print_headings: bool,
    /// Outline summary rows appear *below* the grouped detail rows (authoring);
    /// the Excel default. When `false`, emits `<outlinePr summaryBelow="0"/>`.
    pub(crate) outline_summary_below: bool,
    /// Outline summary columns appear to the *right* of the grouped detail
    /// columns (authoring); the Excel default. When `false`, emits
    /// `<outlinePr summaryRight="0"/>`.
    pub(crate) outline_summary_right: bool,
    /// Rows whose group is collapsed (authoring): the summary row stays visible
    /// (`<row collapsed="1" hidden="1">`) while its detail rows are hidden.
    pub(crate) collapsed_rows: BTreeSet<u32>,
}

struct RenderStyleSidecarIdentity<'a> {
    sheet: &'a Sheet,
    ranges: RenderStyleRangeIndex,
}

struct RelevantTableRegionFormats<'a> {
    sheet: &'a Sheet,
    ranges: &'a RenderStyleRangeIndex,
}

impl fmt::Debug for RelevantTableRegionFormats<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = formatter.debug_list();
        for (table_index, table) in self.sheet.tables.iter().enumerate() {
            let Some(application) = self.sheet.table_region_formats.get(&table.name) else {
                continue;
            };
            if self.ranges.intersects(table.range) {
                list.entry(&(table_index, &table.name, application));
            }
        }
        list.finish()
    }
}

struct RelevantDirectCellFormats<'a> {
    sheet: &'a Sheet,
    ranges: &'a RenderStyleRangeIndex,
}

impl fmt::Debug for RelevantDirectCellFormats<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = formatter.debug_list();
        self.ranges.for_each_relevant_direct_cell_format(
            &self.sheet.direct_cell_formats,
            |row, col, overlay| {
                list.entry(&(row, col, overlay));
            },
        );
        list.finish()
    }
}

impl fmt::Debug for RenderStyleSidecarIdentity<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RenderStyleSidecarIdentity")
            .field(
                "table_region_formats",
                &RelevantTableRegionFormats {
                    sheet: self.sheet,
                    ranges: &self.ranges,
                },
            )
            .field(
                "direct_cell_formats",
                &RelevantDirectCellFormats {
                    sheet: self.sheet,
                    ranges: &self.ranges,
                },
            )
            .finish()
    }
}

pub(super) type RenderStyleRange = (u32, u16, u32, u16);

#[derive(Clone, Copy)]
struct RenderStyleRowEvent {
    row: u64,
    first_col_index: usize,
    after_last_col_index: usize,
    delta: i32,
}

pub(super) struct RenderStyleRangeIndex {
    ranges: Vec<RenderStyleRange>,
    row_events: Vec<RenderStyleRowEvent>,
    column_boundaries: Vec<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct RenderStyleTraversalStats {
    pub(super) direct_entries_visited: u64,
    pub(super) membership_queries: u64,
    pub(super) row_events_applied: u64,
    pub(super) selected_entries: u64,
}

struct ColumnCoverageTree {
    values: Vec<i32>,
}

impl ColumnCoverageTree {
    fn new(boundary_count: usize) -> Self {
        Self {
            values: vec![0; boundary_count.saturating_add(1)],
        }
    }

    fn add(&mut self, boundary_index: usize, delta: i32) {
        let mut index = boundary_index.saturating_add(1);
        while index < self.values.len() {
            self.values[index] += delta;
            index = index.saturating_add(index & index.wrapping_neg());
        }
    }

    fn prefix_sum(&self, mut boundary_count: usize) -> i32 {
        let mut value = 0_i32;
        while boundary_count > 0 {
            value += self.values[boundary_count];
            boundary_count &= boundary_count - 1;
        }
        value
    }
}

impl RenderStyleRangeIndex {
    pub(super) fn new(ranges: &[RenderStyleRange]) -> Self {
        let mut ranges = ranges
            .iter()
            .copied()
            .filter(|range| range.0 <= range.2 && range.1 <= range.3)
            .collect::<Vec<_>>();
        ranges.sort_unstable();
        ranges.dedup();

        let mut column_boundaries = Vec::with_capacity(ranges.len().saturating_mul(2));
        let mut raw_events = Vec::with_capacity(ranges.len().saturating_mul(2));
        for &(first_row, first_col, last_row, last_col) in &ranges {
            let first_col = u32::from(first_col);
            let after_last_col = u32::from(last_col) + 1;
            column_boundaries.push(first_col);
            column_boundaries.push(after_last_col);
            raw_events.push((u64::from(first_row), first_col, after_last_col, 1_i32));
            raw_events.push((u64::from(last_row) + 1, first_col, after_last_col, -1_i32));
        }
        column_boundaries.sort_unstable();
        column_boundaries.dedup();
        raw_events.sort_unstable_by_key(|event| (event.0, event.1, event.2, event.3));

        let mut row_events = Vec::<RenderStyleRowEvent>::with_capacity(raw_events.len());
        for (row, first_col, after_last_col, delta) in raw_events {
            let first_col_index = column_boundaries
                .binary_search(&first_col)
                .expect("style range start is indexed");
            let after_last_col_index = column_boundaries
                .binary_search(&after_last_col)
                .expect("style range end is indexed");
            if let Some(previous) = row_events.last_mut() {
                if previous.row == row
                    && previous.first_col_index == first_col_index
                    && previous.after_last_col_index == after_last_col_index
                {
                    previous.delta += delta;
                    if previous.delta == 0 {
                        row_events.pop();
                    }
                    continue;
                }
            }
            row_events.push(RenderStyleRowEvent {
                row,
                first_col_index,
                after_last_col_index,
                delta,
            });
        }

        Self {
            ranges,
            row_events,
            column_boundaries,
        }
    }

    fn intersects(&self, candidate: RenderStyleRange) -> bool {
        let (first_row, first_col, last_row, last_col) = candidate;
        first_row <= last_row
            && first_col <= last_col
            && self.ranges.iter().any(|range| {
                first_row <= range.2
                    && last_row >= range.0
                    && first_col <= range.3
                    && last_col >= range.1
            })
    }

    pub(super) fn for_each_relevant_direct_cell_format(
        &self,
        formats: &BTreeMap<(u32, u16), CellStyleOverlay>,
        mut visit: impl FnMut(u32, u16, &CellStyleOverlay),
    ) -> RenderStyleTraversalStats {
        let mut stats = RenderStyleTraversalStats::default();
        let mut coverage = ColumnCoverageTree::new(self.column_boundaries.len());
        let mut event_index = 0_usize;

        for (&(row, col), overlay) in formats {
            stats.direct_entries_visited = stats.direct_entries_visited.saturating_add(1);
            while let Some(event) = self
                .row_events
                .get(event_index)
                .filter(|event| event.row <= u64::from(row))
            {
                coverage.add(event.first_col_index, event.delta);
                coverage.add(event.after_last_col_index, -event.delta);
                event_index += 1;
                stats.row_events_applied = stats.row_events_applied.saturating_add(1);
            }
            stats.membership_queries = stats.membership_queries.saturating_add(1);
            let boundary_count = self
                .column_boundaries
                .partition_point(|boundary| *boundary <= u32::from(col));
            if coverage.prefix_sum(boundary_count) > 0 {
                stats.selected_entries = stats.selected_entries.saturating_add(1);
                visit(row, col, overlay);
            }
        }
        stats
    }

    pub(super) fn relevant_direct_cell_format_count(
        &self,
        formats: &BTreeMap<(u32, u16), CellStyleOverlay>,
    ) -> (u64, RenderStyleTraversalStats) {
        let mut count = 0_u64;
        let stats = self.for_each_relevant_direct_cell_format(formats, |_, _, _| {
            count = count.saturating_add(1);
        });
        (count, stats)
    }
}

impl Default for Sheet {
    fn default() -> Self {
        Sheet {
            name: String::default(),
            is_worksheet: bool::default(),
            style_fidelity: StyleFidelity::Unavailable,
            display_date1904: false,
            sheet_type: None,
            cells: Vec::default(),
            display_cell_index: OnceLock::new(),
            col_widths: BTreeMap::default(),
            xlsb_col_widths_256: BTreeMap::default(),
            xlsb_default_col_width: None,
            biff_application_default_col_width: false,
            biff_application_default_row_height: false,
            physical_col_widths: BTreeMap::default(),
            imported_column_axis_measures: BTreeMap::default(),
            imported_default_column_axis_measure: None,
            row_heights: BTreeMap::default(),
            automatic_row_height_candidates: BTreeSet::default(),
            imported_row_axis_measures: BTreeMap::default(),
            imported_default_row_axis_measure: None,
            hidden_cols: BTreeSet::default(),
            hidden_rows: BTreeSet::default(),
            default_hidden_row_exceptions: None,
            col_formats: BTreeMap::default(),
            row_formats: BTreeMap::default(),
            default_format: None,
            blank_styles: BTreeMap::default(),
            default_row_height: None,
            automatic_default_row_height_candidate: false,
            default_col_width: None,
            ooxml_implicit_col_width: OoxmlImplicitColumnWidth::None,
            ooxml_defaulted_base_col_width: false,
            ooxml_implicit_row_height: OoxmlImplicitRowHeight::None,
            xlsx_normal_font_size_pt: None,
            xlsb_normal_font_size_pt: None,
            xlsb_cell_font_sizes_pt: Vec::default(),
            xlsb_row_font_sizes_pt: BTreeMap::default(),
            xlsb_col_font_sizes_pt: BTreeMap::default(),
            merges: Vec::default(),
            read_merges: Vec::default(),
            read_hyperlinks: Vec::default(),
            freeze: None,
            autofilter: None,
            page_setup: None,
            print_metadata: PrintMetadata::default(),
            tab_color: None,
            protect: bool::default(),
            protect_options: None,
            data_validations: Vec::default(),
            cond_formats: Vec::default(),
            cond_format_metadata: Vec::default(),
            images: Vec::default(),
            charts: Vec::default(),
            drawing_metadata: Vec::default(),
            style_losses: Vec::default(),
            sparklines: Vec::default(),
            tables: Vec::default(),
            table_header_formats: BTreeMap::default(),
            table_region_formats: BTreeMap::default(),
            direct_cell_formats: BTreeMap::default(),
            comments: Vec::default(),
            rich: BTreeMap::default(),
            hide_gridlines: bool::default(),
            zoom: None,
            show_headers: None,
            right_to_left: bool::default(),
            hidden: bool::default(),
            very_hidden: bool::default(),
            autofit: bool::default(),
            row_outline: BTreeMap::default(),
            col_outline: BTreeMap::default(),
            print_gridlines: bool::default(),
            print_headings: bool::default(),
            // Excel's outline defaults: summaries below/right of the detail.
            outline_summary_below: true,
            outline_summary_right: true,
            collapsed_rows: BTreeSet::default(),
        }
    }
}

impl Sheet {
    /// Flatten this sheet to text: rows sorted top-to-bottom, cells tab-joined
    /// left-to-right.
    pub fn to_text(&self) -> String {
        // Last-write-wins per (row, col) — agree with `cell()`/`rows()` and Excel,
        // rather than emitting a re-written coordinate twice. A BTreeMap keyed by
        // coordinate both dedups (later insert wins) and sorts.
        let mut by_coord: BTreeMap<(u32, u16), &str> = BTreeMap::new();
        for c in &self.cells {
            by_coord.insert((c.row, c.col), c.text.as_str());
        }
        let mut out = String::new();
        let mut cur_row: Option<u32> = None;
        for ((row, _col), text) in &by_coord {
            // Skip value-less cells (e.g. a formula with a blank cached result):
            // they carry identity for `cells()`/`rows()` but contribute no token.
            if text.is_empty() {
                continue;
            }
            match cur_row {
                Some(r) if r == *row => out.push('\t'),
                _ => {
                    if cur_row.is_some() {
                        out.push('\n');
                    }
                    cur_row = Some(*row);
                }
            }
            out.push_str(text);
        }
        out
    }

    /// Export non-empty worksheet rows as CSV using comma separators.
    ///
    /// Values use the same formatted display text as [`Sheet::to_text`]. Empty
    /// rows are skipped; empty cells between non-empty cells in a row are kept.
    /// This compatibility method is capped at
    /// [`crate::DEFAULT_EXPORT_MAX_BYTES`]. If the cap is exceeded, it returns
    /// a diagnostic record beginning `# rxls-export-error:` instead of partial
    /// or truncated source data. Use [`crate::export_csv`] to select a limit and
    /// receive a typed error.
    pub fn to_csv(&self) -> String {
        self.to_csv_with_delimiter(',')
    }

    /// Export non-empty worksheet rows as delimiter-separated values.
    ///
    /// Fields are quoted when they contain the delimiter, a quote, or a line
    /// break; embedded quotes are doubled. Empty rows are skipped so sparse
    /// max-coordinate sheets do not expand into unbounded blank output.
    ///
    /// `'"'` is not a valid delimiter: quoted-field boundaries and the field
    /// separator would become the same character, making the output
    /// genuinely ambiguous to any reader. Since this method's return type
    /// cannot signal failure, a `delimiter` of `'"'` is silently normalized
    /// to the default `','` rather than emitting that ambiguous output.
    ///
    /// This compatibility method is capped at
    /// [`crate::DEFAULT_EXPORT_MAX_BYTES`]. If the cap is exceeded, it returns
    /// a diagnostic record beginning `# rxls-export-error:` instead of partial
    /// or truncated source data. Use [`crate::export_csv`] to select a limit and
    /// receive a typed error.
    pub fn to_csv_with_delimiter(&self, delimiter: char) -> String {
        let delimiter = if delimiter == '"' { ',' } else { delimiter };
        crate::export::legacy_csv(self, delimiter, crate::DEFAULT_EXPORT_MAX_BYTES)
    }

    /// Export the worksheet as an HTML table fragment.
    ///
    /// The fragment contains one `<table>` and no document wrapper. Values use
    /// the same formatted display text as [`Sheet::to_text`].
    ///
    /// This compatibility method uses sparse `colspan` cells and is capped at
    /// [`crate::DEFAULT_EXPORT_MAX_BYTES`] plus a bounded merged-range work
    /// budget. On either limit it returns a visible table carrying a
    /// `data-rxls-export-error` attribute; it never returns partial or silently
    /// truncated source data. Use [`crate::export_html`] for typed errors and a
    /// caller-selected byte limit.
    pub fn to_html(&self) -> String {
        crate::export::legacy_html(self, crate::DEFAULT_EXPORT_MAX_BYTES)
    }

    /// Export the worksheet as GitHub-flavored Markdown.
    ///
    /// Merged cells cannot be expressed losslessly in GFM, so sheets with merges
    /// fall back to the HTML fragment. Very wide sheets also fall back to HTML to
    /// keep the Markdown table bounded.
    ///
    /// This compatibility method is capped at
    /// [`crate::DEFAULT_EXPORT_MAX_BYTES`] plus a bounded merged-range work
    /// budget. On either limit it returns a visible block beginning with an
    /// `rxls export error` marker; it never returns partial or silently
    /// truncated source data. Use [`crate::export_markdown`] for typed errors
    /// and a caller-selected byte limit.
    pub fn to_markdown(&self) -> String {
        crate::export::legacy_markdown(self, crate::DEFAULT_EXPORT_MAX_BYTES)
    }

    /// Grouped worksheet-level metadata borrowed from this sheet.
    pub fn metadata(&self) -> WorksheetMetadata<'_> {
        WorksheetMetadata {
            name: &self.name,
            sheet_type: self.sheet_type(),
            visible: self.visible(),
            dimensions: self.dimensions(),
            merged_ranges: self.merged_ranges(),
            hyperlinks: self.hyperlinks(),
            comments: self.comments(),
            tables: self.tables(),
            data_validations: self.data_validations(),
            conditional_formats: self.conditional_formats(),
            protected: self.is_protected(),
            protection_options: self.protection_options(),
            page_setup: self.page_setup(),
            sheet_view: self.sheet_view(),
            autofilter_range: self.autofilter_range(),
            tab_color: self.tab_color(),
            print_gridlines: self.print_gridlines(),
            print_headings: self.print_headings(),
            row_outline_levels: self.row_outline_levels(),
            col_outline_levels: self.col_outline_levels(),
            collapsed_rows: self.collapsed_rows(),
            outline_summary_below: self.outline_summary_below(),
            outline_summary_right: self.outline_summary_right(),
            images: self.images(),
            charts: self.charts(),
            sparklines: self.sparklines(),
        }
    }

    /// Iterate the non-empty cells as `(row, col, &Cell)`, in **record order**.
    ///
    /// This is the raw cell stream and may yield the same `(row, col)` more than
    /// once if a file (or authoring code) writes a coordinate repeatedly — the
    /// later record is the effective value (Excel last-write-wins). For a
    /// deduplicated, ordered view use [`Sheet::rows`] or [`Sheet::cell`]; this
    /// method stays allocation-free by not deduplicating.
    pub fn cells(&self) -> impl Iterator<Item = (u32, u16, &Cell)> {
        self.cells.iter().map(|c| (c.row, c.col, &c.value))
    }

    /// Iterate display-ready cells in ascending coordinate order with duplicate
    /// records resolved by last-write-wins semantics.
    ///
    /// The iterator builds a bounded index proportional to the sheet's retained
    /// cell count so consumers do not need repeated linear lookups for formatted
    /// text, style, rich text, or hyperlinks.
    pub fn display_cells(&self) -> impl Iterator<Item = DisplayCell<'_>> {
        self.display_cell_index()
            .cells
            .iter()
            .map(|entry| entry.source_index)
            .map(move |source_index| self.display_cell_from_index(source_index))
    }

    /// Renderer-private rectangular traversal over the compact sorted display
    /// index. The initial build is linear-memory plus one sort; subsequent point
    /// and range queries use binary search and contiguous ordered traversal
    /// rather than rescanning every retained worksheet record.
    /// This is hidden from the supported public API surface; external consumers
    /// should use [`Sheet::display_cells`].
    #[doc(hidden)]
    pub fn display_cells_in_range(
        &self,
        first_row: u32,
        first_col: u16,
        last_row: u32,
        last_col: u16,
    ) -> impl Iterator<Item = DisplayCell<'_>> {
        self.display_cell_index()
            .source_indices_in_range(first_row, first_col, last_row, last_col)
            .map(move |source_index| self.display_cell_from_index(source_index))
    }

    fn display_cell_index(&self) -> &DisplayCellIndex {
        self.display_cell_index.get_or_init(|| {
            let mut cells = Vec::with_capacity(self.cells.len());
            for (source_index, entry) in self.cells.iter().enumerate() {
                cells.push(DisplayCellIndexEntry {
                    coordinate: DisplayCellIndex::packed_coordinate(entry.row, entry.col),
                    source_index,
                });
            }
            let mut read_hyperlinks = Vec::with_capacity(self.read_hyperlinks.len());
            for (source_index, (row, col, _)) in self.read_hyperlinks.iter().enumerate() {
                read_hyperlinks.push(DisplayCellIndexEntry {
                    coordinate: DisplayCellIndex::packed_coordinate(*row, *col),
                    source_index,
                });
            }
            DisplayCellIndex {
                cells: DisplayCellIndex::compact_last_by_coordinate(cells),
                read_hyperlinks: DisplayCellIndex::compact_last_by_coordinate(read_hyperlinks),
            }
        })
    }

    fn display_cell_from_index(&self, entry_index: usize) -> DisplayCell<'_> {
        let entry = &self.cells[entry_index];
        let coordinate = (entry.row, entry.col);
        let read_hyperlink_index = self
            .display_cell_index
            .get()
            .and_then(|index| index.read_hyperlink_index(coordinate.0, coordinate.1));
        self.display_cell_from_index_with_read_hyperlink(entry_index, read_hyperlink_index)
    }

    fn display_cell_from_index_with_read_hyperlink(
        &self,
        entry_index: usize,
        read_hyperlink_index: Option<usize>,
    ) -> DisplayCell<'_> {
        let entry = &self.cells[entry_index];
        let coordinate = (entry.row, entry.col);
        DisplayCell {
            row: entry.row,
            col: entry.col,
            value: &entry.value,
            formatted: &entry.text,
            explicit_style: entry.style.as_ref(),
            rich_text: self.rich.get(&coordinate).map(Vec::as_slice),
            hyperlink: entry.hyperlink.as_deref().or_else(|| {
                read_hyperlink_index
                    .and_then(|index| self.read_hyperlinks.get(index))
                    .map(|(_, _, target)| target.as_str())
            }),
        }
    }

    fn effective_cell_entry(&self, row: u32, col: u16) -> Option<&CellEntry> {
        self.display_cell_index()
            .source_index(row, col)
            .and_then(|source_index| self.cells.get(source_index))
    }

    /// The typed value at `(row, col)`, if that cell is non-empty. When a
    /// coordinate has multiple records the last one wins (Excel semantics).
    pub fn cell(&self, row: u32, col: u16) -> Option<&Cell> {
        self.effective_cell_entry(row, col)
            .map(|entry| &entry.value)
    }

    /// The rendered **display text** at `(row, col)` — the pre-formatted string
    /// [`Sheet::to_text`] emits for that cell (e.g. `50%`, `2024-03-15`,
    /// `₩1,000`, `TRUE`), as a calamine-`formatted_value`-style accessor. This
    /// is the number-format-applied surface, whereas [`Sheet::cell`] returns the
    /// typed value (`Cell::Number(0.5)`, `Cell::Date(45366.0)`, …). Last-write-
    /// wins per coordinate, matching [`Sheet::cell`]. Returns `None` when the
    /// cell is empty.
    pub fn formatted(&self, row: u32, col: u16) -> Option<&str> {
        self.effective_cell_entry(row, col)
            .map(|entry| entry.text.as_str())
    }

    /// Effective cell style at `(row, col)`, when retained by the reader or set
    /// explicitly for authoring. A format-only blank cell is also surfaced.
    pub fn cell_style(&self, row: u32, col: u16) -> Option<&CellStyle> {
        self.effective_cell_entry(row, col)
            .and_then(|entry| entry.style.as_ref())
            .or_else(|| self.blank_styles.get(&(row, col)))
    }

    /// Report how completely source styles were retained for this worksheet.
    pub fn style_fidelity(&self) -> StyleFidelity {
        self.style_fidelity
    }

    /// Aggregated, typed boundaries that made source style retention partial.
    pub fn style_losses(&self) -> &[StyleLoss] {
        &self.style_losses
    }

    /// Resolve worksheet, column, row, table-region, and explicit cell styles
    /// for `(row, col)` using deterministic low-to-high precedence.
    ///
    /// The returned style is owned because inherited layers are merged. `None`
    /// means no style layer applies.
    pub fn resolved_cell_style(&self, row: u32, col: u16) -> Option<CellStyle> {
        let table_style = self.tables.iter().find_map(|table| {
            self.table_region_formats
                .get(&table.name)
                .and_then(|application| application.resolve(table.range, row, col))
        });
        let legacy_table_header = self.tables.iter().find_map(|table| {
            let (r0, c0, _, c1) = table.range;
            (row == r0 && col >= c0 && col <= c1)
                .then(|| self.table_header_formats.get(&table.name))
                .flatten()
        });

        // Imported readers retain resolved XF/styles rather than authoring
        // overlays. Select the highest-precedence retained layer directly so a
        // normal-font row, for example, can clear a bold column default. XLSX
        // table imports additionally retain sparse direct-cell XF overlays;
        // those can safely compose over table differential styles.
        if self.style_fidelity != StyleFidelity::Authored {
            if !self.table_region_formats.is_empty() || !self.direct_cell_formats.is_empty() {
                let direct = self.direct_cell_formats.get(&(row, col));
                // Imported row/column XFs are fully resolved styles, so the
                // highest-precedence inherited XF replaces the lower one.
                // Table DXFs are sparse and therefore merge property-wise.
                // Finally, explicitly applied direct-cell XF components
                // replace their complete component (including false/None
                // resets such as normal font, no border, or General format).
                let mut resolved = self
                    .row_formats
                    .get(&row)
                    .or_else(|| self.col_formats.get(&col))
                    .or(self.default_format.as_ref())
                    .cloned();
                if let Some(table) = table_style.as_ref().or(legacy_table_header) {
                    resolved = Some(match resolved {
                        Some(base) => base.merge(table),
                        None => table.clone(),
                    });
                }
                if let Some(direct) = direct {
                    resolved = Some(direct.apply_to(resolved));
                }
                return resolved.or_else(|| self.cell_style(row, col).cloned());
            }
            return self
                .cell_style(row, col)
                .or(table_style.as_ref())
                .or(legacy_table_header)
                .or_else(|| self.row_formats.get(&row))
                .or_else(|| self.col_formats.get(&col))
                .or(self.default_format.as_ref())
                .cloned();
        }
        [
            self.default_format.as_ref(),
            self.col_formats.get(&col),
            self.row_formats.get(&row),
            table_style.as_ref().or(legacy_table_header),
            self.cell_style(row, col),
        ]
        .into_iter()
        .flatten()
        .fold(None, |resolved: Option<CellStyle>, style| {
            Some(match resolved {
                Some(base) => base.merge(style),
                None => style.clone(),
            })
        })
    }

    /// Worksheet-default cell style before column, row, table, or cell layers.
    pub fn default_cell_style(&self) -> Option<&CellStyle> {
        self.default_format.as_ref()
    }

    /// Column-default styles keyed by zero-based column index.
    pub fn column_styles(&self) -> &BTreeMap<u16, CellStyle> {
        &self.col_formats
    }

    /// Row-default styles keyed by zero-based row index.
    pub fn row_styles(&self) -> &BTreeMap<u32, CellStyle> {
        &self.row_formats
    }

    /// Explicit styles for format-only blank cells.
    pub fn blank_cell_styles(&self) -> &BTreeMap<(u32, u16), CellStyle> {
        &self.blank_styles
    }

    /// Table-header style overrides keyed by authored table name.
    pub fn table_header_styles(&self) -> &BTreeMap<String, CellStyle> {
        &self.table_header_formats
    }

    /// Internal deterministic style sidecar consumed by `rxls-render`.
    ///
    /// Public style accessors cover worksheet, row, column, blank-cell,
    /// conditional, and legacy table-header state. This opaque value adds the
    /// complete read-side table-region cascade and sparse direct-cell overlays
    /// without exposing their implementation types or materializing styled
    /// blank-cell ranges.
    #[doc(hidden)]
    pub fn render_style_sidecar_identity<'a>(
        &'a self,
        ranges: &[(u32, u16, u32, u16)],
    ) -> impl std::fmt::Debug + 'a {
        RenderStyleSidecarIdentity {
            sheet: self,
            ranges: RenderStyleRangeIndex::new(ranges),
        }
    }

    /// Number of bounded structural entries in
    /// [`Sheet::render_style_sidecar_identity`].
    #[doc(hidden)]
    pub fn render_style_sidecar_entry_count(&self, ranges: &[(u32, u16, u32, u16)]) -> u64 {
        let ranges = RenderStyleRangeIndex::new(ranges);
        let table_entries = self
            .tables
            .iter()
            .filter(|table| self.table_region_formats.contains_key(&table.name))
            .filter(|table| ranges.intersects(table.range))
            .count() as u64;
        let (direct_entries, _) =
            ranges.relevant_direct_cell_format_count(&self.direct_cell_formats);
        table_entries.saturating_add(direct_entries)
    }

    /// Rich-text runs retained for a cell. Plain strings return `None`; the
    /// concatenated value remains available through [`Sheet::cell`].
    pub fn rich_text_runs(&self, row: u32, col: u16) -> Option<&[TextRun]> {
        self.rich.get(&(row, col)).map(Vec::as_slice)
    }

    /// Explicit column widths in character units, keyed by 0-based column.
    pub fn column_widths(&self) -> &BTreeMap<u16, f32> {
        &self.col_widths
    }

    /// Return raw XLSB per-column widths in 1/256 standard-digit units.
    ///
    /// This is an internal cross-crate contract for `rxls-render`. Other input
    /// formats and authored widths leave this map empty.
    #[doc(hidden)]
    pub fn xlsb_column_widths_256(&self) -> &BTreeMap<u16, u32> {
        &self.xlsb_col_widths_256
    }

    /// Return retained XLSB sheet-wide column-width provenance.
    ///
    /// This is an internal cross-crate contract for `rxls-render`. Explicit
    /// entries in [`Sheet::xlsb_column_widths_256`] take precedence.
    #[doc(hidden)]
    pub fn xlsb_default_column_width(&self) -> Option<XlsbDefaultColumnWidth> {
        self.xlsb_default_col_width
    }

    /// Whether this imported BIFF worksheet uses Calc's application-default
    /// column width because no valid `STANDARDWIDTH` or `DEFCOLWIDTH` record
    /// was present.
    ///
    /// This is an internal cross-crate contract for `rxls-render`.
    #[doc(hidden)]
    pub fn biff_uses_application_default_column_width(&self) -> bool {
        self.biff_application_default_col_width
    }

    /// Whether this imported BIFF worksheet uses Calc's application-default
    /// row height because no valid `DEFAULTROWHEIGHT` record was present.
    ///
    /// This is an internal cross-crate contract for `rxls-render`.
    #[doc(hidden)]
    pub fn biff_uses_application_default_row_height(&self) -> bool {
        self.biff_application_default_row_height
    }

    /// Explicit absolute column widths in points, keyed by 0-based column.
    ///
    /// Formats such as ODS store physical lengths rather than Excel character
    /// units. The ordinary [`Sheet::column_widths`] projection remains
    /// available for compatibility; renderers should prefer this map when an
    /// entry is present.
    pub fn physical_column_widths(&self) -> &BTreeMap<u16, f32> {
        &self.physical_col_widths
    }

    /// Return exact imported per-column source geometry for renderers.
    #[doc(hidden)]
    pub fn imported_column_axis_measures(&self) -> &BTreeMap<u16, ImportedAxisMeasure> {
        &self.imported_column_axis_measures
    }

    /// Return exact imported sheet-wide column geometry for renderers.
    #[doc(hidden)]
    pub fn imported_default_column_axis_measure(&self) -> Option<ImportedAxisMeasure> {
        self.imported_default_column_axis_measure
    }

    /// Retained row heights in points, keyed by 0-based row.
    ///
    /// XLSX imports may include cached automatic heights; XLSB retains
    /// `BrtRowHdr.miyRw` only when `fUnsynced` makes it authoritative. Use
    /// [`Sheet::row_height_is_manual`] when provenance matters.
    pub fn row_heights(&self) -> &BTreeMap<u32, f32> {
        &self.row_heights
    }

    /// Whether a retained per-row height is a manual geometry override.
    ///
    /// This is an internal cross-crate contract for `rxls-render`. XLSX rows
    /// with valid cached heights but without `customHeight`, and ODS rows whose
    /// resolved row style enables `style:use-optimal-row-height`, retain source
    /// geometry while remaining eligible for automatic height expansion. Other
    /// retained per-row heights are authoritative.
    #[doc(hidden)]
    pub fn row_height_is_manual(&self, row: u32) -> bool {
        self.row_heights.contains_key(&row) && !self.automatic_row_height_candidates.contains(&row)
    }

    /// Return exact imported per-row source geometry for renderers.
    #[doc(hidden)]
    pub fn imported_row_axis_measures(&self) -> &BTreeMap<u32, ImportedAxisMeasure> {
        &self.imported_row_axis_measures
    }

    /// Return exact imported sheet-wide row geometry for renderers.
    #[doc(hidden)]
    pub fn imported_default_row_axis_measure(&self) -> Option<ImportedAxisMeasure> {
        self.imported_default_row_axis_measure
    }

    /// Default column width in character units when no explicit width exists.
    pub fn default_column_width(&self) -> Option<f32> {
        self.default_col_width
    }

    /// Return retained OOXML implicit-width provenance for the renderer.
    ///
    /// This is an internal cross-crate contract. `None` means the sheet did
    /// not come from an implicit OOXML default; `Some(None)` means the OOXML
    /// application default applies; and `Some(Some(chars))` retains an
    /// explicitly authored `baseColWidth` / `cchDefColWidth` value.
    #[doc(hidden)]
    pub fn implicit_ooxml_column_width(&self) -> Option<Option<f32>> {
        match self.ooxml_implicit_col_width {
            OoxmlImplicitColumnWidth::None => None,
            OoxmlImplicitColumnWidth::ApplicationDefault => Some(None),
            OoxmlImplicitColumnWidth::BaseCharacters(chars) => Some(Some(chars)),
        }
    }

    /// Whether XLSX import applied `sheetFormatPr`'s defaulted base width.
    ///
    /// This is an internal cross-crate provenance contract used to reproduce
    /// Calc's distinct wrapping width without changing the compatibility
    /// character-width projection.
    #[doc(hidden)]
    pub fn ooxml_uses_defaulted_base_column_width(&self) -> bool {
        self.ooxml_defaulted_base_col_width
    }

    /// Default row height in points when no explicit height exists.
    pub fn default_row_height(&self) -> Option<f32> {
        self.default_row_height
    }

    /// Whether a retained sheet-wide default row height is a manual override.
    ///
    /// This is an internal cross-crate contract for `rxls-render`. An absent
    /// default is never manual.
    #[doc(hidden)]
    pub fn default_row_height_is_manual(&self) -> bool {
        self.default_row_height.is_some() && !self.automatic_default_row_height_candidate
    }

    /// Whether an imported OOXML worksheet omitted an authoritative default row
    /// height and therefore retains the spreadsheet application's default.
    ///
    /// This is an internal cross-crate contract for `rxls-render`. An authored
    /// sheet and imported non-OOXML formats return `false`.
    #[doc(hidden)]
    pub fn has_implicit_ooxml_row_height(&self) -> bool {
        self.ooxml_implicit_row_height != OoxmlImplicitRowHeight::None
    }

    /// Return the exact OOXML family that supplied an implicit row default.
    #[doc(hidden)]
    pub fn implicit_ooxml_row_height_source(&self) -> Option<OoxmlImplicitRowHeight> {
        match self.ooxml_implicit_row_height {
            OoxmlImplicitRowHeight::None => None,
            source => Some(source),
        }
    }

    /// Return an evidence-bounded XLSX Normal-style font size for rendering.
    ///
    /// The value is retained only for imported XLSX worksheets whose first
    /// cell XF and named/built-in Normal style reference the same source font
    /// with an exact integral point size accepted by Office. It therefore also
    /// acts as durable XLSX provenance independent of mutable row/column
    /// defaults. Ambiguous, fractional, invalid, and non-XLSX sources return
    /// `None`.
    #[doc(hidden)]
    pub fn verified_xlsx_normal_font_size_pt(&self) -> Option<u16> {
        self.xlsx_normal_font_size_pt
    }

    /// Return an evidence-bounded XLSX font size for one retained cell XF.
    ///
    /// The value exists only when the effective last-write-wins cell came from
    /// XLSX and its cell XF directly references a complete, unambiguous font
    /// record with an exact integral source size. Fractional, duplicated,
    /// malformed, out-of-range, inherited, authored, and non-XLSX sources
    /// return `None`.
    #[doc(hidden)]
    pub fn verified_xlsx_cell_font_size_pt(&self, row: u32, col: u16) -> Option<u16> {
        let entry = self.effective_cell_entry(row, col)?;
        let points = entry.xlsx_font_size_pt?;
        if let Some(overlay) = self.direct_cell_formats.get(&(row, col)) {
            if overlay.replace_font {
                return Some(points);
            }
            // Style index zero is represented by the retained cell style alone.
            // A nonzero cell XF has a direct-overlay entry even when it does not
            // replace the font; in that case the worksheet default participates
            // in resolution and its rounded public Font cannot prove the exact
            // source size carried by this cell XF.
            if let Some(default_font) = self
                .default_format
                .as_ref()
                .and_then(|style| style.font.as_ref())
            {
                let retained_font = entry.style.as_ref().and_then(|style| style.font.as_ref());
                if self.xlsx_normal_font_size_pt != Some(points)
                    || retained_font != Some(default_font)
                {
                    return None;
                }
            }
        }

        // A non-font-replacing cell XF inherits these layers. Their public
        // whole-point Font values cannot distinguish, for example, an exact
        // 14pt source from a fractional 13.5pt source rounded to 14pt, so do
        // not transfer the cell-XF evidence through any such font layer.
        let inherited_font = self
            .row_formats
            .get(&row)
            .and_then(|style| style.font.as_ref())
            .is_some()
            || self
                .col_formats
                .get(&col)
                .and_then(|style| style.font.as_ref())
                .is_some()
            || self.tables.iter().any(|table| {
                self.table_region_formats
                    .get(&table.name)
                    .and_then(|application| application.resolve(table.range, row, col))
                    .and_then(|style| style.font)
                    .is_some()
                    || {
                        let (first_row, first_col, _, last_col) = table.range;
                        row == first_row
                            && col >= first_col
                            && col <= last_col
                            && self
                                .table_header_formats
                                .get(&table.name)
                                .and_then(|style| style.font.as_ref())
                                .is_some()
                    }
            });
        (!inherited_font).then_some(points)
    }

    /// Return an evidence-bounded XLSB Normal-style font size for rendering.
    ///
    /// The value is retained only for imported XLSB worksheets whose binary
    /// Fonts, CellStyleXFs, CellXFs, and Styles collections are complete,
    /// bounded, and identify one built-in Normal style. Its first cell XF and
    /// Normal style XF must resolve to the same exact integral source font.
    /// Malformed, fractional, out-of-range, authored, and non-XLSB sources
    /// return `None`.
    #[doc(hidden)]
    pub fn verified_xlsb_normal_font_size_pt(&self) -> Option<u16> {
        self.xlsb_normal_font_size_pt
    }

    /// Return an evidence-bounded XLSB font size for one retained cell XF.
    ///
    /// The value follows the effective last-write-wins cell and exists only
    /// when its binary cell XF resolves through complete, bounded source style
    /// tables to an exact integral font size. Rounded public styles, authored
    /// cells, inherited mutable font layers, malformed references, and
    /// non-XLSB sources return `None`.
    #[doc(hidden)]
    pub fn verified_xlsb_cell_font_size_pt(&self, row: u32, col: u16) -> Option<u16> {
        let source_index = self.display_cell_index().source_index(row, col)?;
        let entry = self.cells.get(source_index)?;
        let points = self
            .xlsb_cell_font_sizes_pt
            .get(source_index)
            .copied()
            .flatten()?;

        if self.direct_cell_formats.contains_key(&(row, col)) {
            return None;
        }

        // Style index zero is represented by the worksheet default instead of
        // an explicit retained cell style. Do not transfer its source evidence
        // through a mutable row, column, or table font layer.
        if entry.style.is_none() {
            let inherited_points = if self.row_formats.contains_key(&row) {
                self.xlsb_row_font_sizes_pt.get(&row).copied()
            } else if self.col_formats.contains_key(&col) {
                self.xlsb_col_font_sizes_pt.get(&col).copied()
            } else {
                self.xlsb_normal_font_size_pt
            };
            if inherited_points != Some(points) {
                return None;
            }
        }

        let table_font = self.tables.iter().any(|table| {
            self.table_region_formats
                .get(&table.name)
                .and_then(|application| application.resolve(table.range, row, col))
                .and_then(|style| style.font)
                .is_some()
                || {
                    let (first_row, first_col, _, last_col) = table.range;
                    row == first_row
                        && col >= first_col
                        && col <= last_col
                        && self
                            .table_header_formats
                            .get(&table.name)
                            .and_then(|style| style.font.as_ref())
                            .is_some()
                }
        });
        if table_font {
            return None;
        }

        let retained_font = entry
            .style
            .as_ref()
            .or_else(|| self.row_formats.get(&row))
            .or_else(|| self.col_formats.get(&col))
            .or(self.default_format.as_ref())
            .and_then(|style| style.font.as_ref());
        (retained_font.and_then(|font| font.size_pt) == Some(points)).then_some(points)
    }

    /// Explicitly hidden columns, as 0-based indexes.
    pub fn hidden_columns(&self) -> &BTreeSet<u16> {
        &self.hidden_cols
    }

    /// Explicitly hidden rows, as 0-based indexes.
    pub fn hidden_rows(&self) -> &BTreeSet<u32> {
        &self.hidden_rows
    }

    /// Return OOXML default-hidden-row provenance for the renderer.
    ///
    /// `None` means rows are visible by default. `Some(exceptions)` means
    /// unspecified rows are hidden by default and `exceptions` contains the
    /// rows explicitly retained as visible by the source format.
    #[doc(hidden)]
    pub fn default_hidden_row_exceptions(&self) -> Option<&BTreeSet<u32>> {
        self.default_hidden_row_exceptions.as_ref()
    }

    /// Worksheet tab color, when the source workbook or authoring model set one.
    ///
    /// Currently read from OOXML `<sheetPr><tabColor .../>` RGB, theme/tint, and
    /// indexed tab colors, XLSB/BIFF tab-color records, and ODS
    /// `style:table-properties table:tab-color`; emitted by the `.xlsx` writer
    /// when set through [`Sheet::set_tab_color`].
    pub fn tab_color(&self) -> Option<Color> {
        self.tab_color
    }

    /// Whether printed pages include worksheet gridlines.
    pub fn print_gridlines(&self) -> bool {
        self.print_gridlines
    }

    /// Whether printed pages include row and column headings.
    pub fn print_headings(&self) -> bool {
        self.print_headings
    }

    /// Row outline levels keyed by 0-based row index.
    pub fn row_outline_levels(&self) -> &BTreeMap<u32, u8> {
        &self.row_outline
    }

    /// Column outline levels keyed by 0-based column index.
    pub fn col_outline_levels(&self) -> &BTreeMap<u16, u8> {
        &self.col_outline
    }

    /// Rows marked as collapsed outline summary rows.
    pub fn collapsed_rows(&self) -> &BTreeSet<u32> {
        &self.collapsed_rows
    }

    /// Whether outline summary rows appear below grouped detail rows.
    pub fn outline_summary_below(&self) -> bool {
        self.outline_summary_below
    }

    /// Whether outline summary columns appear to the right of grouped detail columns.
    pub fn outline_summary_right(&self) -> bool {
        self.outline_summary_right
    }

    /// Merged cell ranges as `(first_row, first_col, last_row, last_col)`,
    /// 0-based and inclusive. Populated on read from `.xls` `MERGECELLS` and
    /// `.xlsx` `<mergeCells>`, or the ranges set when authoring via
    /// [`Sheet::merge`]. The merged value lives in the top-left cell.
    pub fn merged_ranges(&self) -> &[(u32, u16, u32, u16)] {
        if self.merges.is_empty() {
            &self.read_merges
        } else {
            &self.merges
        }
    }

    /// External hyperlinks read from supported spreadsheet formats, as `(row, col,
    /// url)`, 0-based. For `.xlsx`, URLs are resolved through worksheet rels
    /// (`TargetMode="External"`); for `.xls`, they come from BIFF HLINK records;
    /// for `.ods`, they come from `text:a` `xlink:href` links. Empty for files
    /// without hyperlinks. Independent of the per-cell authoring hyperlink
    /// consumed by the writer.
    pub fn hyperlinks(&self) -> &[(u32, u16, String)] {
        &self.read_hyperlinks
    }

    /// Legacy cell comments / notes anchored to cells (`.xlsx`
    /// `xl/comments{N}.xml`, `.xls` BIFF notes, `.xlsb` comments parts, or
    /// `.ods` `office:annotation`). Shares the authoring [`Comment`] storage, so
    /// a read workbook round-trips its comments on write.
    pub fn comments(&self) -> &[Comment] {
        &self.comments
    }

    /// Worksheet tables (`.xlsx` `xl/tables/table{N}.xml`, `.xlsb` binary table
    /// parts, or named `.ods` `table:database-range`) with named header columns.
    /// Shares the authoring [`Table`] storage, so a read workbook round-trips its
    /// tables on write.
    /// Each [`Table`] carries its `range` (0-based, inclusive), `name`, and header
    /// `columns`.
    pub fn tables(&self) -> &[Table] {
        &self.tables
    }

    /// Data-validation rules discovered when reading supported spreadsheet
    /// formats (`.xlsx`, `.xls`, `.xlsb`, `.ods`), or added for authoring with
    /// [`Sheet::add_data_validation`].
    pub fn data_validations(&self) -> &[DataValidation] {
        &self.data_validations
    }

    /// Conditional-formatting rules discovered when reading supported
    /// spreadsheet formats, or added for authoring with
    /// [`Sheet::add_conditional_format`].
    pub fn conditional_formats(&self) -> &[CondFormat] {
        &self.cond_formats
    }

    /// Read-side conditional-formatting metadata aligned by rule index.
    ///
    /// Authored rules carry default metadata and therefore retain their
    /// document-order precedence. Readers for formats without an equivalent
    /// sidecar may return fewer entries; consumers should then use document
    /// order and the public [`crate::CfRule`] payload.
    pub fn conditional_format_metadata(&self) -> &[ConditionalFormatMetadata] {
        &self.cond_format_metadata
    }

    /// Whether worksheet protection is enabled.
    pub fn is_protected(&self) -> bool {
        self.protect
    }

    /// Granular worksheet-protection allowances, if the source or authoring
    /// model supplied any.
    pub fn protection_options(&self) -> Option<ProtectionOptions> {
        self.protect_options
    }

    /// Print/page setup discovered when reading supported spreadsheet formats,
    /// including `.xls`/`.xlsb` page setup records plus `Print_Area` /
    /// `Print_Titles` built-in name ranges, or set for authoring with
    /// [`Sheet::set_page_setup`].
    pub fn page_setup(&self) -> Option<&PageSetup> {
        self.page_setup.as_ref()
    }

    /// Loss-aware source print metadata, including details not represented by
    /// [`PageSetup`] such as multiple print areas and manual page breaks.
    pub fn print_metadata(&self) -> &PrintMetadata {
        &self.print_metadata
    }

    /// Worksheet view metadata discovered when reading supported spreadsheet
    /// formats, or set for authoring through the sheet-view builder methods.
    pub fn sheet_view(&self) -> SheetView {
        SheetView {
            freeze: self.freeze,
            hide_gridlines: self.hide_gridlines,
            zoom: self.zoom,
            show_headers: self.show_headers,
            right_to_left: self.right_to_left,
        }
    }

    /// Autofilter range as `(first_row, first_col, last_row, last_col)`, 0-based
    /// and inclusive, when the worksheet declares one (`.xlsx` `autoFilter` or
    /// sheet-local `_FilterDatabase`, `.xlsb` `BrtBeginAFilter`, `.xls`
    /// `_FilterDatabase`, or `.ods` `table:database-range`).
    pub fn autofilter_range(&self) -> Option<(u32, u16, u32, u16)> {
        self.autofilter
    }

    /// Embedded images (`xl/media/imageN.*` or ODS `draw:image` package parts)
    /// anchored to worksheet cells. Shares the authoring [`Image`] storage, so a
    /// read workbook round-trips its images on write.
    pub fn images(&self) -> &[Image] {
        &self.images
    }

    /// Charts anchored to worksheet cell boxes.
    /// Currently populated by the `.xlsx` reader; shares the authoring
    /// [`Chart`] storage, so a read workbook round-trips its charts on write.
    pub fn charts(&self) -> &[Chart] {
        &self.charts
    }

    /// Rendering sidecars for retained images, charts, and unsupported shapes.
    ///
    /// `object_index` addresses [`Self::images`] or [`Self::charts`] according
    /// to each entry's `kind`; unsupported shapes carry geometry but have no
    /// Image/Chart object.
    pub fn drawing_metadata(&self) -> &[DrawingMetadata] {
        &self.drawing_metadata
    }

    /// Sparklines (`x14:sparklineGroup`) anchored to worksheet cells.
    /// Currently populated by the `.xlsx` reader; shares the authoring
    /// [`Sparkline`] storage, so a read workbook round-trips its sparklines on
    /// write.
    pub fn sparklines(&self) -> &[Sparkline] {
        &self.sparklines
    }

    /// Whether this worksheet is hidden (`<sheet state="hidden">` / `.xls`-`.xlsb`
    /// `hsState == 1`). A hidden sheet is unhideable through the Excel UI but stays
    /// in the workbook. Matches calamine's `Sheet::visible`. Read on every format.
    pub fn is_hidden(&self) -> bool {
        self.hidden
    }

    /// Whether this worksheet is *very* hidden (`<sheet state="veryHidden">` /
    /// `hsState == 2`) — hideable/unhideable only via VBA, never the Excel UI.
    /// A very-hidden sheet reports `false` from [`Self::is_hidden`]; the two states
    /// are distinct.
    pub fn is_very_hidden(&self) -> bool {
        self.very_hidden
    }

    /// Sheet type for metadata views.
    pub fn sheet_type(&self) -> SheetType {
        self.sheet_type.unwrap_or(if self.is_worksheet {
            SheetType::WorkSheet
        } else {
            SheetType::ChartSheet
        })
    }

    /// Sheet visibility for metadata views.
    pub fn visible(&self) -> SheetVisible {
        if self.very_hidden {
            SheetVisible::VeryHidden
        } else if self.hidden {
            SheetVisible::Hidden
        } else {
            SheetVisible::Visible
        }
    }

    /// Used range as `(min_row, min_col, max_row, max_col)` over non-empty cells.
    pub fn dimensions(&self) -> Option<(u32, u16, u32, u16)> {
        let mut it = self.cells.iter();
        let f = it.next()?;
        let (mut r0, mut c0, mut r1, mut c1) = (f.row, f.col, f.row, f.col);
        for c in it {
            r0 = r0.min(c.row);
            c0 = c0.min(c.col);
            r1 = r1.max(c.row);
            c1 = c1.max(c.col);
        }
        Some((r0, c0, r1, c1))
    }

    /// Inclusive dimensions covering values, format-only blanks, and merged
    /// ranges. This is the render/export surface rather than only populated
    /// value cells.
    pub fn visual_dimensions(&self) -> Option<(u32, u16, u32, u16)> {
        let mut dimensions = self.dimensions();
        let mut include = |row: u32, col: u16| {
            dimensions = Some(match dimensions {
                Some((r0, c0, r1, c1)) => (r0.min(row), c0.min(col), r1.max(row), c1.max(col)),
                None => (row, col, row, col),
            });
        };
        for &(row, col) in self.blank_styles.keys() {
            include(row, col);
        }
        for &(r0, c0, r1, c1) in self.merged_ranges() {
            include(r0, c0);
            include(r1, c1);
        }
        dimensions
    }

    /// Used range dimensions as a typed inclusive rectangle.
    pub fn dimensions_info(&self) -> Option<Dimensions> {
        self.dimensions().map(Dimensions::from_range_tuple)
    }

    /// Iterate the non-empty cells grouped by row, in ascending `(row, col)`
    /// order: each item is `(row, [(col, &Cell), …])`. A calamine-`Range::rows`-
    /// style view over this crate's sparse cell model.
    pub fn rows(&self) -> impl Iterator<Item = (u32, Vec<(u16, &Cell)>)> {
        // Last-write-wins per (row, col) to agree with `cell()` and Excel — a
        // nested map overwrites duplicate coordinates instead of listing both.
        let mut by_row: BTreeMap<u32, BTreeMap<u16, &Cell>> = BTreeMap::new();
        for c in &self.cells {
            by_row.entry(c.row).or_default().insert(c.col, &c.value);
        }
        by_row
            .into_iter()
            .map(|(r, cols)| (r, cols.into_iter().collect()))
    }
}

impl Sheet {
    /// A new empty worksheet for authoring.
    pub fn new(name: impl AsRef<str>) -> Self {
        Self::new_with_display_epoch(name, false)
    }

    pub(in crate::model) fn new_with_display_epoch(
        name: impl AsRef<str>,
        display_date1904: bool,
    ) -> Self {
        Sheet {
            name: name.as_ref().to_string(),
            is_worksheet: true,
            sheet_type: Some(SheetType::WorkSheet),
            style_fidelity: StyleFidelity::Authored,
            display_date1904,
            print_metadata: PrintMetadata::authored(),
            ..Default::default()
        }
    }

    pub(in crate::model) fn formula_range(&self) -> FormulaRange<'_> {
        FormulaRange::from_borrowed_cells(
            self.cells
                .iter()
                .map(|cell| (cell.row, cell.col, &cell.value)),
        )
    }
    /// Build a rectangular [`Range`] view over this sheet's effective cells.
    pub fn range(&self) -> Range<'_> {
        Range::from_borrowed_cells(
            self.cells
                .iter()
                .map(|cell| (cell.row, cell.col, &cell.value, cell.text.as_str())),
        )
    }
    /// Write a value at `(row, col)`.
    pub fn write(&mut self, row: u32, col: u16, value: impl Into<Cell>) {
        self.push_authored(row, col, value.into(), None, None);
    }
    /// Write a string value at `(row, col)`.
    pub fn write_string(&mut self, row: u32, col: u16, value: impl AsRef<str>) {
        self.write(row, col, value.as_ref());
    }
    /// Write a number at `(row, col)`.
    pub fn write_number(&mut self, row: u32, col: u16, value: impl Into<f64>) {
        self.write(row, col, value.into());
    }
    /// Write a boolean value at `(row, col)`.
    pub fn write_boolean(&mut self, row: u32, col: u16, value: bool) {
        self.write(row, col, value);
    }
    /// Write a boolean value at `(row, col)` with a [`Format`].
    pub fn write_boolean_with_format(&mut self, row: u32, col: u16, value: bool, format: &Format) {
        self.write_styled(row, col, value, format.as_cell_style());
    }
    /// Write a typed spreadsheet error at `(row, col)`.
    pub fn write_error(&mut self, row: u32, col: u16, error: CellErrorType) {
        self.write(row, col, error);
    }
    /// Write a typed spreadsheet error at `(row, col)` with a [`Format`].
    pub fn write_error_with_format(
        &mut self,
        row: u32,
        col: u16,
        error: CellErrorType,
        format: &Format,
    ) {
        self.write_styled(row, col, error, format.as_cell_style());
    }
    /// Write an Excel serial date/time value at `(row, col)`.
    ///
    /// The serial is stored as [`Cell::Date`], so the writer emits a date cell and
    /// the reader reopens it as a typed date value.
    pub fn write_datetime(&mut self, row: u32, col: u16, serial: impl Into<f64>) {
        self.write(row, col, Cell::Date(serial.into()));
    }
    /// Write an Excel serial date/time value at `(row, col)` with a [`Format`].
    pub fn write_datetime_with_format(
        &mut self,
        row: u32,
        col: u16,
        serial: impl Into<f64>,
        format: &Format,
    ) {
        self.write_styled(row, col, Cell::Date(serial.into()), format.as_cell_style());
    }
    /// Write a value at `(row, col)` with an inline style.
    pub fn write_styled(&mut self, row: u32, col: u16, value: impl Into<Cell>, style: &CellStyle) {
        self.push_authored(row, col, value.into(), Some(style.clone()), None);
    }
    /// Write a value at `(row, col)` with a [`Format`].
    pub fn write_with_format(
        &mut self,
        row: u32,
        col: u16,
        value: impl Into<Cell>,
        format: &Format,
    ) {
        self.write_styled(row, col, value, format.as_cell_style());
    }
    /// Write a string at `(row, col)` with a [`Format`].
    pub fn write_string_with_format(
        &mut self,
        row: u32,
        col: u16,
        value: impl AsRef<str>,
        format: &Format,
    ) {
        self.write_styled(row, col, value.as_ref(), format.as_cell_style());
    }
    /// Write a number at `(row, col)` with a [`Format`].
    pub fn write_number_with_format(
        &mut self,
        row: u32,
        col: u16,
        value: impl Into<f64>,
        format: &Format,
    ) {
        self.write_styled(row, col, value.into(), format.as_cell_style());
    }
    /// Write a formula at `(row, col)` with a cached result value.
    ///
    /// `formula` is stored without a leading `=`. `cached` is the last calculated
    /// result that spreadsheet readers can show before recalculation.
    pub fn write_formula(
        &mut self,
        row: u32,
        col: u16,
        formula: impl AsRef<str>,
        cached: impl Into<Cell>,
    ) {
        self.write(
            row,
            col,
            Cell::Formula {
                formula: formula.as_ref().to_string(),
                cached: Box::new(cached.into()),
            },
        );
    }
    /// Write a formula at `(row, col)` with a [`Format`] and cached result value.
    ///
    /// `formula` is stored without a leading `=`. `cached` is the last calculated
    /// result that spreadsheet readers can show before recalculation.
    pub fn write_formula_with_format(
        &mut self,
        row: u32,
        col: u16,
        formula: impl AsRef<str>,
        cached: impl Into<Cell>,
        format: &Format,
    ) {
        self.write_styled(
            row,
            col,
            Cell::Formula {
                formula: formula.as_ref().to_string(),
                cached: Box::new(cached.into()),
            },
            format.as_cell_style(),
        );
    }
    /// Write a format-only blank cell at `(row, col)` with an inline style.
    pub fn write_blank_styled(&mut self, row: u32, col: u16, style: &CellStyle) {
        self.rich.remove(&(row, col));
        self.display_cell_index.take();
        if self.xlsb_cell_font_sizes_pt.len() == self.cells.len() {
            let source_font_sizes = std::mem::take(&mut self.xlsb_cell_font_sizes_pt);
            let mut retained_font_sizes = Vec::with_capacity(source_font_sizes.len());
            let mut source_index = 0;
            self.cells.retain(|entry| {
                let retain = entry.row != row || entry.col != col;
                if retain {
                    retained_font_sizes.push(source_font_sizes[source_index]);
                }
                source_index += 1;
                retain
            });
            self.xlsb_cell_font_sizes_pt = retained_font_sizes;
        } else {
            self.cells
                .retain(|entry| entry.row != row || entry.col != col);
            self.xlsb_cell_font_sizes_pt.clear();
        }
        self.blank_styles.insert((row, col), style.clone());
    }
    /// Write a format-only blank cell at `(row, col)` with a [`Format`].
    pub fn write_blank_with_format(&mut self, row: u32, col: u16, format: &Format) {
        self.write_blank_styled(row, col, format.as_cell_style());
    }
    /// Hide the worksheet gridlines (authoring).
    pub fn hide_gridlines(&mut self) {
        self.hide_gridlines = true;
    }
    /// Set the sheet zoom as a percentage, e.g. `150` (authoring).
    pub fn set_zoom(&mut self, percent: u16) {
        self.zoom = Some(percent);
    }
    /// Show or hide the row and column headers in the sheet view (authoring).
    /// Pass `false` to emit `<sheetView showRowColHeaders="0">`.
    pub fn set_show_headers(&mut self, show: bool) {
        self.show_headers = Some(show);
    }
    /// Lay the sheet out right-to-left (authoring): `<sheetView rightToLeft="1">`.
    pub fn set_right_to_left(&mut self, rtl: bool) {
        self.right_to_left = rtl;
    }
    /// Set worksheet view metadata in one object-model call.
    pub fn set_sheet_view(&mut self, view: SheetView) {
        self.freeze = view.freeze;
        self.hide_gridlines = view.hide_gridlines;
        self.zoom = view.zoom;
        self.show_headers = view.show_headers;
        self.right_to_left = view.right_to_left;
    }
    /// Hide this worksheet in the workbook (authoring).
    pub fn hide(&mut self) {
        self.hidden = true;
    }
    /// Very-hide this worksheet (authoring): `state="veryHidden"`, which Excel hides
    /// from the unhide menu (only a macro/VBA can reveal it).
    pub fn hide_very(&mut self) {
        self.very_hidden = true;
    }
    /// Auto-size column widths from the cell text when authoring. An explicit
    /// [`Sheet::set_col_width`] still takes precedence for that column.
    pub fn set_autofit(&mut self) {
        self.autofit = true;
    }
    /// Group rows `first..=last` at outline `level` (1-based depth) for collapsible
    /// row outlines (authoring). The span is clamped to the row grid.
    pub fn group_rows(&mut self, first: u32, last: u32, level: u8) {
        let last = last.min(1_048_575);
        for r in first..=last {
            self.row_outline.insert(r, level);
        }
    }
    /// Group columns `first..=last` at outline `level` (authoring).
    pub fn group_cols(&mut self, first: u16, last: u16, level: u8) {
        for c in first..=last {
            self.col_outline.insert(c, level);
        }
    }
    /// Set whether outline summary rows sit *below* their grouped detail rows and
    /// summary columns sit to the *right* of theirs (authoring). Both default to
    /// `true` (Excel's default); passing `false` for either emits
    /// `<sheetPr><outlinePr summaryBelow="0" summaryRight="0"/></sheetPr>`.
    pub fn set_outline_summary(&mut self, below: bool, right: bool) {
        self.outline_summary_below = below;
        self.outline_summary_right = right;
    }
    /// Mark the summary `row` of a collapsed group (authoring): the row is emitted
    /// as `<row collapsed="1" hidden="1">`, keeping the summary visible while Excel
    /// treats the group as collapsed. Pair with [`Sheet::group_rows`] on the detail
    /// rows.
    pub fn collapse_row(&mut self, row: u32) {
        self.collapsed_rows.insert(row);
    }
    /// Print the gridlines on the printed page (authoring).
    pub fn set_print_gridlines(&mut self) {
        self.print_gridlines = true;
        self.print_metadata.set_print_gridlines(true);
    }
    /// Print the row and column headings on the printed page (authoring).
    pub fn set_print_headings(&mut self) {
        self.print_headings = true;
        self.print_metadata.set_print_headings(true);
    }
    /// Write a rich (mixed-format) string at `(row, col)`: each [`TextRun`] carries
    /// its own font. Emitted as an inline rich string; the concatenated text is also
    /// stored so the cell has a plain value for readers and other tooling. Empty-text
    /// runs are dropped, and empty/all-empty `runs` is a no-op. Per-run fonts come
    /// from each [`TextRun`]; use [`Self::write_rich_with_format`] to add a
    /// cell-level style.
    pub fn write_rich<I>(&mut self, row: u32, col: u16, runs: I)
    where
        I: IntoIterator<Item = TextRun>,
    {
        self.push_rich(row, col, runs, None);
    }
    /// Write a rich string at `(row, col)` with a cell-level [`CellStyle`].
    ///
    /// The style applies to the cell (`s="..."`) while each [`TextRun`] still
    /// carries its own run font inside the inline string.
    pub fn write_rich_styled<I>(&mut self, row: u32, col: u16, runs: I, style: &CellStyle)
    where
        I: IntoIterator<Item = TextRun>,
    {
        self.push_rich(row, col, runs, Some(style.clone()));
    }
    /// Write a rich string at `(row, col)` with a writer-facing [`Format`].
    pub fn write_rich_with_format<I>(&mut self, row: u32, col: u16, runs: I, format: &Format)
    where
        I: IntoIterator<Item = TextRun>,
    {
        self.write_rich_styled(row, col, runs, format.as_cell_style());
    }
    fn push_rich<I>(&mut self, row: u32, col: u16, runs: I, style: Option<CellStyle>)
    where
        I: IntoIterator<Item = TextRun>,
    {
        let runs: Vec<TextRun> = runs.into_iter().filter(|r| !r.text.is_empty()).collect();
        if runs.is_empty() {
            return;
        }
        let joined: String = runs.iter().map(|r| r.text.as_str()).collect();
        self.push_authored(row, col, Cell::Text(joined), style, None);
        self.rich.insert((row, col), runs);
    }
    /// Write `text` at `(row, col)` as an external hyperlink to `url`.
    pub fn write_url(&mut self, row: u32, col: u16, url: impl AsRef<str>, text: impl AsRef<str>) {
        self.push_authored(
            row,
            col,
            Cell::Text(text.as_ref().to_string()),
            None,
            Some(url.as_ref().to_string()),
        );
    }
    /// Write `text` at `(row, col)` as an external hyperlink to `url`.
    ///
    /// This is a rust_xlsxwriter-style alias for [`Self::write_url`].
    pub fn write_url_with_text(
        &mut self,
        row: u32,
        col: u16,
        url: impl AsRef<str>,
        text: impl AsRef<str>,
    ) {
        self.write_url(row, col, url, text);
    }
    /// Write `url` at `(row, col)` as an external hyperlink with a [`Format`].
    pub fn write_url_with_format(
        &mut self,
        row: u32,
        col: u16,
        url: impl AsRef<str>,
        format: &Format,
    ) {
        let url = url.as_ref();
        self.write_url_with_text_and_format(row, col, url, url, format);
    }
    /// Write `text` at `(row, col)` as an external hyperlink to `url` with a
    /// [`Format`].
    pub fn write_url_with_text_and_format(
        &mut self,
        row: u32,
        col: u16,
        url: impl AsRef<str>,
        text: impl AsRef<str>,
        format: &Format,
    ) {
        self.push_authored(
            row,
            col,
            Cell::Text(text.as_ref().to_string()),
            Some(format.as_cell_style().clone()),
            Some(url.as_ref().to_string()),
        );
    }
    /// Merge the rectangular range `(r0,c0)..=(r1,c1)`.
    pub fn merge(&mut self, r0: u32, c0: u16, r1: u32, c1: u16) {
        self.merges.push((r0, c0, r1, c1));
    }
    /// Merge the rectangular range `(r0,c0)..=(r1,c1)` and write `text` to the
    /// top-left cell with a [`Format`].
    pub fn merge_range(
        &mut self,
        r0: u32,
        c0: u16,
        r1: u32,
        c1: u16,
        text: impl AsRef<str>,
        format: &Format,
    ) {
        self.merge(r0, c0, r1, c1);
        self.write_with_format(r0, c0, text.as_ref(), format);
    }
    /// Set a column width in character units.
    pub fn set_col_width(&mut self, col: u16, chars: f32) {
        self.col_widths.insert(col, chars);
        self.xlsb_col_widths_256.remove(&col);
        self.physical_col_widths.remove(&col);
        self.imported_column_axis_measures.remove(&col);
    }
    /// Set a row height in points.
    pub fn set_row_height(&mut self, row: u32, points: f32) {
        self.row_heights.insert(row, points);
        self.automatic_row_height_candidates.remove(&row);
        self.imported_row_axis_measures.remove(&row);
        if !self.hidden_rows.contains(&row) {
            if let Some(exceptions) = self.default_hidden_row_exceptions.as_mut() {
                exceptions.insert(row);
            }
        }
    }
    /// Hide a column by 0-based index.
    pub fn hide_column(&mut self, col: u16) {
        self.hidden_cols.insert(col);
    }
    /// Hide a row by 0-based index.
    pub fn hide_row(&mut self, row: u32) {
        self.hidden_rows.insert(row);
        if let Some(exceptions) = self.default_hidden_row_exceptions.as_mut() {
            exceptions.remove(&row);
        }
    }
    /// Set the default format for cells in a row.
    pub fn set_row_format(&mut self, row: u32, format: &Format) {
        self.row_formats.insert(row, format.as_cell_style().clone());
        self.xlsb_row_font_sizes_pt.remove(&row);
        self.refresh_authored_display_texts();
    }
    /// Set the default format for cells in a column.
    pub fn set_col_format(&mut self, col: u16, format: &Format) {
        self.col_formats.insert(col, format.as_cell_style().clone());
        self.xlsb_col_font_sizes_pt.remove(&col);
        self.refresh_authored_display_texts();
    }
    /// Set the worksheet default format for cells without a more specific format.
    ///
    /// Column, row, and explicit cell formats merge over this base style.
    pub fn set_default_format(&mut self, format: &Format) {
        self.default_format = Some(format.as_cell_style().clone());
        self.xlsx_normal_font_size_pt = None;
        self.xlsb_normal_font_size_pt = None;
        self.xlsb_cell_font_sizes_pt.clear();
        self.xlsb_row_font_sizes_pt.clear();
        self.xlsb_col_font_sizes_pt.clear();
        self.refresh_authored_display_texts();
    }
    /// Set the format for the header row cells of the named table.
    ///
    /// The `table_name` is the authored [`Table::name`]. The writer composes this
    /// over worksheet, column, and row defaults; explicit cell formats still win.
    /// [`crate::Workbook::to_xlsx_checked`] rejects names that do not match a table on
    /// this sheet.
    pub fn set_table_header_format(&mut self, table_name: impl AsRef<str>, format: &Format) {
        self.table_header_formats.insert(
            table_name.as_ref().to_string(),
            format.as_cell_style().clone(),
        );
        self.refresh_authored_display_texts();
    }
    /// Set the default row height (points) for rows without an explicit height.
    pub fn set_default_row_height(&mut self, points: f32) {
        self.default_row_height = Some(points);
        self.automatic_default_row_height_candidate = false;
        self.ooxml_implicit_row_height = OoxmlImplicitRowHeight::None;
        self.biff_application_default_row_height = false;
        self.imported_default_row_axis_measure = None;
        self.default_hidden_row_exceptions = None;
    }
    /// Set the default column width (character units) for columns without an
    /// explicit width.
    pub fn set_default_col_width(&mut self, chars: f32) {
        self.default_col_width = Some(chars);
        self.ooxml_implicit_col_width = OoxmlImplicitColumnWidth::None;
        self.ooxml_defaulted_base_col_width = false;
        self.xlsb_default_col_width = None;
        self.biff_application_default_col_width = false;
        self.imported_default_column_axis_measure = None;
    }
    /// Freeze the panes above `row` and left of `col`.
    pub fn freeze_panes(&mut self, row: u32, col: u16) {
        self.freeze = Some((row, col));
    }
    /// Apply an autofilter over the range `(r0,c0)..=(r1,c1)`.
    pub fn autofilter(&mut self, r0: u32, c0: u16, r1: u32, c1: u16) {
        self.autofilter = Some((r0, c0, r1, c1));
    }
    /// Set the print / page setup.
    pub fn set_page_setup(&mut self, ps: PageSetup) {
        let print_gridlines = self.print_metadata.print_gridlines;
        let print_headings = self.print_metadata.print_headings;
        self.print_metadata = PrintMetadata::authored();
        self.print_metadata
            .set_fit_to_page(ps.fit_to_width.is_some() || ps.fit_to_height.is_some());
        if let Some(value) = print_gridlines {
            self.print_metadata.set_print_gridlines(value);
        }
        if let Some(value) = print_headings {
            self.print_metadata.set_print_headings(value);
        }
        if let Some(area) = ps.print_area {
            self.print_metadata.push_print_area(area);
        }
        self.print_metadata
            .set_center_horizontally(ps.center_horizontally);
        self.print_metadata
            .set_center_vertically(ps.center_vertically);
        if let Some(header) = ps.header.clone() {
            self.print_metadata
                .set_header_footer(HeaderFooterKind::OddHeader, header);
        }
        if let Some(footer) = ps.footer.clone() {
            self.print_metadata
                .set_header_footer(HeaderFooterKind::OddFooter, footer);
        }
        self.page_setup = Some(ps);
    }
    /// Set the sheet tab color.
    pub fn set_tab_color(&mut self, color: impl Into<Color>) {
        self.tab_color = Some(color.into());
    }
    /// Protect the worksheet (locks cells against editing in Excel).
    pub fn protect(&mut self) {
        self.protect = true;
    }
    /// Protect the worksheet while permitting the actions enabled in `opts`
    /// (e.g. sorting, AutoFilter, formatting). Anything left `false` stays
    /// locked, exactly as [`Self::protect`].
    pub fn protect_with(&mut self, opts: ProtectionOptions) {
        self.protect = true;
        self.protect_options = Some(opts);
    }
    /// Add a data-validation rule (e.g. [`DataValidation::list`] for a dropdown).
    pub fn add_data_validation(&mut self, dv: DataValidation) {
        self.data_validations.push(dv);
    }
    /// Add a conditional-formatting rule over a range.
    pub fn add_conditional_format(&mut self, cf: CondFormat) {
        self.cond_formats.push(cf);
        self.cond_format_metadata
            .push(ConditionalFormatMetadata::default());
    }
    /// Embed an image anchored to a cell box.
    pub fn add_image(&mut self, img: Image) {
        self.images.push(img);
    }
    /// Add a chart anchored to a cell box.
    pub fn add_chart(&mut self, chart: Chart) {
        self.charts.push(chart);
    }
    /// Add a sparkline anchored to a single destination cell.
    pub fn add_sparkline(&mut self, sparkline: Sparkline) {
        self.sparklines.push(sparkline);
    }
    /// Add a worksheet table over a range (first row = header).
    pub fn add_table(&mut self, table: Table) {
        self.tables.push(table);
        self.refresh_authored_display_texts();
    }
    /// Attach a legacy cell comment / note to `(row, col)` with `text` and an
    /// optional `author`. Passing a direct author string is treated as `Some`.
    pub fn add_comment(
        &mut self,
        row: u32,
        col: u16,
        text: impl AsRef<str>,
        author: impl Into<CommentAuthor>,
    ) {
        self.comments.push(Comment {
            row,
            col,
            text: text.as_ref().to_string(),
            author: author.into().0,
        });
    }
    fn push_authored(
        &mut self,
        row: u32,
        col: u16,
        mut value: Cell,
        style: Option<CellStyle>,
        hyperlink: Option<String>,
    ) {
        // OOXML stores formula source without Excel's UI-only leading `=`.
        // Keep the public Cell invariant consistent across `write_formula`,
        // `write_formula_with_format`, and callers that pass Cell::Formula to
        // a generic write method. Match Spreadsheet::set_cell_formula by
        // accepting and removing every repeated leading marker.
        if let Cell::Formula { formula, .. } = &mut value {
            if formula.starts_with('=') {
                *formula = formula.trim_start_matches('=').to_string();
            }
        }
        // A plain write supersedes any rich-string runs previously set here;
        // `write_rich` re-inserts after calling this, so its own runs survive.
        self.rich.remove(&(row, col));
        self.blank_styles.remove(&(row, col));
        let num_fmt = self.effective_authored_num_fmt(row, col, style.as_ref());
        let text = display_text_with_num_fmt(&value, num_fmt, self.display_date1904);
        self.display_cell_index.take();
        self.cells.push(CellEntry {
            row,
            col,
            value,
            text,
            style,
            xlsx_font_size_pt: None,
            hyperlink,
        });
        self.xlsb_cell_font_sizes_pt.push(None);
    }

    fn effective_authored_num_fmt<'a>(
        &'a self,
        row: u32,
        col: u16,
        explicit: Option<&'a CellStyle>,
    ) -> Option<&'a str> {
        explicit
            .and_then(|style| style.num_fmt.as_deref())
            .or_else(|| {
                self.tables.iter().find_map(|table| {
                    let (r0, c0, _, c1) = table.range;
                    (row == r0 && col >= c0 && col <= c1)
                        .then(|| self.table_header_formats.get(&table.name))
                        .flatten()
                        .and_then(|style| style.num_fmt.as_deref())
                })
            })
            .or_else(|| {
                self.row_formats
                    .get(&row)
                    .and_then(|style| style.num_fmt.as_deref())
            })
            .or_else(|| {
                self.col_formats
                    .get(&col)
                    .and_then(|style| style.num_fmt.as_deref())
            })
            .or_else(|| {
                self.default_format
                    .as_ref()
                    .and_then(|style| style.num_fmt.as_deref())
            })
    }

    fn refresh_authored_display_texts(&mut self) {
        if self.style_fidelity != StyleFidelity::Authored {
            return;
        }
        let formats: Vec<Option<String>> = self
            .cells
            .iter()
            .map(|cell| {
                self.effective_authored_num_fmt(cell.row, cell.col, cell.style.as_ref())
                    .map(str::to_string)
            })
            .collect();
        for (cell, num_fmt) in self.cells.iter_mut().zip(formats) {
            cell.text =
                display_text_with_num_fmt(&cell.value, num_fmt.as_deref(), self.display_date1904);
        }
    }
}
