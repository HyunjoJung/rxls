const MAX_PRINT_AREAS: usize = 256;
const MAX_MANUAL_PAGE_BREAKS: usize = 1_026;
pub(super) const MAX_HEADER_FOOTER_BYTES: usize = 8_192;

/// Fidelity of source print metadata retained by a worksheet reader.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PrintFidelity {
    /// The reader did not expose source print metadata for this sheet.
    #[default]
    Unavailable,
    /// Useful print metadata was retained, with typed losses for omitted state.
    Partial,
    /// Every source print property represented by [`PrintMetadata`] was retained.
    Retained,
    /// The sheet was created through rxls authoring APIs.
    Authored,
}

/// Stable reason why source print metadata could not be represented exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PrintLossKind {
    /// A source print-area reference was malformed or outside the worksheet grid.
    InvalidPrintArea,
    /// A source manual page break was malformed or outside the worksheet grid.
    InvalidPageBreak,
    /// A referenced source definition or relationship was absent.
    MissingReference,
    /// A source property has no equivalent in the retained print model.
    UnsupportedProperty,
    /// Header or footer content was malformed and could not be retained exactly.
    MalformedHeaderFooter,
    /// A format-defined or rxls safety limit was reached.
    LimitExceeded,
}

/// One aggregated source print-metadata loss boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PrintLoss {
    /// Stable typed reason.
    pub kind: PrintLossKind,
    /// Number of occurrences, saturated at [`u32::MAX`].
    pub occurrences: u32,
}

/// Authored order used to traverse a worksheet's printed pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PrintPageOrder {
    /// Print down the worksheet first, then move to the next page column.
    DownThenOver,
    /// Print across the worksheet first, then move to the next page row.
    OverThenDown,
}

/// One of the six independently authored worksheet header/footer strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HeaderFooterKind {
    /// Header used on odd and default pages.
    OddHeader,
    /// Footer used on odd and default pages.
    OddFooter,
    /// Header used on even pages.
    EvenHeader,
    /// Footer used on even pages.
    EvenFooter,
    /// Header used on the first page.
    FirstHeader,
    /// Footer used on the first page.
    FirstFooter,
}

/// Distinct source header/footer strings and their authored behavior flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct HeaderFooterMetadata {
    odd_header: Option<String>,
    odd_footer: Option<String>,
    even_header: Option<String>,
    even_footer: Option<String>,
    first_header: Option<String>,
    first_footer: Option<String>,
    different_odd_even: Option<bool>,
    different_first: Option<bool>,
    scale_with_document: Option<bool>,
    align_with_margins: Option<bool>,
}

impl HeaderFooterMetadata {
    /// Return the retained string for `kind`, preserving source control codes.
    pub fn get(&self, kind: HeaderFooterKind) -> Option<&str> {
        match kind {
            HeaderFooterKind::OddHeader => self.odd_header.as_deref(),
            HeaderFooterKind::OddFooter => self.odd_footer.as_deref(),
            HeaderFooterKind::EvenHeader => self.even_header.as_deref(),
            HeaderFooterKind::EvenFooter => self.even_footer.as_deref(),
            HeaderFooterKind::FirstHeader => self.first_header.as_deref(),
            HeaderFooterKind::FirstFooter => self.first_footer.as_deref(),
        }
    }

    /// Header used on odd and default pages.
    pub fn odd_header(&self) -> Option<&str> {
        self.odd_header.as_deref()
    }

    /// Footer used on odd and default pages.
    pub fn odd_footer(&self) -> Option<&str> {
        self.odd_footer.as_deref()
    }

    /// Header used on even pages.
    pub fn even_header(&self) -> Option<&str> {
        self.even_header.as_deref()
    }

    /// Footer used on even pages.
    pub fn even_footer(&self) -> Option<&str> {
        self.even_footer.as_deref()
    }

    /// Header used on the first page.
    pub fn first_header(&self) -> Option<&str> {
        self.first_header.as_deref()
    }

    /// Footer used on the first page.
    pub fn first_footer(&self) -> Option<&str> {
        self.first_footer.as_deref()
    }

    /// Whether even pages use their distinct header/footer strings.
    pub fn different_odd_even(&self) -> Option<bool> {
        self.different_odd_even
    }

    /// Whether the first page uses its distinct header/footer strings.
    pub fn different_first(&self) -> Option<bool> {
        self.different_first
    }

    /// Whether header/footer text scales with the printed document.
    pub fn scale_with_document(&self) -> Option<bool> {
        self.scale_with_document
    }

    /// Whether header/footer positions align with page margins.
    pub fn align_with_margins(&self) -> Option<bool> {
        self.align_with_margins
    }

    fn slot_mut(&mut self, kind: HeaderFooterKind) -> &mut Option<String> {
        match kind {
            HeaderFooterKind::OddHeader => &mut self.odd_header,
            HeaderFooterKind::OddFooter => &mut self.odd_footer,
            HeaderFooterKind::EvenHeader => &mut self.even_header,
            HeaderFooterKind::EvenFooter => &mut self.even_footer,
            HeaderFooterKind::FirstHeader => &mut self.first_header,
            HeaderFooterKind::FirstFooter => &mut self.first_footer,
        }
    }
}

/// Bounded, loss-aware source print metadata retained beside [`PageSetup`].
///
/// The long-standing [`PageSetup`] remains the authoring and compatibility
/// facade. This sidecar preserves source-only details such as multiple print
/// areas, manual page breaks, page traversal order, and all six header/footer
/// variants without changing that public struct's fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PrintMetadata {
    fidelity: PrintFidelity,
    print_areas: Vec<(u32, u16, u32, u16)>,
    manual_row_breaks: Vec<u32>,
    manual_col_breaks: Vec<u16>,
    page_order: Option<PrintPageOrder>,
    fit_to_page: Option<bool>,
    pub(super) print_gridlines: Option<bool>,
    pub(super) print_headings: Option<bool>,
    center_horizontally: Option<bool>,
    center_vertically: Option<bool>,
    header_footer: HeaderFooterMetadata,
    losses: Vec<PrintLoss>,
}

impl PrintMetadata {
    /// Source-retention fidelity for this sheet's print metadata.
    pub fn fidelity(&self) -> PrintFidelity {
        self.fidelity
    }

    /// Source print areas in authored order, as inclusive zero-based ranges.
    pub fn print_areas(&self) -> &[(u32, u16, u32, u16)] {
        self.print_areas.as_slice()
    }

    /// Sorted, deduplicated zero-based row indexes with manual page breaks.
    pub fn manual_row_breaks(&self) -> &[u32] {
        self.manual_row_breaks.as_slice()
    }

    /// Sorted, deduplicated zero-based column indexes with manual page breaks.
    pub fn manual_col_breaks(&self) -> &[u16] {
        self.manual_col_breaks.as_slice()
    }

    /// Authored page traversal order, when the source exposes it.
    pub fn page_order(&self) -> Option<PrintPageOrder> {
        self.page_order
    }

    /// Explicit source selection of fit-to-pages (`true`) versus percentage
    /// scaling (`false`). `None` means the source format did not retain a mode.
    pub fn fit_to_page(&self) -> Option<bool> {
        self.fit_to_page
    }

    /// Explicit source setting for printing worksheet gridlines.
    pub fn print_gridlines(&self) -> Option<bool> {
        self.print_gridlines
    }

    /// Explicit source setting for printing row and column headings.
    pub fn print_headings(&self) -> Option<bool> {
        self.print_headings
    }

    /// Explicit source setting for horizontal print centering.
    pub fn center_horizontally(&self) -> Option<bool> {
        self.center_horizontally
    }

    /// Explicit source setting for vertical print centering.
    pub fn center_vertically(&self) -> Option<bool> {
        self.center_vertically
    }

    /// Distinct odd, even, and first-page header/footer metadata.
    pub fn header_footer(&self) -> &HeaderFooterMetadata {
        &self.header_footer
    }

    /// Aggregated typed source-loss reasons.
    pub fn losses(&self) -> &[PrintLoss] {
        self.losses.as_slice()
    }

    pub(crate) fn authored() -> Self {
        Self {
            fidelity: PrintFidelity::Authored,
            ..Self::default()
        }
    }

    pub(crate) fn mark_source(&mut self) {
        if self.fidelity == PrintFidelity::Unavailable {
            self.fidelity = PrintFidelity::Retained;
        }
    }

    pub(crate) fn add_loss(&mut self, kind: PrintLossKind) {
        self.mark_source();
        self.fidelity = PrintFidelity::Partial;
        if let Some(loss) = self.losses.iter_mut().find(|loss| loss.kind == kind) {
            loss.occurrences = loss.occurrences.saturating_add(1);
        } else {
            self.losses.push(PrintLoss {
                kind,
                occurrences: 1,
            });
        }
    }

    pub(crate) fn push_print_area(&mut self, area: (u32, u16, u32, u16)) {
        self.mark_source();
        if area.0 > area.2 || area.1 > area.3 || area.2 > 1_048_575 || area.3 > 16_383 {
            self.add_loss(PrintLossKind::InvalidPrintArea);
            return;
        }
        if self.print_areas.contains(&area) {
            return;
        }
        if self.print_areas.len() == MAX_PRINT_AREAS {
            self.add_loss(PrintLossKind::LimitExceeded);
            return;
        }
        self.print_areas.push(area);
    }

    pub(crate) fn push_manual_row_break(&mut self, row: u32) {
        self.mark_source();
        if row > 1_048_575 {
            self.add_loss(PrintLossKind::InvalidPageBreak);
            return;
        }
        if self.manual_row_breaks.binary_search(&row).is_ok() {
            return;
        }
        if self.manual_row_breaks.len() == MAX_MANUAL_PAGE_BREAKS {
            self.add_loss(PrintLossKind::LimitExceeded);
            return;
        }
        let index = self
            .manual_row_breaks
            .partition_point(|candidate| *candidate < row);
        self.manual_row_breaks.insert(index, row);
    }

    pub(crate) fn push_manual_col_break(&mut self, col: u16) {
        self.mark_source();
        if col > 16_383 {
            self.add_loss(PrintLossKind::InvalidPageBreak);
            return;
        }
        if self.manual_col_breaks.binary_search(&col).is_ok() {
            return;
        }
        if self.manual_col_breaks.len() == MAX_MANUAL_PAGE_BREAKS {
            self.add_loss(PrintLossKind::LimitExceeded);
            return;
        }
        let index = self
            .manual_col_breaks
            .partition_point(|candidate| *candidate < col);
        self.manual_col_breaks.insert(index, col);
    }

    pub(crate) fn set_page_order(&mut self, order: PrintPageOrder) {
        self.mark_source();
        self.page_order = Some(order);
    }

    pub(crate) fn set_fit_to_page(&mut self, value: bool) {
        self.mark_source();
        self.fit_to_page = Some(value);
    }

    pub(crate) fn set_print_gridlines(&mut self, value: bool) {
        self.mark_source();
        self.print_gridlines = Some(value);
    }

    pub(crate) fn set_print_headings(&mut self, value: bool) {
        self.mark_source();
        self.print_headings = Some(value);
    }

    pub(crate) fn set_center_horizontally(&mut self, value: bool) {
        self.mark_source();
        self.center_horizontally = Some(value);
    }

    pub(crate) fn set_center_vertically(&mut self, value: bool) {
        self.mark_source();
        self.center_vertically = Some(value);
    }

    pub(crate) fn set_header_footer_flag(
        &mut self,
        different_odd_even: Option<bool>,
        different_first: Option<bool>,
        scale_with_document: Option<bool>,
        align_with_margins: Option<bool>,
    ) {
        self.mark_source();
        self.header_footer.different_odd_even = different_odd_even;
        self.header_footer.different_first = different_first;
        self.header_footer.scale_with_document = scale_with_document;
        self.header_footer.align_with_margins = align_with_margins;
    }

    pub(crate) fn set_header_footer(&mut self, kind: HeaderFooterKind, value: String) {
        self.mark_source();
        let (value, truncated) = bounded_print_text(value);
        *self.header_footer.slot_mut(kind) = Some(value);
        if truncated {
            self.add_loss(PrintLossKind::LimitExceeded);
        }
    }

    #[cfg(any(feature = "xlsx", feature = "ods"))]
    pub(crate) fn append_header_footer(&mut self, kind: HeaderFooterKind, value: &str) {
        self.mark_source();
        let current_len = self
            .header_footer
            .get(kind)
            .map(str::len)
            .unwrap_or_default();
        if current_len >= MAX_HEADER_FOOTER_BYTES {
            if !value.is_empty() {
                self.add_loss(PrintLossKind::LimitExceeded);
            }
            return;
        }
        let available = MAX_HEADER_FOOTER_BYTES - current_len;
        let mut boundary = value.len().min(available);
        while boundary > 0 && !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        self.header_footer
            .slot_mut(kind)
            .get_or_insert_with(String::new)
            .push_str(&value[..boundary]);
        if boundary < value.len() {
            self.add_loss(PrintLossKind::LimitExceeded);
        }
    }
}

fn bounded_print_text(mut value: String) -> (String, bool) {
    if value.len() <= MAX_HEADER_FOOTER_BYTES {
        return (value, false);
    }
    let mut boundary = MAX_HEADER_FOOTER_BYTES;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    (value, true)
}

/// Print / page setup for a worksheet (authoring). All fields optional; an
/// unset field uses Excel's default.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PageSetup {
    /// Landscape orientation (default portrait).
    pub landscape: bool,
    /// Page margins in inches: `(left, right, top, bottom, header, footer)`.
    pub margins: Option<(f64, f64, f64, f64, f64, f64)>,
    /// Print area `(r0, c0, r1, c1)` (0-based, inclusive).
    pub print_area: Option<(u32, u16, u32, u16)>,
    /// Rows to repeat at the top of every printed page, `first..=last` (0-based).
    pub repeat_rows: Option<(u32, u32)>,
    /// Columns to repeat at the left of every printed page, `first..=last`
    /// (0-based).
    pub repeat_cols: Option<(u16, u16)>,
    /// Scale to fit this many pages wide (`fit_to_width`) / tall (`fit_to_height`).
    pub fit_to_width: Option<u16>,
    /// See [`Self::fit_to_width`].
    pub fit_to_height: Option<u16>,
    /// Header text (Excel `&`-codes, e.g. `&C&"Bold"Title`).
    pub header: Option<String>,
    /// Footer text (e.g. `&CPage &P of &N`).
    pub footer: Option<String>,
    /// Paper size code (Excel's `paperSize`, e.g. `1` = Letter, `9` = A4).
    pub paper_size: Option<u16>,
    /// Print scale as a percentage (10–400); ignored when fit-to-page is set.
    pub scale: Option<u16>,
    /// Center the print area horizontally on the page (`<printOptions
    /// horizontalCentered="1">`).
    pub center_horizontally: bool,
    /// Center the print area vertically on the page (`<printOptions
    /// verticalCentered="1">`).
    pub center_vertically: bool,
    /// First printed page number; emits `firstPageNumber="N" useFirstPageNumber="1"`
    /// on `<pageSetup>`. `None` uses Excel's default (auto).
    pub first_page_number: Option<u16>,
}

impl PageSetup {
    /// Construct page setup metadata using Excel defaults.
    pub fn new() -> Self {
        PageSetup::default()
    }

    /// Set landscape orientation.
    pub fn with_landscape(mut self) -> Self {
        self.landscape = true;
        self
    }

    /// Set page margins in inches: left, right, top, bottom, header, footer.
    pub fn with_margins(
        mut self,
        left: f64,
        right: f64,
        top: f64,
        bottom: f64,
        header: f64,
        footer: f64,
    ) -> Self {
        self.margins = Some((left, right, top, bottom, header, footer));
        self
    }

    /// Set the print area `(first_row, first_col, last_row, last_col)`.
    pub fn with_print_area(mut self, range: (u32, u16, u32, u16)) -> Self {
        self.print_area = Some(range);
        self
    }

    /// Set rows repeated at the top of every printed page.
    pub fn with_repeat_rows(mut self, first: u32, last: u32) -> Self {
        self.repeat_rows = Some((first, last));
        self
    }

    /// Set columns repeated at the left of every printed page.
    pub fn with_repeat_cols(mut self, first: u16, last: u16) -> Self {
        self.repeat_cols = Some((first, last));
        self
    }

    /// Scale output to fit this many pages wide and tall.
    pub fn with_fit_to_pages(mut self, width: u16, height: u16) -> Self {
        self.fit_to_width = Some(width);
        self.fit_to_height = Some(height);
        self
    }

    /// Set header text.
    pub fn with_header(mut self, header: impl AsRef<str>) -> Self {
        self.header = Some(header.as_ref().to_string());
        self
    }

    /// Set footer text.
    pub fn with_footer(mut self, footer: impl AsRef<str>) -> Self {
        self.footer = Some(footer.as_ref().to_string());
        self
    }

    /// Set the Excel paper size code.
    pub fn with_paper_size(mut self, paper_size: u16) -> Self {
        self.paper_size = Some(paper_size);
        self
    }

    /// Set print scaling percentage.
    pub fn with_scale(mut self, scale: u16) -> Self {
        self.scale = Some(scale);
        self
    }

    /// Center the print area horizontally on the page.
    pub fn with_center_horizontally(mut self, center: bool) -> Self {
        self.center_horizontally = center;
        self
    }

    /// Center the print area vertically on the page.
    pub fn with_center_vertically(mut self, center: bool) -> Self {
        self.center_vertically = center;
        self
    }

    /// Set the first printed page number.
    pub fn with_first_page_number(mut self, first_page_number: u16) -> Self {
        self.first_page_number = Some(first_page_number);
        self
    }
}
