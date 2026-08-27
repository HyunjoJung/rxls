use super::{
    Dimensions, FormulaRange, HeaderRow, ImageFmt, Picture, Range, Sheet, SheetMetadata, Table,
    WorksheetMetadata,
};

/// Public workbook-level metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkbookMetadata<'a> {
    /// `true` when the workbook uses the 1904 date system.
    pub date1904: bool,
    /// `true` when the reader omitted additional text-bearing cells after hitting
    /// the workbook-wide text allocation cap.
    pub text_truncated: bool,
    /// `true` when workbook structure protection is enabled.
    pub structure_protected: bool,
    /// 0-based active/selected sheet index, if it points at an existing sheet.
    pub active_sheet: Option<usize>,
    /// Active/selected sheet name, if the active sheet index is valid.
    pub active_sheet_name: Option<&'a str>,
    /// Document properties parsed from the workbook package.
    pub properties: &'a DocProperties,
    /// Workbook-global defined names as `(name, refers_to)`.
    pub defined_names: &'a [(String, String)],
    /// Worksheet-scoped defined names.
    pub local_defined_names: &'a [LocalDefinedName],
    /// Sheet metadata in workbook order.
    pub sheets: Vec<SheetMetadata>,
}

impl<'a> WorkbookMetadata<'a> {
    /// `true` when the workbook uses the 1904 date system.
    pub fn has_1904_epoch(&self) -> bool {
        self.date1904
    }

    /// `true` when text-bearing cells were omitted after hitting the text cap.
    pub fn is_text_truncated(&self) -> bool {
        self.text_truncated
    }

    /// `true` when workbook structure protection is enabled.
    pub fn is_structure_protected(&self) -> bool {
        self.structure_protected
    }

    /// 0-based active/selected sheet index, if it points at an existing sheet.
    pub fn active_sheet_index(&self) -> Option<usize> {
        self.active_sheet
    }

    /// Active/selected sheet name, if the active sheet index is valid.
    pub fn active_sheet_name(&self) -> Option<&'a str> {
        self.active_sheet_name
    }

    /// Document properties parsed from the workbook package.
    pub fn document_properties(&self) -> &'a DocProperties {
        self.properties
    }

    /// Workbook-global defined names as `(name, refers_to)`.
    pub fn defined_names(&self) -> &'a [(String, String)] {
        self.defined_names
    }

    /// Sheet metadata in workbook order.
    pub fn sheets(&self) -> &[SheetMetadata] {
        self.sheets.as_slice()
    }
}

/// A worksheet-scoped defined name retained independently from global names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDefinedName {
    /// Worksheet name that owns this local name.
    pub sheet: String,
    /// Name visible within `sheet`.
    pub name: String,
    /// Formula or reference text represented by the name.
    pub refers_to: String,
}

/// A workbook — parsed from `.xls`/`.xlsx`, or built for authoring via
/// [`Workbook::new`].
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Workbook {
    /// Sheets in workbook order.
    pub sheets: Vec<Sheet>,
    /// `true` if the workbook uses the 1904 date system (Mac Excel), which shifts
    /// how [`crate::Cell::Date`] serials map to calendar dates.
    pub date1904: bool,
    /// `true` when a reader hit the workbook-wide text allocation cap and omitted
    /// additional cells to keep extraction bounded.
    pub text_truncated: bool,
    /// Document properties (title / author / dates …). Populated from `.xlsx`
    /// and `.xlsb` `docProps/*`, `.xls` OLE SummaryInformation streams, and
    /// `.ods` `meta.xml` on read, and written into `.xlsx` `docProps/*` when
    /// authoring. Empty fields are omitted on write.
    pub properties: DocProperties,
    /// Workbook-global defined names as `(name, refers_to)` (authoring), e.g.
    /// `("TaxRate", "Sheet1!$B$1")`. Set via [`Workbook::define_name`].
    pub defined_names: Vec<(String, String)>,
    /// Sheet-scoped defined names retained from readers and authoring.
    pub local_defined_names: Vec<LocalDefinedName>,
    /// Container path retained from the reader. Authored workbooks use the
    /// default `NotApplicable` value.
    pub(crate) container_parse_mode: crate::ContainerParseMode,
    /// 0-based index of the active/selected sheet (authoring), emitted as
    /// `<workbookView activeTab="N"/>` plus `tabSelected="1"` on that sheet's
    /// view. Defaults to `0`; set via [`Workbook::set_active_sheet`].
    pub(crate) active_sheet: usize,
    /// Lock the workbook structure (authoring): emit `<workbookProtection
    /// lockStructure="1"/>` so sheets cannot be added, deleted, renamed, or
    /// reordered in Excel. Set via [`Workbook::protect_structure`].
    pub(crate) protect_structure: bool,
    /// Calamine-style header row policy for workbook-level worksheet ranges.
    pub(crate) header_row: HeaderRow,
}

/// Calamine-style read facade for generic workbook consumers.
///
/// `Workbook` exposes these methods directly as inherent methods; this trait
/// adds a small compatibility layer for diagnostics and libraries that want to
/// accept any rxls reader-like workbook value without naming the concrete type.
pub trait Reader {
    /// Worksheet names in workbook order.
    fn sheet_names(&self) -> Vec<&str>;
    /// Sheet metadata in workbook order.
    fn sheets_metadata(&self) -> Vec<SheetMetadata>;
    /// Workbook-global defined names as `(name, refers_to)`.
    fn defined_names(&self) -> &[(String, String)];
    /// Worksheet-scoped defined names in workbook order.
    fn local_defined_names(&self) -> &[LocalDefinedName];
    /// Set the row used as the top of workbook-level worksheet ranges.
    fn with_header_row(&mut self, header_row: HeaderRow) -> &mut Self
    where
        Self: Sized;
    /// Current header row policy for workbook-level worksheet ranges.
    fn header_row(&self) -> HeaderRow;
    /// Build a rectangular [`Range`] view for a worksheet by name.
    fn worksheet_range(&self, name: &str) -> Option<Range<'_>>;
    /// Build a borrowed rectangular [`Range`] view for a worksheet by name.
    ///
    /// This calamine-style `ReaderRef` alias defaults to [`Reader::worksheet_range`]
    /// because rxls ranges already borrow worksheet cells.
    fn worksheet_range_ref(&self, name: &str) -> Option<Range<'_>> {
        self.worksheet_range(name)
    }
    /// Build a rectangular [`Range`] view for the worksheet at workbook index.
    fn worksheet_range_at(&self, index: usize) -> Option<Range<'_>>;
    /// Build a borrowed rectangular [`Range`] view for the worksheet at workbook
    /// index.
    ///
    /// This calamine-style `ReaderRef` alias defaults to
    /// [`Reader::worksheet_range_at`] because rxls ranges already borrow
    /// worksheet cells.
    fn worksheet_range_at_ref(&self, index: usize) -> Option<Range<'_>> {
        self.worksheet_range_at(index)
    }
    /// Build a formula-text range for a worksheet by name.
    fn worksheet_formula(&self, name: &str) -> Option<FormulaRange<'_>>;
    /// Build a borrowed formula-text range for a worksheet by name.
    ///
    /// This calamine-style `ReaderRef` alias defaults to
    /// [`Reader::worksheet_formula`] because rxls formula ranges already borrow
    /// worksheet formula cells.
    fn worksheet_formula_ref(&self, name: &str) -> Option<FormulaRange<'_>> {
        self.worksheet_formula(name)
    }
    /// Build a formula-text range for the worksheet at workbook index.
    fn worksheet_formula_at(&self, index: usize) -> Option<FormulaRange<'_>>;
    /// Build a borrowed formula-text range for the worksheet at workbook index.
    ///
    /// This calamine-style `ReaderRef` alias defaults to
    /// [`Reader::worksheet_formula_at`] because rxls formula ranges already
    /// borrow worksheet formula cells.
    fn worksheet_formula_at_ref(&self, index: usize) -> Option<FormulaRange<'_>> {
        self.worksheet_formula_at(index)
    }
    /// Merged cell ranges for a worksheet by name.
    fn worksheet_merge_cells(&self, name: &str) -> Option<&[(u32, u16, u32, u16)]>;
    /// Merged cell ranges for the worksheet at workbook index.
    fn worksheet_merge_cells_at(&self, index: usize) -> Option<&[(u32, u16, u32, u16)]>;
    /// All merged regions as `(sheet_name, dimensions)` in workbook order.
    fn merged_regions(&self) -> Vec<(&str, Dimensions)>;
    /// Merged regions for one worksheet name.
    fn merged_regions_by_sheet(&self, name: &str) -> Vec<Dimensions>;
    /// Grouped worksheet metadata for a worksheet by name.
    fn worksheet_metadata(&self, name: &str) -> Option<WorksheetMetadata<'_>>;
    /// Grouped worksheet metadata for the worksheet at workbook index.
    fn worksheet_metadata_at(&self, index: usize) -> Option<WorksheetMetadata<'_>>;
    /// Grouped worksheet metadata for all worksheets in workbook order.
    fn worksheets_metadata(&self) -> Vec<WorksheetMetadata<'_>>;
    /// Fetch all worksheet data as `(sheet_name, range)` in workbook order.
    fn worksheets(&self) -> Vec<(String, Range<'_>)>;
    /// Workbook-level metadata grouped into one public facade.
    fn metadata(&self) -> WorkbookMetadata<'_>;
    /// `true` if the workbook uses the 1904 date epoch.
    fn has_1904_epoch(&self) -> bool {
        self.metadata().date1904
    }
    /// 0-based active/selected sheet index, if it points at an existing sheet.
    fn active_sheet_index(&self) -> Option<usize> {
        self.metadata().active_sheet
    }
    /// Active/selected sheet name, if the active sheet index is valid.
    fn active_sheet_name(&self) -> Option<&str> {
        self.metadata().active_sheet_name
    }
    /// Workbook-level embedded pictures as `(extension, bytes)`.
    fn pictures(&self) -> Option<Vec<(String, Vec<u8>)>>;
    /// Workbook-level embedded pictures with sheet and anchor metadata.
    fn pictures_with_metadata(&self) -> Vec<Picture>;
    /// Workbook-level worksheet table names in workbook/sheet order.
    fn table_names(&self) -> Vec<&str>;
    /// Worksheet table names for `sheet_name`.
    fn table_names_in_sheet(&self, sheet_name: &str) -> Vec<&str>;
    /// Find a worksheet table by name.
    fn table_by_name(&self, table_name: &str) -> Option<(&str, &Table)>;
    /// Find a worksheet table by name through the borrowed table facade.
    ///
    /// rxls table metadata is already borrowed from the workbook, so this
    /// calamine-style alias is identical to [`Reader::table_by_name`].
    fn table_by_name_ref(&self, table_name: &str) -> Option<(&str, &Table)> {
        self.table_by_name(table_name)
    }
    /// Find a worksheet table by name and return its data body as a [`Range`].
    fn table_data_by_name(&self, table_name: &str) -> Option<(&str, Range<'_>)>;
    /// Find a worksheet table by name and return a borrowed data-body range.
    ///
    /// rxls ranges already borrow worksheet cells, so this calamine-style alias
    /// is identical to [`Reader::table_data_by_name`].
    fn table_data_by_name_ref(&self, table_name: &str) -> Option<(&str, Range<'_>)> {
        self.table_data_by_name(table_name)
    }
}

/// Workbook document properties (Dublin Core + extended), read from OOXML
/// `docProps/*`, ODF `meta.xml`, and legacy OLE property streams, and written to
/// `docProps/core.xml` and `docProps/app.xml` for `.xlsx`. Every field is
/// optional; only the set ones are emitted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DocProperties {
    /// `dc:title`.
    pub title: Option<String>,
    /// `dc:subject`.
    pub subject: Option<String>,
    /// `dc:creator` (author).
    pub creator: Option<String>,
    /// `cp:keywords`.
    pub keywords: Option<String>,
    /// `dc:description` (comments).
    pub description: Option<String>,
    /// `cp:lastModifiedBy`.
    pub last_modified_by: Option<String>,
    /// `<Company>` in the extended properties.
    pub company: Option<String>,
    /// W3CDTF timestamp (e.g. `2024-01-01T00:00:00Z`) used for both
    /// `dcterms:created` and `dcterms:modified`.
    pub created: Option<String>,
}

impl DocProperties {
    /// Construct empty workbook document properties.
    pub fn new() -> Self {
        DocProperties::default()
    }

    /// Set the document title.
    pub fn with_title(mut self, title: impl AsRef<str>) -> Self {
        self.title = Some(title.as_ref().to_string());
        self
    }

    /// Set the document subject.
    pub fn with_subject(mut self, subject: impl AsRef<str>) -> Self {
        self.subject = Some(subject.as_ref().to_string());
        self
    }

    /// Set the document creator/author.
    pub fn with_creator(mut self, creator: impl AsRef<str>) -> Self {
        self.creator = Some(creator.as_ref().to_string());
        self
    }

    /// Set the document keywords.
    pub fn with_keywords(mut self, keywords: impl AsRef<str>) -> Self {
        self.keywords = Some(keywords.as_ref().to_string());
        self
    }

    /// Set the document description/comments.
    pub fn with_description(mut self, description: impl AsRef<str>) -> Self {
        self.description = Some(description.as_ref().to_string());
        self
    }

    /// Set the last-modified-by property.
    pub fn with_last_modified_by(mut self, last_modified_by: impl AsRef<str>) -> Self {
        self.last_modified_by = Some(last_modified_by.as_ref().to_string());
        self
    }

    /// Set the company extended property.
    pub fn with_company(mut self, company: impl AsRef<str>) -> Self {
        self.company = Some(company.as_ref().to_string());
        self
    }

    /// Set the W3CDTF creation/modification timestamp text.
    pub fn with_created(mut self, created: impl AsRef<str>) -> Self {
        self.created = Some(created.as_ref().to_string());
        self
    }
}

fn image_extension(format: ImageFmt) -> &'static str {
    match format {
        ImageFmt::Png => "png",
        ImageFmt::Jpeg => "jpg",
    }
}

impl Workbook {
    /// Flatten every worksheet to text, each prefixed with `# <name>`.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for sheet in self.sheets.iter().filter(|s| s.is_worksheet) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("# ");
            out.push_str(&sheet.name);
            out.push('\n');
            out.push_str(&sheet.to_text());
            out.push('\n');
        }
        out
    }

    /// Export the worksheet at `sheet_index` as CSV using comma separators.
    pub fn to_csv(&self, sheet_index: usize) -> Option<String> {
        self.to_csv_with_delimiter(sheet_index, ',')
    }

    /// Export the worksheet at `sheet_index` as delimiter-separated values.
    ///
    /// Returns `None` for an out-of-range `sheet_index`, a non-worksheet
    /// (e.g. a chart sheet), or an invalid `delimiter` of `'"'` -- that
    /// character can't act as both the field separator and the quoted-field
    /// boundary without making the output ambiguous, so this method rejects
    /// it outright rather than emitting it (contrast with
    /// [`Sheet::to_csv_with_delimiter`], whose `String` return type can't
    /// signal failure and instead normalizes `'"'` to `','`).
    pub fn to_csv_with_delimiter(&self, sheet_index: usize, delimiter: char) -> Option<String> {
        if delimiter == '"' {
            return None;
        }
        self.sheets
            .get(sheet_index)
            .filter(|sheet| sheet.is_worksheet)
            .map(|sheet| sheet.to_csv_with_delimiter(delimiter))
    }

    /// Export the worksheet at `sheet_index` as an HTML table fragment.
    pub fn to_html(&self, sheet_index: usize) -> Option<String> {
        self.sheets
            .get(sheet_index)
            .filter(|sheet| sheet.is_worksheet)
            .map(Sheet::to_html)
    }

    /// Export the worksheet at `sheet_index` as GitHub-flavored Markdown.
    pub fn to_markdown(&self, sheet_index: usize) -> Option<String> {
        self.sheets
            .get(sheet_index)
            .filter(|sheet| sheet.is_worksheet)
            .map(Sheet::to_markdown)
    }

    /// `true` when parsing produced a bounded, partial workbook rather than every
    /// text-bearing cell in the source file.
    pub fn is_partial(&self) -> bool {
        self.text_truncated
    }

    /// Return typed, bounded provenance for this workbook's successful parse.
    ///
    /// Authored workbooks report [`crate::ContainerParseMode::NotApplicable`]. A
    /// recovered parse remains subject to the normal strict edit/save
    /// safeguards; provenance is an audit signal, not a validity certificate.
    pub fn parse_provenance(&self) -> crate::ParseProvenance {
        crate::ParseProvenance::from_state(self.container_parse_mode, self.text_truncated)
    }

    /// `true` if the workbook uses the 1904 date epoch.
    ///
    /// This is a calamine-style alias over [`Workbook::date1904`].
    pub fn has_1904_epoch(&self) -> bool {
        self.date1904
    }

    /// Set the calamine-style row used as the top of workbook-level worksheet
    /// ranges.
    ///
    /// [`HeaderRow::FirstNonEmptyRow`] leaves worksheet ranges unchanged. An
    /// explicit [`HeaderRow::Row`] clips [`Workbook::worksheet_range`],
    /// [`Workbook::worksheet_range_at`], and [`Workbook::worksheets`] so the
    /// returned range starts at that absolute worksheet row.
    pub fn with_header_row(&mut self, header_row: HeaderRow) -> &mut Self {
        self.header_row = header_row;
        self
    }

    /// Current header row policy for workbook-level worksheet ranges.
    pub fn header_row(&self) -> HeaderRow {
        self.header_row
    }

    fn apply_header_row_to_range<'a>(&self, range: Range<'a>) -> Range<'a> {
        match self.header_row {
            HeaderRow::FirstNonEmptyRow => range,
            HeaderRow::Row(header_row) => {
                let (Some((_, start_col)), Some((end_row, end_col))) = (range.start(), range.end())
                else {
                    return range;
                };
                if header_row > end_row {
                    Range::empty()
                } else {
                    range.range((header_row, start_col), (end_row, end_col))
                }
            }
        }
    }

    /// `true` when workbook structure protection is enabled.
    pub fn is_structure_protected(&self) -> bool {
        self.protect_structure
    }

    /// 0-based active/selected sheet index, if it points at an existing sheet.
    pub fn active_sheet_index(&self) -> Option<usize> {
        (self.active_sheet < self.sheets.len()).then_some(self.active_sheet)
    }

    /// Active/selected sheet name, if the active sheet index is valid.
    pub fn active_sheet_name(&self) -> Option<&str> {
        self.active_sheet_index()
            .and_then(|index| self.sheets.get(index))
            .map(|sheet| sheet.name.as_str())
    }

    /// Workbook-level metadata grouped into one public facade.
    pub fn metadata(&self) -> WorkbookMetadata<'_> {
        WorkbookMetadata {
            date1904: self.date1904,
            text_truncated: self.text_truncated,
            structure_protected: self.is_structure_protected(),
            active_sheet: self.active_sheet_index(),
            active_sheet_name: self.active_sheet_name(),
            properties: &self.properties,
            defined_names: &self.defined_names,
            local_defined_names: &self.local_defined_names,
            sheets: self.sheets_metadata(),
        }
    }

    /// Workbook-level embedded pictures as `(extension, bytes)`, in workbook
    /// sheet order.
    ///
    /// This is a calamine-style aggregate over [`Sheet::images`]. It returns
    /// `None` when no supported embedded pictures are present.
    pub fn pictures(&self) -> Option<Vec<(String, Vec<u8>)>> {
        let pictures: Vec<_> = self
            .sheets
            .iter()
            .flat_map(|sheet| {
                sheet.images().iter().map(|image| {
                    (
                        image_extension(image.format).to_string(),
                        image.data.clone(),
                    )
                })
            })
            .collect();
        (!pictures.is_empty()).then_some(pictures)
    }

    /// Workbook-level embedded pictures with sheet and top-left anchor metadata,
    /// in workbook sheet order.
    ///
    /// This is a calamine-style aggregate over [`Sheet::images`]. `name` is
    /// empty until rxls stores stable drawing object names in [`crate::Image`].
    pub fn pictures_with_metadata(&self) -> Vec<Picture> {
        self.sheets
            .iter()
            .flat_map(|sheet| {
                sheet.images().iter().map(|image| Picture {
                    row: image.from.0,
                    col: u32::from(image.from.1),
                    sheet_name: sheet.name.clone(),
                    extension: image_extension(image.format).to_string(),
                    data: image.data.clone(),
                    name: String::new(),
                })
            })
            .collect()
    }

    /// Workbook-level worksheet table names in workbook/sheet order.
    ///
    /// This is a calamine-style discovery facade over the sheet-owned
    /// [`Sheet::tables`] metadata populated by supported readers.
    pub fn table_names(&self) -> Vec<&str> {
        self.sheets
            .iter()
            .flat_map(|sheet| sheet.tables().iter().map(|table| table.name.as_str()))
            .collect()
    }

    /// Worksheet table names for `sheet_name`, or an empty vector when the sheet
    /// is absent or has no table metadata.
    pub fn table_names_in_sheet(&self, sheet_name: &str) -> Vec<&str> {
        self.sheet_by_name(sheet_name)
            .map(|sheet| {
                sheet
                    .tables()
                    .iter()
                    .map(|table| table.name.as_str())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Find a worksheet table by name, returning the parent sheet name plus the
    /// borrowed [`Table`] metadata.
    pub fn table_by_name(&self, table_name: &str) -> Option<(&str, &Table)> {
        self.sheets.iter().find_map(|sheet| {
            sheet
                .tables()
                .iter()
                .find(|table| table_name_eq(&table.name, table_name))
                .map(|table| (sheet.name.as_str(), table))
        })
    }

    /// Find a worksheet table by name through the borrowed table facade.
    ///
    /// rxls table metadata is already borrowed from the workbook, so this
    /// calamine-style alias is identical to [`Workbook::table_by_name`].
    pub fn table_by_name_ref(&self, table_name: &str) -> Option<(&str, &Table)> {
        self.table_by_name(table_name)
    }

    /// Find a worksheet table by name, returning the parent sheet name plus a
    /// rectangular [`Range`] over the table's data body.
    ///
    /// The returned range excludes the table header row, matching calamine's
    /// `Table::data` surface. Header-only tables return an empty range while
    /// still reporting the table's parent sheet.
    pub fn table_data_by_name(&self, table_name: &str) -> Option<(&str, Range<'_>)> {
        self.sheets.iter().find_map(|sheet| {
            let table = sheet
                .tables()
                .iter()
                .find(|table| table_name_eq(&table.name, table_name))?;
            Some((sheet.name.as_str(), table_data_range(sheet, table)))
        })
    }

    /// Find a worksheet table by name and return a borrowed data-body range.
    ///
    /// rxls ranges already borrow worksheet cells, so this calamine-style alias
    /// is identical to [`Workbook::table_data_by_name`].
    pub fn table_data_by_name_ref(&self, table_name: &str) -> Option<(&str, Range<'_>)> {
        self.table_data_by_name(table_name)
    }
}

impl Table {
    /// Build a borrowed range over this table's data body in `sheet`.
    ///
    /// The returned range excludes the table header row, matching calamine's
    /// table-data surface while preserving rxls' borrowed sparse range model.
    pub fn data<'a>(&self, sheet: &'a Sheet) -> Range<'a> {
        table_data_range(sheet, self)
    }
}

fn table_data_range<'a>(sheet: &'a Sheet, table: &Table) -> Range<'a> {
    let Some(first_data_row) = table.range.0.checked_add(1) else {
        return Range::empty();
    };
    if first_data_row > table.range.2 {
        return Range::empty();
    }
    sheet.range().range(
        (first_data_row, u32::from(table.range.1)),
        (table.range.2, u32::from(table.range.3)),
    )
}

fn table_name_eq(left: &str, right: &str) -> bool {
    left.chars()
        .flat_map(char::to_lowercase)
        .eq(right.chars().flat_map(char::to_lowercase))
}

impl Workbook {
    /// A new empty workbook for authoring.
    pub fn new() -> Self {
        Workbook::default()
    }

    /// Like [`to_xlsx`](Self::to_xlsx), but **validates first**: returns a typed
    /// [`WriteError`](crate::WriteError) for the first structural problem the
    /// infallible writer would otherwise silently sanitize (out-of-grid or reversed
    /// cells/merges/ranges, authored cell/formula XML text, duplicate/invalid
    /// sheet or table names, a table range whose width disagrees with its column
    /// count, table-header format target mismatches, nested formula cached values,
    /// active-sheet index mistakes, too many sheets). On success the bytes are exactly what
    /// [`to_xlsx`](Self::to_xlsx) produces, unmodified.
    ///
    /// This is a best-effort structural pre-flight, not an exhaustive Excel
    /// validator: it checks ranges and bounds, but not formula syntax, chart
    /// series references, or consumer-specific rendering details.
    ///
    /// Available with the default `xlsx` feature.
    ///
    /// # Errors
    ///
    /// Returns the first typed [`WriteError`](crate::WriteError) found during
    /// structural, text, range, or output-budget validation.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), rxls::WriteError> {
    /// let mut workbook = rxls::Workbook::new();
    /// workbook.add_sheet("Data").write(0, 0, "ready");
    /// let bytes = workbook.to_xlsx_checked()?;
    /// assert!(bytes.starts_with(b"PK"));
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "xlsx")]
    pub fn to_xlsx_checked(&self) -> Result<Vec<u8>, crate::WriteError> {
        crate::write::validate(self)?;
        Ok(self.to_xlsx())
    }
    /// Append a worksheet and return a mutable handle to it.
    pub fn add_sheet(&mut self, name: impl AsRef<str>) -> &mut Sheet {
        let sheet = Sheet::new_with_display_epoch(name, self.date1904);
        self.sheets.push(sheet);
        self.sheets
            .last_mut()
            .expect("just pushed a sheet, so last_mut is Some")
    }
    /// Define a workbook-global name pointing at `refers_to` (e.g.
    /// `define_name("TaxRate", "Sheet1!$B$1")`), emitted as a `<definedName>`
    /// when authoring an `.xlsx`.
    pub fn define_name(&mut self, name: impl AsRef<str>, refers_to: impl AsRef<str>) {
        self.defined_names
            .push((name.as_ref().to_string(), refers_to.as_ref().to_string()));
    }
    /// Define a name scoped to one worksheet. The checked writer rejects an
    /// unknown sheet; the infallible writer omits such an invalid entry.
    pub fn define_local_name(
        &mut self,
        sheet: impl AsRef<str>,
        name: impl AsRef<str>,
        refers_to: impl AsRef<str>,
    ) {
        self.local_defined_names.push(LocalDefinedName {
            sheet: sheet.as_ref().to_string(),
            name: name.as_ref().to_string(),
            refers_to: refers_to.as_ref().to_string(),
        });
    }
    /// Set workbook document properties for `.xlsx` authoring.
    pub fn set_properties(&mut self, properties: DocProperties) {
        self.properties = properties;
    }
    /// Set the 0-based index of the active/selected sheet, emitted as
    /// `<workbookView activeTab="N"/>` and `tabSelected="1"` on that sheet's
    /// view when authoring an `.xlsx`. An out-of-range index is tolerated by the
    /// infallible writer (it falls back to no selection) and rejected by
    /// [`Workbook::to_xlsx_checked`].
    pub fn set_active_sheet(&mut self, idx: usize) {
        self.active_sheet = idx;
    }
    /// Lock the workbook structure (authoring): emits `<workbookProtection
    /// lockStructure="1"/>` so Excel forbids adding, deleting, renaming, hiding,
    /// or reordering sheets. No password is set (structure is locked but
    /// unprotectable without one). Distinct from per-sheet [`Sheet::protect`].
    pub fn protect_structure(&mut self) {
        self.protect_structure = true;
    }
    /// Workbook-global defined names as `(name, refers_to)` — the read accessor
    /// over [`Self::defined_names`], populated by the `.xlsx` reader,
    /// workbook-global `.xls` `Lbl` / `.xlsb` `BrtName` records, and `.ods`
    /// named ranges, then round-tripped by the writer. Built-in `_xlnm.*` names
    /// Sheet-local user names are exposed separately through
    /// [`Self::local_defined_names`].
    pub fn defined_names(&self) -> &[(String, String)] {
        &self.defined_names
    }
    /// Sheet-scoped defined names retained by readers or added for authoring.
    pub fn local_defined_names(&self) -> &[LocalDefinedName] {
        &self.local_defined_names
    }
    /// Find a worksheet by name (case-sensitive) — the calamine-style by-name
    /// accessor over [`Self::sheets`].
    pub fn sheet_by_name(&self, name: &str) -> Option<&Sheet> {
        self.sheets.iter().find(|s| s.name == name)
    }
    /// Build a rectangular [`Range`] view for a worksheet by name.
    pub fn worksheet_range(&self, name: &str) -> Option<Range<'_>> {
        self.sheet_by_name(name)
            .filter(|sheet| sheet.is_worksheet)
            .map(|sheet| self.apply_header_row_to_range(sheet.range()))
    }
    /// Build a borrowed rectangular [`Range`] view for a worksheet by name.
    ///
    /// rxls ranges are already borrowed views over sparse worksheet cells, so
    /// this calamine-style `ReaderRef` alias is identical to
    /// [`Workbook::worksheet_range`].
    pub fn worksheet_range_ref(&self, name: &str) -> Option<Range<'_>> {
        self.worksheet_range(name)
    }
    /// Build a rectangular [`Range`] view for the worksheet at workbook index.
    pub fn worksheet_range_at(&self, index: usize) -> Option<Range<'_>> {
        self.sheets
            .get(index)
            .filter(|sheet| sheet.is_worksheet)
            .map(|sheet| self.apply_header_row_to_range(sheet.range()))
    }
    /// Build a borrowed rectangular [`Range`] view for the worksheet at workbook
    /// index.
    ///
    /// rxls ranges are already borrowed views over sparse worksheet cells, so
    /// this calamine-style `ReaderRef` alias is identical to
    /// [`Workbook::worksheet_range_at`].
    pub fn worksheet_range_at_ref(&self, index: usize) -> Option<Range<'_>> {
        self.worksheet_range_at(index)
    }
    /// Fetch all worksheet data as `(sheet_name, range)` in workbook order.
    pub fn worksheets(&self) -> Vec<(String, Range<'_>)> {
        self.sheets
            .iter()
            .filter(|sheet| sheet.is_worksheet)
            .map(|sheet| {
                (
                    sheet.name.clone(),
                    self.apply_header_row_to_range(sheet.range()),
                )
            })
            .collect()
    }
    /// Build a formula-text range for a worksheet by name.
    pub fn worksheet_formula(&self, name: &str) -> Option<FormulaRange<'_>> {
        self.sheet_by_name(name)
            .filter(|sheet| sheet.is_worksheet)
            .map(Sheet::formula_range)
    }
    /// Build a borrowed formula-text range for a worksheet by name.
    ///
    /// rxls formula ranges are already borrowed views over sparse worksheet
    /// formulas, so this calamine-style `ReaderRef` alias is identical to
    /// [`Workbook::worksheet_formula`].
    pub fn worksheet_formula_ref(&self, name: &str) -> Option<FormulaRange<'_>> {
        self.worksheet_formula(name)
    }
    /// Build a formula-text range for the worksheet at workbook index.
    pub fn worksheet_formula_at(&self, index: usize) -> Option<FormulaRange<'_>> {
        self.sheets
            .get(index)
            .filter(|sheet| sheet.is_worksheet)
            .map(Sheet::formula_range)
    }
    /// Build a borrowed formula-text range for the worksheet at workbook index.
    ///
    /// rxls formula ranges are already borrowed views over sparse worksheet
    /// formulas, so this calamine-style `ReaderRef` alias is identical to
    /// [`Workbook::worksheet_formula_at`].
    pub fn worksheet_formula_at_ref(&self, index: usize) -> Option<FormulaRange<'_>> {
        self.worksheet_formula_at(index)
    }
    /// Merged cell ranges for a worksheet by name.
    pub fn worksheet_merge_cells(&self, name: &str) -> Option<&[(u32, u16, u32, u16)]> {
        self.sheet_by_name(name)
            .filter(|sheet| sheet.is_worksheet)
            .map(Sheet::merged_ranges)
    }
    /// Merged cell ranges for the worksheet at workbook index.
    pub fn worksheet_merge_cells_at(&self, index: usize) -> Option<&[(u32, u16, u32, u16)]> {
        self.sheets
            .get(index)
            .filter(|sheet| sheet.is_worksheet)
            .map(Sheet::merged_ranges)
    }

    /// All merged regions as `(sheet_name, dimensions)` in workbook order.
    ///
    /// This is a calamine-style aggregate over the sheet-owned merge metadata.
    /// rxls does not expose package part paths in this facade, so callers get
    /// the owning sheet name plus typed inclusive dimensions.
    pub fn merged_regions(&self) -> Vec<(&str, Dimensions)> {
        self.sheets
            .iter()
            .filter(|sheet| sheet.is_worksheet)
            .flat_map(|sheet| {
                sheet
                    .merged_ranges()
                    .iter()
                    .map(move |&range| (sheet.name.as_str(), Dimensions::from_range_tuple(range)))
            })
            .collect()
    }

    /// Merged regions for one worksheet name.
    pub fn merged_regions_by_sheet(&self, name: &str) -> Vec<Dimensions> {
        self.sheet_by_name(name)
            .filter(|sheet| sheet.is_worksheet)
            .map(|sheet| {
                sheet
                    .merged_ranges()
                    .iter()
                    .map(|&range| Dimensions::from_range_tuple(range))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Grouped worksheet metadata for a worksheet by name.
    pub fn worksheet_metadata(&self, name: &str) -> Option<WorksheetMetadata<'_>> {
        self.sheet_by_name(name)
            .filter(|sheet| sheet.is_worksheet)
            .map(Sheet::metadata)
    }
    /// Grouped worksheet metadata for the worksheet at workbook index.
    pub fn worksheet_metadata_at(&self, index: usize) -> Option<WorksheetMetadata<'_>> {
        self.sheets
            .get(index)
            .filter(|sheet| sheet.is_worksheet)
            .map(Sheet::metadata)
    }
    /// Grouped worksheet metadata for all worksheets in workbook order.
    pub fn worksheets_metadata(&self) -> Vec<WorksheetMetadata<'_>> {
        self.sheets
            .iter()
            .filter(|sheet| sheet.is_worksheet)
            .map(Sheet::metadata)
            .collect()
    }
    /// Sheet metadata in workbook order.
    pub fn sheets_metadata(&self) -> Vec<SheetMetadata> {
        self.sheets
            .iter()
            .map(|sheet| SheetMetadata {
                name: sheet.name.clone(),
                typ: sheet.sheet_type(),
                visible: sheet.visible(),
            })
            .collect()
    }
    /// Worksheet names, in order.
    pub fn sheet_names(&self) -> Vec<&str> {
        self.sheets.iter().map(|s| s.name.as_str()).collect()
    }
}

impl Reader for Workbook {
    fn sheet_names(&self) -> Vec<&str> {
        Workbook::sheet_names(self)
    }

    fn sheets_metadata(&self) -> Vec<SheetMetadata> {
        Workbook::sheets_metadata(self)
    }

    fn defined_names(&self) -> &[(String, String)] {
        Workbook::defined_names(self)
    }

    fn local_defined_names(&self) -> &[LocalDefinedName] {
        Workbook::local_defined_names(self)
    }

    fn with_header_row(&mut self, header_row: HeaderRow) -> &mut Self {
        Workbook::with_header_row(self, header_row)
    }

    fn header_row(&self) -> HeaderRow {
        Workbook::header_row(self)
    }

    fn worksheet_range(&self, name: &str) -> Option<Range<'_>> {
        Workbook::worksheet_range(self, name)
    }

    fn worksheet_range_at(&self, index: usize) -> Option<Range<'_>> {
        Workbook::worksheet_range_at(self, index)
    }

    fn worksheet_formula(&self, name: &str) -> Option<FormulaRange<'_>> {
        Workbook::worksheet_formula(self, name)
    }

    fn worksheet_formula_at(&self, index: usize) -> Option<FormulaRange<'_>> {
        Workbook::worksheet_formula_at(self, index)
    }

    fn worksheet_merge_cells(&self, name: &str) -> Option<&[(u32, u16, u32, u16)]> {
        Workbook::worksheet_merge_cells(self, name)
    }

    fn worksheet_merge_cells_at(&self, index: usize) -> Option<&[(u32, u16, u32, u16)]> {
        Workbook::worksheet_merge_cells_at(self, index)
    }

    fn merged_regions(&self) -> Vec<(&str, Dimensions)> {
        Workbook::merged_regions(self)
    }

    fn merged_regions_by_sheet(&self, name: &str) -> Vec<Dimensions> {
        Workbook::merged_regions_by_sheet(self, name)
    }

    fn worksheet_metadata(&self, name: &str) -> Option<WorksheetMetadata<'_>> {
        Workbook::worksheet_metadata(self, name)
    }

    fn worksheet_metadata_at(&self, index: usize) -> Option<WorksheetMetadata<'_>> {
        Workbook::worksheet_metadata_at(self, index)
    }

    fn worksheets_metadata(&self) -> Vec<WorksheetMetadata<'_>> {
        Workbook::worksheets_metadata(self)
    }

    fn worksheets(&self) -> Vec<(String, Range<'_>)> {
        Workbook::worksheets(self)
    }

    fn metadata(&self) -> WorkbookMetadata<'_> {
        Workbook::metadata(self)
    }

    fn active_sheet_index(&self) -> Option<usize> {
        Workbook::active_sheet_index(self)
    }

    fn active_sheet_name(&self) -> Option<&str> {
        Workbook::active_sheet_name(self)
    }

    fn pictures(&self) -> Option<Vec<(String, Vec<u8>)>> {
        Workbook::pictures(self)
    }

    fn pictures_with_metadata(&self) -> Vec<Picture> {
        Workbook::pictures_with_metadata(self)
    }

    fn table_names(&self) -> Vec<&str> {
        Workbook::table_names(self)
    }

    fn table_names_in_sheet(&self, sheet_name: &str) -> Vec<&str> {
        Workbook::table_names_in_sheet(self, sheet_name)
    }

    fn table_by_name(&self, table_name: &str) -> Option<(&str, &Table)> {
        Workbook::table_by_name(self, table_name)
    }

    fn table_data_by_name(&self, table_name: &str) -> Option<(&str, Range<'_>)> {
        Workbook::table_data_by_name(self, table_name)
    }
}
