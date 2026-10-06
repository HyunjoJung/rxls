//! Bounded worksheet-viewport preparation; independent of print pagination.

use std::error::Error;
use std::fmt;
use std::mem::size_of;

use rxls::{DisplayCell, Sheet, Workbook};

use crate::layout::{
    build_sheet_scene_sparse_viewport, measure_viewport_sparse_automatic_rows,
    viewport_blank_has_visible_paint, viewport_row_is_manual, viewport_text_overflows,
    visit_viewport_baseline_runs, AutomaticRows, MeasuredAxisSlot, SparseCellGeometry,
    SparseSceneGeometry, ViewportBaselineRun, ViewportMeasurementContext,
    ViewportPreparationReport, MAX_WORKSHEET_COLUMN,
};
use crate::{
    ClipGroupNode, Fixed, LimitKind, Rect, RenderError, RenderOptions, RenderRange, RenderReport,
    Scene, SceneNode,
};

/// Additional preparation/query bounds; ordinary rendering limits are unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportLimits {
    /// Default-family string admitted by preparation. Other options are
    /// fixed-size values or shared immutable font-pack handles.
    pub max_options_bytes: u64,
    /// Geometry resolutions, metadata/source visits and reserved column/point work.
    pub max_coordinate_visits: u64,
    /// Total retained row and column runs, including zero-sized hidden runs.
    pub max_axis_runs: u64,
    /// Axis and fixed temporary vector/index capacity bytes; source/options are borrowed.
    pub max_geometry_bytes: u64,
    /// Logical sheet dimension, independently of an ordinary scene canvas limit.
    pub max_logical_dimension_raw: u64,
}

impl Default for ViewportLimits {
    fn default() -> Self {
        Self {
            max_options_bytes: 65_536,
            max_coordinate_visits: 2_000_000,
            max_axis_runs: 65_536,
            max_geometry_bytes: 8 << 20,
            max_logical_dimension_raw: 16_000_000 * 1_024,
        }
    }
}

/// A typed viewport-only rejection; legacy renderer errors remain intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewportError {
    /// An existing range, typography, scene or output guard rejected the work.
    Render(RenderError),
    /// A viewport preparation/query cap was exceeded.
    Limit {
        /// Stable resource identifier.
        resource: &'static str,
        /// Configured inclusive cap.
        limit: u64,
        /// Required count when exactly known.
        actual: u64,
    },
    /// A feature outside the bounded viewport path's supported geometry.
    Unsupported {
        /// Stable, path-free reason.
        reason: &'static str,
    },
}

impl From<RenderError> for ViewportError {
    fn from(error: RenderError) -> Self {
        Self::Render(error)
    }
}

impl fmt::Display for ViewportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Render(error) => write!(f, "{error}"),
            Self::Limit {
                resource,
                limit,
                actual,
            } => {
                write!(
                    f,
                    "viewport {resource} limit exceeded: limit {limit}, required {actual}"
                )
            }
            Self::Unsupported { reason } => write!(f, "viewport feature unsupported: {reason}"),
        }
    }
}

impl Error for ViewportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Render(error) => Some(error),
            _ => None,
        }
    }
}

/// Consecutive source indices having the same retained track size and visibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportAxisRun {
    /// First source index.
    pub first: u32,
    /// Last source index, inclusive.
    pub last: u32,
    /// Sheet-space prefix at the first index.
    pub offset: Fixed,
    /// Size of each track; hidden tracks contribute exactly zero.
    pub size: Fixed,
    /// Prefix after the last index.
    pub end: Fixed,
    /// Whether legacy layout retains this track. A retained physical zero differs
    /// from an omitted hidden track, even though both have zero logical extent.
    pub included: bool,
}

/// Immutable compressed source-axis geometry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewportAxis {
    runs: Vec<ViewportAxisRun>,
    extent: Fixed,
}

impl ViewportAxis {
    /// Read bounded runs without expanding one record per source index.
    pub fn runs(&self) -> &[ViewportAxisRun] {
        &self.runs
    }

    /// Logical dimension; zero for an entirely hidden axis.
    pub fn extent(&self) -> Fixed {
        self.extent
    }

    /// Prefix and size of one prepared source index, including hidden tracks.
    pub fn track(&self, index: u32) -> Result<(Fixed, Fixed), ViewportError> {
        let run = self.run(index)?;
        Ok((
            advance(run.offset, run.size, u64::from(index - run.first))?,
            run.size,
        ))
    }

    /// Source interval intersecting a positive half-open pixel interval.
    /// Zero-sized runs are skipped by prefix search rather than expanded.
    pub fn source_interval(
        &self,
        first: Fixed,
        end: Fixed,
    ) -> Result<Option<(u32, u32)>, ViewportError> {
        if first < Fixed::ZERO || end <= first || end > self.extent {
            return unsupported("invalid_axis_pixel_interval");
        }
        let last_pixel = Fixed::from_raw(
            end.raw()
                .checked_sub(1)
                .ok_or(RenderError::CoordinateOverflow)?,
        );
        Ok(Some((self.source_at(first)?, self.source_at(last_pixel)?)))
    }

    fn run(&self, index: u32) -> Result<&ViewportAxisRun, ViewportError> {
        self.runs
            .get(self.runs.partition_point(|run| run.last < index))
            .filter(|run| run.first <= index)
            .ok_or(ViewportError::Unsupported {
                reason: "source_index_not_prepared",
            })
    }

    fn source_at(&self, pixel: Fixed) -> Result<u32, ViewportError> {
        let run = self
            .runs
            .get(self.runs.partition_point(|run| run.end <= pixel))
            .filter(|run| run.size > Fixed::ZERO && pixel >= run.offset)
            .ok_or(ViewportError::Unsupported {
                reason: "pixel_not_prepared",
            })?;
        let relative = pixel
            .raw()
            .checked_sub(run.offset.raw())
            .ok_or(RenderError::CoordinateOverflow)?;
        let index = u64::from(run.first)
            .checked_add(
                u64::try_from(relative / run.size.raw())
                    .map_err(|_| RenderError::CoordinateOverflow)?,
            )
            .ok_or(RenderError::CoordinateOverflow)?;
        u32::try_from(index).map_err(|_| RenderError::CoordinateOverflow.into())
    }
}

/// Geometry bound by immutable borrows to its exact source and render options.
///
/// Sparse baseline runs and one global automatic measurement avoid source-row
/// expansion. Complete intersecting automatic-height merge row spans are required.
/// Explicit and display Used source selections share the same geometry; tiles
/// materialize only their visible axes and bounded nonlocal text/merge owners.
/// Worker revision/owned-handle storage remains separate.
#[derive(Debug)]
pub struct PreparedViewport<'a> {
    sheet: &'a Sheet,
    sheet_index: usize,
    options: &'a RenderOptions,
    limits: ViewportLimits,
    source_range: RenderRange,
    rows: ViewportAxis,
    columns: ViewportAxis,
    geometry_bytes: u64,
    coordinate_visits: u64,
    automatic_measurement: bool,
    digit_width: Fixed,
    preparation_report: ViewportPreparationReport,
    paint_merges: Vec<PreparedPaintMerge>,
    source_index: PreparedSourceIndex<'a>,
}

impl PreparedViewport<'_> {
    /// Work and diagnostics performed once, separately from each tile's report.
    pub fn preparation_report(&self) -> &ViewportPreparationReport {
        &self.preparation_report
    }
    /// Complete prepared source range.
    pub fn source_range(&self) -> RenderRange {
        self.source_range
    }
    /// Compressed row prefixes and heights.
    pub fn rows(&self) -> &ViewportAxis {
        &self.rows
    }
    /// Compressed logical-order column prefixes and widths.
    pub fn columns(&self) -> &ViewportAxis {
        &self.columns
    }
    /// Accounted retained vector capacity bytes.
    pub fn geometry_bytes(&self) -> u64 {
        self.geometry_bytes
    }
    /// Charged geometry, metadata, candidate and reserved span work during preparation.
    pub fn coordinate_visits(&self) -> u64 {
        self.coordinate_visits
    }
    /// Whether global sparse automatic-height measurement was required.
    pub fn automatic_measurement(&self) -> bool {
        self.automatic_measurement
    }
}

/// One bounded viewport SVG with complete source layout clipped into its rectangle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewportTile {
    /// Actual sheet-space rectangle after clipping to logical extent.
    pub logical_rect: Rect,
    /// Source bounds of the visible tracks; hidden gaps do not expand their slots.
    pub source_range: RenderRange,
    /// Tile-local clipped scene; text geometry was translated, never reflowed.
    pub scene: Scene,
    /// SVG whose internal clip identifiers carry the caller's tile namespace.
    pub svg: String,
    /// Per-tile work/output report; preparation diagnostics are retained separately.
    pub report: RenderReport,
    /// Charged coordinate/source/metadata work for this query, excluding preparation.
    pub coordinate_visits: u64,
    /// Accounted query axis/candidate/index vector capacity, excluding scene output.
    pub geometry_bytes: u64,
    /// Additional positive rows admitted for neighboring border claims.
    pub halo_rows: u64,
    /// Additional positive columns admitted for neighboring border claims.
    pub halo_columns: u64,
    /// Extra halo grid coordinates considered before merge collapse.
    pub halo_cells: u64,
}

/// Prepare one workbook sheet without changing legacy rendering or its limits.
pub fn prepare_viewport<'a>(
    workbook: &'a Workbook,
    sheet_index: usize,
    source_range: RenderRange,
    options: &'a RenderOptions,
    limits: ViewportLimits,
) -> Result<PreparedViewport<'a>, ViewportError> {
    let sheet = workbook
        .sheets
        .get(sheet_index)
        .ok_or(RenderError::SheetIndexOutOfRange {
            requested: sheet_index,
            sheet_count: workbook.sheets.len(),
        })?;
    prepare_sheet_viewport(sheet, sheet_index, source_range, options, limits)
}

/// Prepare explicit source geometry. Partial merges and drawing geometry are
/// typed unsupported rather than rendered with tile-dependent layout.
pub fn prepare_sheet_viewport<'a>(
    sheet: &'a Sheet,
    sheet_index: usize,
    source_range: RenderRange,
    options: &'a RenderOptions,
    limits: ViewportLimits,
) -> Result<PreparedViewport<'a>, ViewportError> {
    let source_range = source_range.validate()?;
    reject_drawings(sheet)?;
    let mut budget = Budget::new(limits);
    let source_index = preflight_source_index(sheet, options, &mut budget)?;
    prepare_sheet_viewport_inner(
        sheet,
        sheet_index,
        source_range,
        options,
        limits,
        None,
        budget,
        source_index,
    )
}

#[allow(clippy::too_many_arguments)]
fn prepare_sheet_viewport_inner<'a>(
    sheet: &'a Sheet,
    sheet_index: usize,
    source_range: RenderRange,
    options: &'a RenderOptions,
    limits: ViewportLimits,
    active_used_merges: Option<&[(u32, u16, u32, u16)]>,
    mut budget: Budget,
    source_index: PreparedSourceIndex<'a>,
) -> Result<PreparedViewport<'a>, ViewportError> {
    let source_range = source_range.validate()?;
    viewport_limit(
        "options_bytes",
        limits.max_options_bytes,
        options.default_font_family.len() as u64,
    )?;
    reject_drawings(sheet)?;
    let mut partial_automatic_merge = false;
    for &(r0, c0, r1, c1) in sheet.merged_ranges() {
        budget.visit()?;
        if active_used_merges.is_some() && (r0 > r1 || c0 > c1) {
            continue;
        }
        let merge = RenderRange::new(r0, c0, r1, c1).validate()?;
        partial_automatic_merge |= intersects_rows(merge, source_range)
            && (r0 < source_range.first_row || r1 > source_range.last_row);
        if let Some(active) = active_used_merges {
            let mut found = false;
            for candidate in active {
                budget.visit()?;
                if *candidate == (r0, c0, r1, c1) {
                    found = true;
                    break;
                }
            }
            if !found {
                continue;
            }
        }
        if intersects_rows(merge, source_range)
            && c0 <= source_range.last_col
            && c1 >= source_range.first_col
            && (r0 < source_range.first_row
                || r1 > source_range.last_row
                || c0 < source_range.first_col
                || c1 > source_range.last_col)
        {
            return unsupported("partial_merge_source_range");
        }
    }
    let mut rows = AxisBuilder::new();
    let mut columns = AxisBuilder::new();
    let mut automatic = false;
    budget.visit()?;
    let mut context = ViewportMeasurementContext::new(sheet, options)?;
    visit_viewport_baseline_runs(
        sheet,
        source_range,
        options,
        &mut context,
        |axis| match axis {
            ViewportBaselineRun::Work(amount) => budget.charge(amount),
            ViewportBaselineRun::Row {
                first,
                last,
                size,
                included,
                manual,
            } => {
                automatic |= included && !manual && options.font_pack.is_some();
                rows.push_span(first, last, size, included, &mut budget)
            }
            ViewportBaselineRun::Column {
                index,
                size,
                included,
            } => columns.push(u32::from(index), size, included, &mut budget),
        },
    )?;
    let mut rows = rows.finish();
    let columns = columns.finish();
    if automatic {
        // All-column measurement sees merges outside painted columns as well.
        // Do not clip one of those constraints to the prepared row envelope.
        if partial_automatic_merge {
            return unsupported("partial_automatic_merge_source_rows");
        }
        render_limit(
            LimitKind::Cells,
            options.limits.max_cells,
            sheet.merged_ranges().len() as u64,
        )?;
        let candidates = collect_prepared_candidates(
            &source_index,
            source_range,
            0,
            MAX_WORKSHEET_COLUMN,
            options,
            &mut budget,
        )?;
        let mut automatic_rows =
            SparseAutomaticRows::new(&rows, options.limits.max_cells, &mut budget);
        measure_viewport_sparse_automatic_rows(
            sheet,
            source_range,
            options,
            &candidates,
            &mut automatic_rows,
            &mut context,
        )?;
        let candidate_bytes = vector_bytes(&candidates)?;
        drop(candidates);
        automatic_rows.budget.release_workspace(candidate_bytes)?;
        // Old/new axis capacities and positive growth points coexist until the
        // complete staged replacement has passed its run/dimension/byte caps.
        let measured_axis = automatic_rows.finish()?;
        budget.release_axis(&rows)?;
        rows = measured_axis;
    }
    let paint_merges = prepare_paint_merges(
        sheet,
        source_range,
        &rows,
        &columns,
        active_used_merges,
        options,
        &mut budget,
        &mut context,
    )?;
    let (digit_width, mut preparation_report) = context.finish(options);
    preparation_report.geometry_bytes = budget.geometry_bytes;
    preparation_report.coordinate_visits = budget.visits;
    preparation_report.source_raw_cells = source_index.raw_cells;
    preparation_report.source_hyperlinks = source_index.hyperlinks;
    preparation_report.source_index_build_peak_bytes = source_index.build_peak_bytes;
    Ok(PreparedViewport {
        sheet,
        sheet_index,
        options,
        limits,
        source_range,
        rows,
        columns,
        geometry_bytes: budget.geometry_bytes,
        coordinate_visits: budget.visits,
        automatic_measurement: automatic,
        digit_width,
        preparation_report,
        paint_merges,
        source_index,
    })
}

/// Render a pixel rectangle using visible compressed-axis slots and complete
/// global merge/text layout. `None` denotes an outside or fully hidden rectangle.
/// Legacy scene bounds apply to admitted tile work, not the source span. Supply
/// a distinct namespace for tile SVGs simultaneously embedded in one document.
pub fn render_viewport_tile(
    prepared: &PreparedViewport<'_>,
    requested: Rect,
    namespace: u64,
) -> Result<Option<ViewportTile>, ViewportError> {
    if requested.x < Fixed::ZERO
        || requested.y < Fixed::ZERO
        || requested.width <= Fixed::ZERO
        || requested.height <= Fixed::ZERO
    {
        return unsupported("invalid_viewport_rectangle");
    }
    let right = requested
        .x
        .checked_add(requested.width)
        .ok_or(RenderError::CoordinateOverflow)?
        .min(prepared.columns.extent);
    let bottom = requested
        .y
        .checked_add(requested.height)
        .ok_or(RenderError::CoordinateOverflow)?
        .min(prepared.rows.extent);
    if right <= requested.x || bottom <= requested.y {
        return Ok(None);
    }
    let logical_rect = Rect {
        x: requested.x,
        y: requested.y,
        width: right
            .checked_sub(requested.x)
            .ok_or(RenderError::CoordinateOverflow)?,
        height: bottom
            .checked_sub(requested.y)
            .ok_or(RenderError::CoordinateOverflow)?,
    };
    let SparseTileBuild {
        build,
        range,
        coordinate_visits,
        geometry_bytes,
        halo_rows,
        halo_columns,
        halo_cells,
    } = sparse_tile_build(prepared, logical_rect)?;
    let translate_x = Fixed::ZERO
        .checked_sub(logical_rect.x)
        .ok_or(RenderError::CoordinateOverflow)?;
    let translate_y = Fixed::ZERO
        .checked_sub(logical_rect.y)
        .ok_or(RenderError::CoordinateOverflow)?;
    let wrapper_nodes = build
        .report
        .scene_nodes
        .checked_add(1)
        .ok_or(RenderError::CoordinateOverflow)?;
    render_limit(
        LimitKind::SceneNodes,
        prepared.options.limits.max_scene_nodes,
        wrapper_nodes,
    )?;
    let mut nodes = Vec::new();
    nodes
        .try_reserve_exact(build.scene.nodes.len())
        .map_err(|_| allocation())?;
    for node in build.scene.nodes {
        // Shared existing translation at scale 1000; no authored-print alignment,
        // semantic retention, page planning or layout policies are applied.
        nodes.push(crate::print::transform_node(
            node,
            translate_x,
            translate_y,
            1_000,
        )?);
    }
    let clip = Rect {
        x: Fixed::ZERO,
        y: Fixed::ZERO,
        width: logical_rect.width,
        height: logical_rect.height,
    };
    let mut wrapped = Vec::new();
    wrapped.try_reserve_exact(1).map_err(|_| allocation())?;
    wrapped.push(SceneNode::ClipGroup(ClipGroupNode { clip, nodes }));
    let scene = Scene {
        title: build.scene.title,
        width: clip.width,
        height: clip.height,
        background: build.scene.background,
        nodes: wrapped,
    };
    let svg = crate::svg::render_scene_svg_with_namespace(
        &scene,
        prepared.options.limits.max_output_bytes,
        namespace,
    )?;
    let mut report = build.report;
    report.scene_nodes = wrapper_nodes;
    report.svg_bytes = svg.len() as u64;
    Ok(Some(ViewportTile {
        logical_rect,
        source_range: range,
        scene,
        svg,
        report,
        coordinate_visits,
        geometry_bytes,
        halo_rows,
        halo_columns,
        halo_cells,
    }))
}

/// Positive changes from immutable compressed baseline geometry. Sorted points
/// keep allocation fallible and allow span sums without expanding source rows.
struct SparseAutomaticRows<'a> {
    baseline: &'a ViewportAxis,
    deltas: Vec<(u32, Fixed)>,
    max_points: u64,
    budget: &'a mut Budget,
}

impl<'a> SparseAutomaticRows<'a> {
    fn new(baseline: &'a ViewportAxis, max_points: u64, budget: &'a mut Budget) -> Self {
        Self {
            baseline,
            deltas: Vec::new(),
            max_points,
            budget,
        }
    }

    fn position(&self, row: u32) -> usize {
        self.deltas.partition_point(|&(index, _)| index < row)
    }

    fn prefix(&mut self, row: u32) -> Result<Fixed, ViewportError> {
        self.budget.visit()?;
        if self
            .baseline
            .runs
            .last()
            .and_then(|run| run.last.checked_add(1))
            == Some(row)
        {
            return Ok(self.baseline.extent);
        }
        self.baseline.track(row).map(|(prefix, _)| prefix)
    }

    fn set_delta(&mut self, row: u32, delta: Fixed) -> Result<(), ViewportError> {
        if delta <= Fixed::ZERO {
            return Ok(());
        }
        self.budget.visit()?;
        let position = self.position(row);
        if let Some(point) = self.deltas.get_mut(position).filter(|point| point.0 == row) {
            point.1 = delta;
            return Ok(());
        }
        viewport_limit(
            "automatic_rows",
            self.max_points,
            self.deltas.len() as u64 + 1,
        )?;
        // Inserting an earlier merge target moves retained points; account work
        // before performing that move. Ordinary maxima arrive in sorted order.
        self.budget.charge((self.deltas.len() - position) as u64)?;
        reserve_workspace(&mut self.deltas, self.max_points, self.budget)?;
        self.deltas.insert(position, (row, delta));
        Ok(())
    }

    fn finish(self) -> Result<ViewportAxis, ViewportError> {
        let mut output = AxisBuilder::new();
        let mut points = self.deltas.iter().peekable();
        for run in &self.baseline.runs {
            self.budget.visit()?;
            let mut cursor = run.first;
            while points.peek().is_some_and(|point| point.0 <= run.last) {
                self.budget.visit()?;
                let &(row, delta) = points.next().ok_or(RenderError::CoordinateOverflow)?;
                if row < cursor || !run.included {
                    return Err(RenderError::CoordinateOverflow.into());
                }
                if cursor < row {
                    output.push_span(cursor, row - 1, run.size, run.included, self.budget)?;
                }
                let size = run
                    .size
                    .checked_add(delta)
                    .ok_or(RenderError::CoordinateOverflow)?;
                output.push(row, size, run.included, self.budget)?;
                cursor = row.checked_add(1).ok_or(RenderError::CoordinateOverflow)?;
            }
            if cursor <= run.last {
                output.push_span(cursor, run.last, run.size, run.included, self.budget)?;
            }
        }
        if points.next().is_some() {
            return Err(RenderError::CoordinateOverflow.into());
        }
        self.budget.release_workspace(vector_bytes(&self.deltas)?)?;
        Ok(output.finish())
    }
}

impl AutomaticRows for SparseAutomaticRows<'_> {
    type Error = ViewportError;
    fn charge(&mut self, count: u64) -> Result<(), ViewportError> {
        self.budget.charge(count)
    }
    fn height(&mut self, row: u32) -> Result<Option<Fixed>, ViewportError> {
        self.budget.visit()?;
        let run = self.baseline.run(row)?;
        if !run.included {
            return Ok(None);
        }
        let position = self.position(row);
        let delta = self
            .deltas
            .get(position)
            .filter(|point| point.0 == row)
            .map_or(Fixed::ZERO, |point| point.1);
        Ok(Some(
            run.size
                .checked_add(delta)
                .ok_or(RenderError::CoordinateOverflow)?,
        ))
    }
    fn first_adjustable(
        &mut self,
        sheet: &Sheet,
        first: u32,
        last: u32,
    ) -> Result<Option<u32>, ViewportError> {
        if first > last {
            return Ok(None);
        }
        if sheet.default_row_height_is_manual() {
            // An automatic explicit cached height can override a manual default.
            for (&row, _) in sheet.row_heights().range(first..=last) {
                self.budget.visit()?;
                if !sheet.row_height_is_manual(row) && self.height(row)?.is_some() {
                    return Ok(Some(row));
                }
            }
            return Ok(None);
        }
        let start = self.baseline.runs.partition_point(|run| run.last < first);
        for run in self.baseline.runs[start..]
            .iter()
            .take_while(|run| run.first <= last)
        {
            self.budget.visit()?;
            if !run.included {
                continue;
            }
            let mut cursor = first.max(run.first);
            let end = last.min(run.last);
            for (&row, _) in sheet.row_heights().range(cursor..=end) {
                self.budget.visit()?;
                // A default gap is automatic; only consecutive explicit manual
                // keys can delay its first adjustable row.
                if cursor < row || !sheet.row_height_is_manual(row) {
                    return Ok(Some(cursor));
                }
                cursor = cursor
                    .checked_add(1)
                    .ok_or(RenderError::CoordinateOverflow)?;
                if cursor > end {
                    break;
                }
            }
            if cursor <= end {
                return Ok(Some(cursor));
            }
        }
        Ok(None)
    }
    fn sum(&mut self, first: u32, last: u32) -> Result<Fixed, ViewportError> {
        let end = last.checked_add(1).ok_or(RenderError::CoordinateOverflow)?;
        let end_prefix = self.prefix(end)?;
        let first_prefix = self.prefix(first)?;
        let mut total = end_prefix
            .checked_sub(first_prefix)
            .ok_or(RenderError::CoordinateOverflow)?;
        let start = self.position(first);
        for &(_, delta) in self.deltas[start..]
            .iter()
            .take_while(|point| point.0 <= last)
        {
            self.budget.visit()?;
            total = total
                .checked_add(delta)
                .ok_or(RenderError::CoordinateOverflow)?;
        }
        Ok(total)
    }
    fn grow_to(&mut self, row: u32, required: Fixed) -> Result<(), ViewportError> {
        let Some(current) = self.height(row)? else {
            return Ok(());
        };
        if current >= required {
            return Ok(());
        }
        self.budget.visit()?;
        let baseline = self.baseline.run(row)?.size;
        self.set_delta(
            row,
            required
                .checked_sub(baseline)
                .ok_or(RenderError::CoordinateOverflow)?,
        )
    }
    fn add_deficit(&mut self, row: u32, deficit: Fixed) -> Result<(), ViewportError> {
        let current = self.height(row)?.ok_or(RenderError::CoordinateOverflow)?;
        self.grow_to(
            row,
            current
                .checked_add(deficit)
                .ok_or(RenderError::CoordinateOverflow)?,
        )
    }
}

fn vector_bytes<T>(values: &Vec<T>) -> Result<u64, ViewportError> {
    u64::try_from(
        values
            .capacity()
            .checked_mul(size_of::<T>())
            .ok_or(RenderError::CoordinateOverflow)?,
    )
    .map_err(|_| RenderError::CoordinateOverflow.into())
}

fn reserve_workspace<T>(
    values: &mut Vec<T>,
    max_items: u64,
    budget: &mut Budget,
) -> Result<(), ViewportError> {
    if values.len() < values.capacity() {
        return Ok(());
    }
    let max = usize::try_from(max_items).unwrap_or(usize::MAX);
    let target = values
        .capacity()
        .checked_mul(2)
        .unwrap_or(usize::MAX)
        .max(8)
        .min(max);
    let additional = target
        .checked_sub(values.capacity())
        .ok_or(RenderError::CoordinateOverflow)?;
    if additional == 0 {
        return allocation_result();
    }
    let before = vector_bytes(values)?;
    let requested = u64::try_from(
        additional
            .checked_mul(size_of::<T>())
            .ok_or(RenderError::CoordinateOverflow)?,
    )
    .map_err(|_| RenderError::CoordinateOverflow)?;
    budget.reserve_workspace(requested)?;
    values
        .try_reserve_exact(additional)
        .map_err(|_| allocation())?;
    let actual = vector_bytes(values)?
        .checked_sub(before)
        .ok_or(RenderError::CoordinateOverflow)?;
    // Allocators may grant more capacity than requested; charge it before use.
    if actual > requested {
        budget.reserve_workspace(actual - requested)?;
    }
    Ok(())
}

fn allocation_result<T>() -> Result<T, ViewportError> {
    Err(allocation())
}

/// Private proof that this exact immutably borrowed source passed the global
/// source/index preflight and its source-owned index constructor has run.
#[derive(Debug)]
struct PreparedSourceIndex<'a> {
    sheet: &'a Sheet,
    raw_cells: u64,
    hyperlinks: u64,
    build_peak_bytes: u64,
}

fn preflight_source_index<'a>(
    sheet: &'a Sheet,
    options: &RenderOptions,
    budget: &mut Budget,
) -> Result<PreparedSourceIndex<'a>, ViewportError> {
    viewport_limit(
        "options_bytes",
        budget.limits.max_options_bytes,
        options.default_font_family.len() as u64,
    )?;
    let mut raw_cells = 0_u64;
    for _ in sheet.cells() {
        budget.visit()?;
        raw_cells = raw_cells
            .checked_add(1)
            .ok_or(RenderError::CoordinateOverflow)?;
        render_limit(LimitKind::Cells, options.limits.max_cells, raw_cells)?;
    }
    let hyperlinks = sheet.hyperlinks().len() as u64;
    let entries = raw_cells
        .checked_add(hyperlinks)
        .ok_or(RenderError::CoordinateOverflow)?;
    render_limit(LimitKind::Cells, options.limits.max_cells, entries)?;
    budget.charge(hyperlinks)?;
    // Sheet uses packed-coordinate/source-index entries for both sorted cell
    // and hyperlink arrays. Keep its possible compaction/shrink peak bounded
    // once before warming, even when a caller previously warmed this cache.
    let build_peak_bytes = entries
        .checked_mul(2 * size_of::<(u64, usize)>() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    budget.reserve_workspace(build_peak_bytes)?;
    budget.visit()?;
    // The range constructor calls display_cell_index() immediately (Sheet's
    // source_indices_in_range call), even when this iterator is never advanced.
    // Retained index storage is source-owned, not retained viewport geometry.
    let _ = sheet.display_cells_in_range(0, 0, 0, 0);
    budget.release_workspace(build_peak_bytes)?;
    Ok(PreparedSourceIndex {
        sheet,
        raw_cells,
        hyperlinks,
        build_peak_bytes,
    })
}

fn collect_prepared_candidates<'a>(
    index: &PreparedSourceIndex<'a>,
    range: RenderRange,
    first_col: u16,
    last_col: u16,
    options: &RenderOptions,
    budget: &mut Budget,
) -> Result<Vec<DisplayCell<'a>>, ViewportError> {
    let mut candidates = Vec::new();
    append_prepared_candidates(
        index,
        range,
        first_col,
        last_col,
        options,
        budget,
        &mut candidates,
    )?;
    Ok(candidates)
}

fn collect_prepared_tile_candidates<'a>(
    index: &PreparedSourceIndex<'a>,
    rows: &[MeasuredAxisSlot<u32>],
    first_col: u16,
    last_col: u16,
    options: &RenderOptions,
    budget: &mut Budget,
) -> Result<Vec<DisplayCell<'a>>, ViewportError> {
    let mut candidates = Vec::new();
    for row in rows {
        // Exact prepared visible rows are sorted and unique. Hidden gaps have
        // no query at all; binary row lookups do not revisit distant records.
        let range = RenderRange::new(row.index, first_col, row.index, last_col);
        append_prepared_candidates(
            index,
            range,
            first_col,
            last_col,
            options,
            budget,
            &mut candidates,
        )?;
    }
    Ok(candidates)
}

#[allow(clippy::too_many_arguments)]
fn append_prepared_candidates<'a>(
    index: &PreparedSourceIndex<'a>,
    range: RenderRange,
    first_col: u16,
    last_col: u16,
    options: &RenderOptions,
    budget: &mut Budget,
    candidates: &mut Vec<DisplayCell<'a>>,
) -> Result<(), ViewportError> {
    let sheet = index.sheet;
    budget.visit()?;
    // The core range API filters columns after traversing the row entries.
    // Ask for all columns, charge each visited entry, then filter locally so
    // unselected columns do not perform invisible, uncharged work.
    for cell in sheet.display_cells_in_range(range.first_row, 0, range.last_row, u16::MAX) {
        budget.visit()?;
        if cell.col < first_col || cell.col > last_col || cell.formatted.is_empty() {
            continue;
        }
        render_limit(
            LimitKind::Cells,
            options.limits.max_cells,
            candidates.len() as u64 + 1,
        )?;
        reserve_workspace(candidates, options.limits.max_cells, budget)?;
        candidates.push(cell);
    }
    Ok(())
}

struct AxisBuilder {
    runs: Vec<ViewportAxisRun>,
    extent: Fixed,
}

impl AxisBuilder {
    fn new() -> Self {
        Self {
            runs: Vec::new(),
            extent: Fixed::ZERO,
        }
    }
    fn push(
        &mut self,
        index: u32,
        size: Fixed,
        included: bool,
        budget: &mut Budget,
    ) -> Result<(), ViewportError> {
        if size < Fixed::ZERO {
            return unsupported("negative_axis_size");
        }
        let end = self
            .extent
            .checked_add(size)
            .ok_or(RenderError::CoordinateOverflow)?;
        viewport_limit(
            "logical_dimension_raw",
            budget.limits.max_logical_dimension_raw,
            u64::try_from(end.raw()).map_err(|_| RenderError::CoordinateOverflow)?,
        )?;
        if let Some(last) = self.runs.last_mut() {
            if last.last.checked_add(1) == Some(index)
                && last.size == size
                && last.included == included
            {
                last.last = index;
                last.end = end;
                self.extent = end;
                return Ok(());
            }
        }
        let new_runs = budget
            .runs
            .checked_add(1)
            .ok_or(RenderError::CoordinateOverflow)?;
        viewport_limit("axis_runs", budget.limits.max_axis_runs, new_runs)?;
        if self.runs.len() == self.runs.capacity() {
            let capacity = self
                .runs
                .capacity()
                .checked_mul(2)
                .unwrap_or(usize::MAX)
                .max(8);
            let max_runs = usize::try_from(budget.limits.max_axis_runs).unwrap_or(usize::MAX);
            let target = capacity.min(max_runs);
            let additional = target
                .checked_sub(self.runs.capacity())
                .ok_or(RenderError::CoordinateOverflow)?;
            let old_capacity = self.runs.capacity();
            let bytes = u64::try_from(
                additional
                    .checked_mul(size_of::<ViewportAxisRun>())
                    .ok_or(RenderError::CoordinateOverflow)?,
            )
            .map_err(|_| RenderError::CoordinateOverflow)?;
            let new_bytes = budget
                .geometry_bytes
                .checked_add(bytes)
                .ok_or(RenderError::CoordinateOverflow)?;
            viewport_limit(
                "geometry_bytes",
                budget.limits.max_geometry_bytes,
                new_bytes,
            )?;
            self.runs
                .try_reserve_exact(additional)
                .map_err(|_| allocation())?;
            let actual_bytes = u64::try_from(
                self.runs
                    .capacity()
                    .checked_sub(old_capacity)
                    .and_then(|count| count.checked_mul(size_of::<ViewportAxisRun>()))
                    .ok_or(RenderError::CoordinateOverflow)?,
            )
            .map_err(|_| RenderError::CoordinateOverflow)?;
            let actual_bytes = budget
                .geometry_bytes
                .checked_add(actual_bytes)
                .ok_or(RenderError::CoordinateOverflow)?;
            viewport_limit(
                "geometry_bytes",
                budget.limits.max_geometry_bytes,
                actual_bytes,
            )?;
            budget.geometry_bytes = actual_bytes;
        }
        self.runs.push(ViewportAxisRun {
            first: index,
            last: index,
            offset: self.extent,
            size,
            end,
            included,
        });
        self.extent = end;
        budget.runs = new_runs;
        Ok(())
    }
    fn push_span(
        &mut self,
        first: u32,
        last: u32,
        size: Fixed,
        included: bool,
        budget: &mut Budget,
    ) -> Result<(), ViewportError> {
        if size < Fixed::ZERO {
            return unsupported("negative_axis_size");
        }
        let count = u64::from(last)
            .checked_sub(u64::from(first))
            .and_then(|count| count.checked_add(1))
            .ok_or(RenderError::CoordinateOverflow)?;
        let end = advance(self.extent, size, count)?;
        viewport_limit(
            "logical_dimension_raw",
            budget.limits.max_logical_dimension_raw,
            u64::try_from(end.raw()).map_err(|_| RenderError::CoordinateOverflow)?,
        )?;
        self.push(first, size, included, budget)?;
        let run = self
            .runs
            .last_mut()
            .ok_or(RenderError::CoordinateOverflow)?;
        run.last = last;
        run.end = end;
        self.extent = end;
        Ok(())
    }
    fn finish(self) -> ViewportAxis {
        ViewportAxis {
            runs: self.runs,
            extent: self.extent,
        }
    }
}

struct Budget {
    limits: ViewportLimits,
    visits: u64,
    runs: u64,
    geometry_bytes: u64,
}

impl Budget {
    fn new(limits: ViewportLimits) -> Self {
        Self {
            limits,
            visits: 0,
            runs: 0,
            geometry_bytes: 0,
        }
    }
    fn visit(&mut self) -> Result<(), ViewportError> {
        self.charge(1)
    }
    fn charge(&mut self, count: u64) -> Result<(), ViewportError> {
        let actual = self
            .visits
            .checked_add(count)
            .ok_or(RenderError::CoordinateOverflow)?;
        viewport_limit(
            "coordinate_visits",
            self.limits.max_coordinate_visits,
            actual,
        )?;
        self.visits = actual;
        Ok(())
    }
    fn reserve_workspace(&mut self, bytes: u64) -> Result<(), ViewportError> {
        let actual = self
            .geometry_bytes
            .checked_add(bytes)
            .ok_or(RenderError::CoordinateOverflow)?;
        viewport_limit("geometry_bytes", self.limits.max_geometry_bytes, actual)?;
        self.geometry_bytes = actual;
        Ok(())
    }
    fn release_workspace(&mut self, bytes: u64) -> Result<(), ViewportError> {
        self.geometry_bytes = self
            .geometry_bytes
            .checked_sub(bytes)
            .ok_or(RenderError::CoordinateOverflow)?;
        Ok(())
    }
    fn release_axis(&mut self, axis: &ViewportAxis) -> Result<(), ViewportError> {
        let bytes = u64::try_from(
            axis.runs
                .capacity()
                .checked_mul(size_of::<ViewportAxisRun>())
                .ok_or(RenderError::CoordinateOverflow)?,
        )
        .map_err(|_| RenderError::CoordinateOverflow)?;
        self.geometry_bytes = self
            .geometry_bytes
            .checked_sub(bytes)
            .ok_or(RenderError::CoordinateOverflow)?;
        self.runs = self
            .runs
            .checked_sub(axis.runs.len() as u64)
            .ok_or(RenderError::CoordinateOverflow)?;
        Ok(())
    }
}

fn advance(offset: Fixed, size: Fixed, count: u64) -> Result<Fixed, ViewportError> {
    let amount = i128::from(size.raw())
        .checked_mul(i128::from(count))
        .ok_or(RenderError::CoordinateOverflow)?;
    let raw = i128::from(offset.raw())
        .checked_add(amount)
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok(Fixed::from_raw(
        i64::try_from(raw).map_err(|_| RenderError::CoordinateOverflow)?,
    ))
}

fn render_limit(kind: LimitKind, limit: u64, actual: u64) -> Result<(), ViewportError> {
    if actual > limit {
        return Err(RenderError::LimitExceeded {
            kind,
            limit,
            actual,
        }
        .into());
    }
    Ok(())
}

fn viewport_limit(resource: &'static str, limit: u64, actual: u64) -> Result<(), ViewportError> {
    if actual > limit {
        return Err(ViewportError::Limit {
            resource,
            limit,
            actual,
        });
    }
    Ok(())
}

fn intersects_rows(left: RenderRange, right: RenderRange) -> bool {
    left.first_row <= right.last_row && left.last_row >= right.first_row
}

fn unsupported<T>(reason: &'static str) -> Result<T, ViewportError> {
    Err(ViewportError::Unsupported { reason })
}

fn allocation() -> ViewportError {
    ViewportError::Unsupported {
        reason: "allocation_failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::synthetic_test_pack;
    use crate::RenderSelection;
    use rxls::CellStyle;

    #[test]
    fn font_backed_nonlocal_wrapped_height_uses_one_exact_full_measurement() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("wrapped");
        sheet.set_col_width(0, 100.0);
        sheet.set_col_width(1, 2.0);
        sheet.write(0, 0, "plain");
        sheet.write_styled(0, 1, "한글中文한글中文", &CellStyle::new().wrap());
        let range = RenderRange::new(0, 0, 0, 1);
        let pack = synthetic_test_pack();
        let options = RenderOptions {
            selection: RenderSelection::Range(range),
            gridlines: false,
            default_font_family: pack.default_family().to_owned(),
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let whole = crate::render_sheet_svg(&workbook, 0, &options).unwrap();
        let prepared =
            prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
        assert!(prepared.automatic_measurement());
        assert_eq!(prepared.rows().extent(), whole.scene.height);
        let tile = render_viewport_tile(
            &prepared,
            Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: prepared.columns().track(0).unwrap().1,
                height: prepared.rows().extent(),
            },
            1,
        )
        .unwrap()
        .unwrap();
        assert_eq!(tile.scene.height, whole.scene.height);
    }

    #[test]
    fn million_row_default_axis_jumps_under_a_constant_work_cap() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("million").set_default_row_height(9.0);
        let range = RenderRange::new(0, 0, 999_999, 0);
        let options = RenderOptions::default();
        let prepared = prepare_viewport(
            &workbook,
            0,
            range,
            &options,
            ViewportLimits {
                max_coordinate_visits: 4,
                ..ViewportLimits::default()
            },
        )
        .unwrap();
        assert_eq!(prepared.coordinate_visits(), 4);
        assert_eq!(prepared.rows().runs().len(), 1);
        assert_eq!(prepared.rows().extent(), Fixed::from_pixels(12_000_000));
        assert_eq!(
            prepared.rows().track(999_999).unwrap(),
            (Fixed::from_pixels(11_999_988), Fixed::from_pixels(12))
        );
    }

    #[test]
    fn global_far_wrapped_row_matches_the_small_dense_row_control() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("large");
        sheet.set_col_width(0, 100.0);
        sheet.set_col_width(1, 2.0);
        sheet.write(0, 0, "plain");
        sheet.write_styled(5_000, 1, "한글中文한글中文", &CellStyle::new().wrap());
        let pack = synthetic_test_pack();
        let options = RenderOptions {
            default_font_family: pack.default_family().to_owned(),
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let range = RenderRange::new(0, 0, 5_000, 1);
        let prepared =
            prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
        let control = crate::layout::measure_sheet_axes_for_ranges(
            &workbook.sheets[0],
            &[RenderRange::new(5_000, 0, 5_000, 1)],
            &options,
        )
        .unwrap();
        assert!(prepared.automatic_measurement());
        assert_eq!(
            prepared.rows().track(5_000).unwrap().1,
            control[0].0[0].size
        );
        assert!(prepared.rows().track(5_000).unwrap().1 > prepared.rows().track(4_999).unwrap().1);
        assert!(prepared.rows().runs().len() <= 3);
        assert!(prepared.coordinate_visits() < 100);
    }

    #[test]
    fn sparse_complete_merge_sums_apply_ordinary_growth_first_and_keep_order() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("adapter");
        let sheet = &workbook.sheets[0];
        let mut budget = Budget::new(ViewportLimits::default());
        let mut builder = AxisBuilder::new();
        builder
            .push_span(0, 3, Fixed::from_pixels(10), true, &mut budget)
            .unwrap();
        let baseline = builder.finish();
        let mut rows = SparseAutomaticRows::new(&baseline, 10, &mut budget);
        rows.grow_to(3, Fixed::from_pixels(30)).unwrap();
        assert_eq!(rows.sum(0, 3).unwrap(), Fixed::from_pixels(60));
        assert_eq!(rows.first_adjustable(sheet, 0, 3).unwrap(), Some(0));
        let result = rows.finish().unwrap();
        assert_eq!(result.track(0).unwrap().1, Fixed::from_pixels(10));
        assert_eq!(result.track(3).unwrap().1, Fixed::from_pixels(30));

        let mut budget = Budget::new(ViewportLimits::default());
        let mut builder = AxisBuilder::new();
        builder
            .push_span(0, 2, Fixed::from_pixels(10), true, &mut budget)
            .unwrap();
        let baseline = builder.finish();
        let mut rows = SparseAutomaticRows::new(&baseline, 10, &mut budget);
        let first = rows.first_adjustable(sheet, 0, 1).unwrap().unwrap();
        rows.add_deficit(first, Fixed::from_pixels(10)).unwrap();
        assert_eq!(rows.sum(1, 2).unwrap(), Fixed::from_pixels(20));
        let second = rows.first_adjustable(sheet, 1, 2).unwrap().unwrap();
        rows.add_deficit(second, Fixed::from_pixels(30)).unwrap();
        let result = rows.finish().unwrap();
        assert_eq!(
            (0..=2)
                .map(|row| result.track(row).unwrap().1)
                .collect::<Vec<_>>(),
            vec![
                Fixed::from_pixels(20),
                Fixed::from_pixels(40),
                Fixed::from_pixels(10)
            ]
        );
    }

    #[test]
    fn sparse_first_adjustable_skips_hidden_runs_and_consecutive_manual_keys() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("selection");
        sheet.set_row_height(0, 9.0);
        sheet.set_row_height(1, 9.0);
        let mut budget = Budget::new(ViewportLimits::default());
        let mut builder = AxisBuilder::new();
        builder
            .push_span(0, 2, Fixed::from_pixels(12), true, &mut budget)
            .unwrap();
        builder
            .push_span(3, 699_999, Fixed::ZERO, false, &mut budget)
            .unwrap();
        builder
            .push(700_000, Fixed::from_pixels(12), true, &mut budget)
            .unwrap();
        let baseline = builder.finish();
        let mut rows = SparseAutomaticRows::new(&baseline, 10, &mut budget);
        assert_eq!(rows.first_adjustable(sheet, 0, 700_000).unwrap(), Some(2));
        assert_eq!(
            rows.first_adjustable(sheet, 3, 700_000).unwrap(),
            Some(700_000)
        );
        assert_eq!(rows.sum(3, 700_000).unwrap(), Fixed::from_pixels(12));
        assert!(rows.budget.visits < 20);
    }

    #[test]
    fn complete_million_row_constraint_adds_only_one_sparse_growth_point() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("million");
        let mut budget = Budget::new(ViewportLimits::default());
        let mut builder = AxisBuilder::new();
        builder
            .push_span(0, 999_999, Fixed::from_pixels(12), true, &mut budget)
            .unwrap();
        let baseline = builder.finish();
        let mut rows = SparseAutomaticRows::new(&baseline, 10, &mut budget);
        assert_eq!(
            rows.sum(0, 999_999).unwrap(),
            Fixed::from_pixels(12_000_000)
        );
        rows.add_deficit(500_000, Fixed::from_pixels(10)).unwrap();
        assert_eq!(
            rows.sum(0, 999_999).unwrap(),
            Fixed::from_pixels(12_000_010)
        );
        let result = rows.finish().unwrap();
        assert_eq!(result.runs().len(), 3);
        assert_eq!(result.track(500_000).unwrap().1, Fixed::from_pixels(22));
        assert_eq!(result.extent(), Fixed::from_pixels(12_000_010));
        assert!(budget.visits < 20);
    }

    #[test]
    fn global_text_budget_covers_far_rows_and_source_index_preflight_is_bounded() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("budget");
        sheet.write(0, 0, "abc");
        sheet.write(5_000, 0, "def");
        let pack = synthetic_test_pack();
        let mut options = RenderOptions {
            default_font_family: pack.default_family().to_owned(),
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        options.limits.max_text_bytes = 3;
        let range = RenderRange::new(0, 0, 5_000, 0);
        assert!(matches!(
            prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()),
            Err(ViewportError::Render(RenderError::LimitExceeded {
                kind: LimitKind::TextBytes,
                ..
            }))
        ));
        options.limits.max_cells = 1;
        assert!(matches!(
            prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()),
            Err(ViewportError::Render(RenderError::LimitExceeded {
                kind: LimitKind::Cells,
                ..
            }))
        ));
    }

    #[test]
    fn global_complete_giant_merge_prepares_and_sparse_tile_keeps_its_box() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("giant");
        sheet.set_col_width(0, 2.0);
        sheet.merge(0, 0, 100_000, 0);
        sheet.write_styled(0, 0, "merged words", &CellStyle::new().wrap());
        let pack = synthetic_test_pack();
        let options = RenderOptions {
            default_font_family: pack.default_family().to_owned(),
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let prepared = prepare_viewport(
            &workbook,
            0,
            RenderRange::new(0, 0, 100_000, 0),
            &options,
            ViewportLimits::default(),
        )
        .unwrap();
        assert!(prepared.automatic_measurement());
        assert!(prepared.coordinate_visits() < 100);
        assert_eq!(prepared.rows().runs().len(), 1);
        let tile = render_viewport_tile(
            &prepared,
            Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_pixels(10),
                height: Fixed::from_pixels(10),
            },
            0,
        )
        .unwrap()
        .unwrap();
        assert_eq!(tile.report.merged_regions, 1);
        assert_eq!(tile.report.visible_rows, 1);
        assert_eq!(prepared.paint_merges[0].rect.height, prepared.rows.extent);
    }

    #[test]
    fn nonlocal_merge_is_measured_completely_or_rejected_before_row_clipping() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("nonlocal-merge");
        sheet.set_col_width(5, 2.0);
        sheet.merge(0, 5, 4, 5);
        sheet.write_styled(
            0,
            5,
            "wide words wide words wide words wide words",
            &CellStyle::new().wrap(),
        );
        let pack = synthetic_test_pack();
        let options = RenderOptions {
            default_font_family: pack.default_family().to_owned(),
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let prepared = prepare_viewport(
            &workbook,
            0,
            RenderRange::new(0, 0, 4, 0),
            &options,
            ViewportLimits::default(),
        )
        .unwrap();
        let control = crate::layout::measure_sheet_axes_for_ranges(
            &workbook.sheets[0],
            &[RenderRange::new(0, 0, 4, 0)],
            &options,
        )
        .unwrap();
        for row in 0..=4 {
            assert_eq!(
                prepared.rows().track(row).unwrap().1,
                control[0].0[row as usize].size
            );
        }
        assert!(matches!(
            prepare_viewport(
                &workbook,
                0,
                RenderRange::new(1, 0, 4, 0),
                &options,
                ViewportLimits::default()
            ),
            Err(ViewportError::Unsupported {
                reason: "partial_automatic_merge_source_rows"
            })
        ));
    }

    #[test]
    fn imported_manual_default_selects_only_the_visible_automatic_cached_exception() {
        use std::io::Write as _;
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (path, xml) in [
            (
                "xl/workbook.xml",
                r#"<workbook><sheets><sheet name="manual-default" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetFormatPr defaultRowHeight="9" customHeight="1" zeroHeight="1"/><sheetData><row r="13" ht="9" customHeight="1"><c r="A13"><v>1</v></c></row><row r="700001" ht="9" customHeight="0"><c r="A700001"><v>2</v></c></row></sheetData></worksheet>"#,
            ),
        ] {
            writer
                .start_file(path, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(xml.as_bytes()).unwrap();
        }
        let workbook = Workbook::open(&writer.finish().unwrap().into_inner()).unwrap();
        let sheet = &workbook.sheets[0];
        assert!(sheet.default_row_height_is_manual());
        assert!(sheet.row_height_is_manual(12));
        assert!(!sheet.row_height_is_manual(700_000));
        let options = RenderOptions::default();
        let prepared = prepare_viewport(
            &workbook,
            0,
            RenderRange::new(0, 0, 700_000, 0),
            &options,
            ViewportLimits {
                max_coordinate_visits: 20,
                ..ViewportLimits::default()
            },
        )
        .unwrap();
        assert_eq!(prepared.rows().extent(), Fixed::from_pixels(24));
        let mut budget = Budget::new(ViewportLimits::default());
        let mut rows = SparseAutomaticRows::new(prepared.rows(), 10, &mut budget);
        assert_eq!(
            rows.first_adjustable(sheet, 0, 700_000).unwrap(),
            Some(700_000)
        );
        rows.add_deficit(700_000, Fixed::from_pixels(6)).unwrap();
        assert_eq!(rows.sum(0, 700_000).unwrap(), Fixed::from_pixels(30));
        let result = rows.finish().unwrap();
        assert_eq!(result.track(12).unwrap().1, Fixed::from_pixels(12));
        assert_eq!(result.track(700_000).unwrap().1, Fixed::from_pixels(18));
        assert_eq!(result.track(100_000).unwrap().1, Fixed::ZERO);
        assert!(budget.visits < 20);
    }
}

#[derive(Debug, Clone, Copy)]
struct PreparedPaintMerge {
    range: RenderRange,
    rect: Rect,
    adjustable: bool,
}

/// Prepare display Used geometry without trusting an authored dimension.
/// Empty Used selections return None. Drawing geometry is typed unsupported.
pub fn prepare_used_viewport<'a>(
    workbook: &'a Workbook,
    sheet_index: usize,
    options: &'a RenderOptions,
    limits: ViewportLimits,
) -> Result<Option<PreparedViewport<'a>>, ViewportError> {
    let sheet = workbook
        .sheets
        .get(sheet_index)
        .ok_or(RenderError::SheetIndexOutOfRange {
            requested: sheet_index,
            sheet_count: workbook.sheets.len(),
        })?;
    prepare_sheet_used_viewport(sheet, sheet_index, options, limits)
}

/// Prepare one sheet's sparse display Used selection; no authored dimension is used.
pub fn prepare_sheet_used_viewport<'a>(
    sheet: &'a Sheet,
    sheet_index: usize,
    options: &'a RenderOptions,
    limits: ViewportLimits,
) -> Result<Option<PreparedViewport<'a>>, ViewportError> {
    reject_drawings(sheet)?;
    let mut budget = Budget::new(limits);
    let source_index = preflight_source_index(sheet, options, &mut budget)?;
    let mut retained = Vec::<(u32, u16)>::new();
    let mut range = None;
    // The shared proof already bounds/warmed this exact source. This second
    // raw traversal preserves Used's overwritten-record coordinate semantics.
    let mut count = source_index.raw_cells;
    for (row, col, _) in sheet.cells() {
        budget.visit()?;
        reserve_workspace(&mut retained, options.limits.max_cells, &mut budget)?;
        retained.push((row, col));
        include_used_coordinate(&mut range, row, col);
    }
    budget.charge(sheet.blank_cell_styles().len() as u64)?;
    count = count
        .checked_add(sheet.blank_cell_styles().len() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    render_limit(LimitKind::Cells, options.limits.max_cells, count)?;
    for &(row, col) in sheet.blank_cell_styles().keys() {
        budget.visit()?;
        if viewport_blank_has_visible_paint(sheet, row, col) {
            reserve_workspace(&mut retained, options.limits.max_cells, &mut budget)?;
            retained.push((row, col));
            include_used_coordinate(&mut range, row, col);
        }
    }
    // Like the source-owned display index, sort/compaction are bounded by the
    // raw-entry ceiling; coordinate visits count entries, not comparator calls.
    budget.charge(retained.len() as u64)?;
    retained.sort_unstable();
    budget.charge(retained.len() as u64)?;
    retained.dedup();
    render_limit(
        LimitKind::Cells,
        options.limits.max_cells,
        sheet.merged_ranges().len() as u64,
    )?;
    let mut active = Vec::new();
    for &(r0, c0, r1, c1) in sheet.merged_ranges() {
        budget.visit()?;
        if r0 > r1 || c0 > c1 {
            continue;
        }
        let mut intersects = false;
        let first = retained.partition_point(|&(row, _)| row < r0);
        for &(row, col) in &retained[first..] {
            budget.visit()?;
            if row > r1 {
                break;
            }
            if col >= c0 && col <= c1 {
                intersects = true;
                break;
            }
        }
        if intersects {
            include_used_coordinate(&mut range, r0, c0);
            include_used_coordinate(&mut range, r1, c1);
            reserve_workspace(&mut active, options.limits.max_cells, &mut budget)?;
            active.push((r0, c0, r1, c1));
        }
    }
    budget.release_workspace(vector_bytes(&retained)?)?;
    drop(retained);
    let Some(range) = range else {
        return Ok(None);
    };
    let active_bytes = vector_bytes(&active)?;
    let mut prepared = prepare_sheet_viewport_inner(
        sheet,
        sheet_index,
        range,
        options,
        limits,
        Some(&active),
        budget,
        source_index,
    )?;
    prepared.geometry_bytes = prepared
        .geometry_bytes
        .checked_sub(active_bytes)
        .ok_or(RenderError::CoordinateOverflow)?;
    prepared.preparation_report.geometry_bytes = prepared.geometry_bytes;
    Ok(Some(prepared))
}

fn include_used_coordinate(range: &mut Option<RenderRange>, row: u32, col: u16) {
    if let Some(range) = range {
        range.first_row = range.first_row.min(row);
        range.last_row = range.last_row.max(row);
        range.first_col = range.first_col.min(col);
        range.last_col = range.last_col.max(col);
    } else {
        *range = Some(RenderRange::new(row, col, row, col));
    }
}

fn reject_drawings(sheet: &Sheet) -> Result<(), ViewportError> {
    if !sheet.images().is_empty()
        || !sheet.charts().is_empty()
        || !sheet.sparklines().is_empty()
        || !sheet.drawing_metadata().is_empty()
    {
        return unsupported("drawing_geometry_not_prepared");
    }
    Ok(())
}

fn axis_after(axis: &ViewportAxis, last: u32) -> Result<Fixed, ViewportError> {
    let (offset, size) = axis.track(last)?;
    offset
        .checked_add(size)
        .ok_or(RenderError::CoordinateOverflow.into())
}

fn axis_span(axis: &ViewportAxis, first: u32, last: u32) -> Result<Fixed, ViewportError> {
    axis_after(axis, last)?
        .checked_sub(axis.track(first)?.0)
        .ok_or(RenderError::CoordinateOverflow.into())
}

fn source_rect(
    rows: &ViewportAxis,
    columns: &ViewportAxis,
    range: RenderRange,
    rtl: bool,
) -> Result<Rect, ViewportError> {
    let width = axis_span(
        columns,
        u32::from(range.first_col),
        u32::from(range.last_col),
    )?;
    let logical_x = columns.track(u32::from(range.first_col))?.0;
    let x = if rtl {
        columns
            .extent
            .checked_sub(logical_x)
            .and_then(|end| end.checked_sub(width))
            .ok_or(RenderError::CoordinateOverflow)?
    } else {
        logical_x
    };
    Ok(Rect {
        x,
        y: rows.track(range.first_row)?.0,
        width,
        height: axis_span(rows, range.first_row, range.last_row)?,
    })
}

#[allow(clippy::too_many_arguments)]
fn prepare_paint_merges(
    sheet: &Sheet,
    source: RenderRange,
    rows: &ViewportAxis,
    columns: &ViewportAxis,
    active: Option<&[(u32, u16, u32, u16)]>,
    options: &RenderOptions,
    budget: &mut Budget,
    context: &mut ViewportMeasurementContext,
) -> Result<Vec<PreparedPaintMerge>, ViewportError> {
    render_limit(
        LimitKind::Cells,
        options.limits.max_cells,
        sheet.merged_ranges().len() as u64,
    )?;
    let mut accepted = Vec::<PreparedPaintMerge>::new();
    for &raw in sheet.merged_ranges() {
        let (r0, c0, r1, c1) = raw;
        budget.visit()?;
        if let Some(active) = active {
            let mut found = false;
            for candidate in active {
                budget.visit()?;
                if *candidate == raw {
                    found = true;
                    break;
                }
            }
            if !found {
                continue;
            }
        }
        if r0 > r1
            || c0 > c1
            || r0 < source.first_row
            || r1 > source.last_row
            || c0 < source.first_col
            || c1 > source.last_col
        {
            continue;
        }
        let range = RenderRange::new(r0, c0, r1, c1);
        let rect = source_rect(rows, columns, range, sheet.sheet_view().right_to_left)?;
        if rect.width <= Fixed::ZERO || rect.height <= Fixed::ZERO {
            continue;
        }
        let mut overlap = false;
        for previous in &accepted {
            budget.visit()?;
            if positive_intersection(previous.rect, rect)? {
                overlap = true;
                break;
            }
        }
        if overlap {
            context.merge_warning(crate::WarningCode::MergeSkipped, r0, c0);
            continue;
        }
        // Reuse the same compressed/manual-row query as global automatic height.
        let mut adapter = SparseAutomaticRows::new(rows, options.limits.max_cells, budget);
        let adjustable = adapter.first_adjustable(sheet, r0, r1)?.is_some();
        reserve_workspace(&mut accepted, options.limits.max_cells, budget)?;
        accepted.push(PreparedPaintMerge {
            range,
            rect,
            adjustable,
        });
    }
    Ok(accepted)
}

fn positive_intersection(left: Rect, right: Rect) -> Result<bool, ViewportError> {
    Ok(left.x
        < right
            .x
            .checked_add(right.width)
            .ok_or(RenderError::CoordinateOverflow)?
        && right.x
            < left
                .x
                .checked_add(left.width)
                .ok_or(RenderError::CoordinateOverflow)?
        && left.y
            < right
                .y
                .checked_add(right.height)
                .ok_or(RenderError::CoordinateOverflow)?
        && right.y
            < left
                .y
                .checked_add(left.height)
                .ok_or(RenderError::CoordinateOverflow)?)
}

fn materialize_window<I: TryFrom<u32>>(
    axis: &ViewportAxis,
    first: Fixed,
    end: Fixed,
    max_items: u64,
    budget: &mut Budget,
) -> Result<Vec<MeasuredAxisSlot<I>>, ViewportError> {
    let mut output = Vec::new();
    let Some((first_index, last_index)) = axis.source_interval(first, end)? else {
        return Ok(output);
    };
    let start = axis.runs.partition_point(|run| run.end < first);
    for run in &axis.runs[start..] {
        budget.visit()?;
        if run.offset > end {
            break;
        }
        if !run.included {
            continue;
        }
        let (source_first, source_last) = if run.size == Fixed::ZERO {
            if run.offset < first || run.offset > end {
                continue;
            }
            // Physical zero tracks retain their border/text claims. Omitted
            // hidden tracks remain run jumps; an excessive retained-zero run
            // fails the unchanged admitted-slot cap before source expansion.
            (run.first, run.last)
        } else {
            if run.end <= first || run.offset >= end {
                continue;
            }
            (run.first.max(first_index), run.last.min(last_index))
        };
        let count = u64::from(source_last) - u64::from(source_first) + 1;
        viewport_limit("visible_axis_slots", max_items, output.len() as u64 + count)?;
        budget.charge(count)?;
        for index in source_first..=source_last {
            reserve_workspace(&mut output, max_items, budget)?;
            let (offset, size) = axis.track(index)?;
            output.push(MeasuredAxisSlot {
                index: I::try_from(index).map_err(|_| RenderError::CoordinateOverflow)?,
                offset,
                size,
            });
        }
    }
    Ok(output)
}

fn merge_at<'a>(
    prepared: &'a PreparedViewport<'_>,
    row: u32,
    col: u16,
    budget: &mut Budget,
) -> Result<Option<&'a PreparedPaintMerge>, ViewportError> {
    for merge in &prepared.paint_merges {
        budget.visit()?;
        if row >= merge.range.first_row
            && row <= merge.range.last_row
            && col >= merge.range.first_col
            && col <= merge.range.last_col
        {
            return Ok(Some(merge));
        }
    }
    Ok(None)
}

fn push_geometry(
    values: &mut Vec<SparseCellGeometry>,
    value: SparseCellGeometry,
    options: &RenderOptions,
    budget: &mut Budget,
) -> Result<(), ViewportError> {
    budget.visit()?;
    render_limit(
        LimitKind::Cells,
        options.limits.max_cells,
        values.len() as u64 + 1,
    )?;
    reserve_workspace(values, options.limits.max_cells, budget)?;
    values.push(value);
    Ok(())
}

fn ordinary_geometry(
    prepared: &PreparedViewport<'_>,
    row: u32,
    col: u16,
) -> Result<SparseCellGeometry, ViewportError> {
    let rect = source_rect(
        &prepared.rows,
        &prepared.columns,
        RenderRange::new(row, col, row, col),
        prepared.sheet.sheet_view().right_to_left,
    )?;
    Ok(SparseCellGeometry {
        source: crate::CellCoordinate { row, col },
        rect,
        is_merged: false,
        has_adjustable_row: !viewport_row_is_manual(prepared.sheet, row),
        first_column: col,
        last_column: col,
        overflow_left: rect.x,
        overflow_right: rect
            .x
            .checked_add(rect.width)
            .ok_or(RenderError::CoordinateOverflow)?,
        anchor_outside: false,
    })
}

// A bounded sweep supplies global blockers without materializing blank gaps.
fn set_spill_bounds(
    prepared: &PreparedViewport<'_>,
    geometry: &mut SparseCellGeometry,
    cells: &[DisplayCell<'_>],
    budget: &mut Budget,
) -> Result<(), ViewportError> {
    if geometry.is_merged {
        return Ok(());
    }
    let row = geometry.source.row;
    let mut left = Fixed::ZERO;
    let mut right = prepared.columns.extent;
    let owner_left = geometry.rect.x;
    let owner_right = owner_left
        .checked_add(geometry.rect.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let start = cells.partition_point(|cell| cell.row < row);
    for cell in &cells[start..] {
        budget.visit()?;
        if cell.row != row {
            break;
        }
        if cell.col == geometry.source.col
            || cell.formatted.is_empty()
            || !prepared.columns.run(u32::from(cell.col))?.included
        {
            continue;
        }
        let rect = ordinary_geometry(prepared, row, cell.col)?.rect;
        let end = rect
            .x
            .checked_add(rect.width)
            .ok_or(RenderError::CoordinateOverflow)?;
        let physical_left = if prepared.sheet.sheet_view().right_to_left {
            cell.col > geometry.source.col
        } else {
            cell.col < geometry.source.col
        };
        let physical_right = if prepared.sheet.sheet_view().right_to_left {
            cell.col < geometry.source.col
        } else {
            cell.col > geometry.source.col
        };
        if end < owner_left || (end == owner_left && (rect.width > Fixed::ZERO || physical_left)) {
            left = left.max(end);
        }
        if rect.x > owner_right
            || (rect.x == owner_right && (rect.width > Fixed::ZERO || physical_right))
        {
            right = right.min(rect.x);
        }
    }
    for merge in &prepared.paint_merges {
        budget.visit()?;
        if row < merge.range.first_row || row > merge.range.last_row {
            continue;
        }
        let end = merge
            .rect
            .x
            .checked_add(merge.rect.width)
            .ok_or(RenderError::CoordinateOverflow)?;
        if end <= owner_left {
            left = left.max(end);
        }
        if merge.rect.x >= owner_right {
            right = right.min(merge.rect.x);
        }
    }
    geometry.overflow_left = left;
    geometry.overflow_right = right;
    Ok(())
}

struct SparseTileBuild {
    build: crate::SceneBuild,
    range: RenderRange,
    coordinate_visits: u64,
    geometry_bytes: u64,
    halo_rows: u64,
    halo_columns: u64,
    halo_cells: u64,
}

fn halo_interval(first: Fixed, end: Fixed, extent: Fixed) -> Result<(Fixed, Fixed), ViewportError> {
    let margin = crate::layout::viewport_border_paint_outset().raw();
    Ok((
        Fixed::from_raw(first.raw().saturating_sub(margin).max(0)),
        Fixed::from_raw(
            end.raw()
                .checked_add(margin)
                .ok_or(RenderError::CoordinateOverflow)?,
        )
        .min(extent),
    ))
}

fn sparse_tile_build(
    prepared: &PreparedViewport<'_>,
    window: Rect,
) -> Result<SparseTileBuild, ViewportError> {
    let options = prepared.options;
    let mut budget = Budget::new(prepared.limits);
    let right = window
        .x
        .checked_add(window.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let bottom = window
        .y
        .checked_add(window.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let rows = materialize_window::<u32>(
        &prepared.rows,
        window.y,
        bottom,
        options.limits.max_rows,
        &mut budget,
    )?;
    let rtl = prepared.sheet.sheet_view().right_to_left;
    let (first_x, end_x) = if rtl {
        (
            prepared
                .columns
                .extent
                .checked_sub(right)
                .ok_or(RenderError::CoordinateOverflow)?,
            prepared
                .columns
                .extent
                .checked_sub(window.x)
                .ok_or(RenderError::CoordinateOverflow)?,
        )
    } else {
        (window.x, right)
    };
    let mut columns = materialize_window::<u16>(
        &prepared.columns,
        first_x,
        end_x,
        options.limits.max_columns,
        &mut budget,
    )?;
    let first_row = rows.first().ok_or(RenderError::CoordinateOverflow)?.index;
    let last_row = rows.last().ok_or(RenderError::CoordinateOverflow)?.index;
    let first_col = columns
        .first()
        .ok_or(RenderError::CoordinateOverflow)?
        .index;
    let last_col = columns.last().ok_or(RenderError::CoordinateOverflow)?.index;
    let range = RenderRange::new(first_row, first_col, last_row, last_col);
    render_limit(
        LimitKind::Cells,
        options.limits.max_cells,
        (rows.len() as u64)
            .checked_mul(columns.len() as u64)
            .ok_or(RenderError::CoordinateOverflow)?,
    )?;
    // Expand by the shared maximum border paint outset, not a track count.
    // Multiple narrow positive tracks may fit inside this bounded margin;
    // prefix queries still jump omitted zero gaps and preserve the final clip.
    let (halo_y, halo_bottom) = halo_interval(window.y, bottom, prepared.rows.extent)?;
    let (halo_x, halo_right) = halo_interval(first_x, end_x, prepared.columns.extent)?;
    let halo_row_slots = materialize_window::<u32>(
        &prepared.rows,
        halo_y,
        halo_bottom,
        options.limits.max_rows,
        &mut budget,
    )?;
    let halo_column_slots = materialize_window::<u16>(
        &prepared.columns,
        halo_x,
        halo_right,
        options.limits.max_columns,
        &mut budget,
    )?;
    let halo_product = (halo_row_slots.len() as u64)
        .checked_mul(halo_column_slots.len() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    render_limit(LimitKind::Cells, options.limits.max_cells, halo_product)?;
    let halo_first_col = halo_column_slots
        .first()
        .ok_or(RenderError::CoordinateOverflow)?
        .index;
    let halo_last_col = halo_column_slots
        .last()
        .ok_or(RenderError::CoordinateOverflow)?
        .index;
    let halo_range = RenderRange::new(
        halo_row_slots
            .first()
            .ok_or(RenderError::CoordinateOverflow)?
            .index,
        halo_first_col,
        halo_row_slots
            .last()
            .ok_or(RenderError::CoordinateOverflow)?
            .index,
        halo_last_col,
    );
    let paint_bounds = source_rect(&prepared.rows, &prepared.columns, halo_range, rtl)?;
    if rtl {
        for column in &mut columns {
            budget.visit()?;
            column.offset = prepared
                .columns
                .extent
                .checked_sub(column.offset)
                .and_then(|end| end.checked_sub(column.size))
                .ok_or(RenderError::CoordinateOverflow)?;
        }
        columns.reverse();
    }
    // Query the prepared source index without a global raw scan or build peak.
    let candidates = collect_prepared_tile_candidates(
        &prepared.source_index,
        &rows,
        prepared.source_range.first_col,
        prepared.source_range.last_col,
        options,
        &mut budget,
    )?;
    let mut geometry = Vec::new();
    for row in &halo_row_slots {
        for column in &halo_column_slots {
            budget.visit()?;
            if merge_at(prepared, row.index, column.index, &mut budget)?.is_some() {
                continue;
            }
            push_geometry(
                &mut geometry,
                ordinary_geometry(prepared, row.index, column.index)?,
                options,
                &mut budget,
            )?;
        }
    }
    for merge in &prepared.paint_merges {
        budget.visit()?;
        if !positive_intersection(merge.rect, paint_bounds)? {
            continue;
        }
        let anchor_rect = source_rect(
            &prepared.rows,
            &prepared.columns,
            RenderRange::new(
                merge.range.first_row,
                merge.range.first_col,
                merge.range.first_row,
                merge.range.first_col,
            ),
            rtl,
        )?;
        push_geometry(
            &mut geometry,
            SparseCellGeometry {
                source: crate::CellCoordinate {
                    row: merge.range.first_row,
                    col: merge.range.first_col,
                },
                rect: merge.rect,
                is_merged: true,
                has_adjustable_row: merge.adjustable,
                first_column: merge.range.first_col,
                last_column: merge.range.last_col,
                overflow_left: merge.rect.x,
                overflow_right: merge
                    .rect
                    .x
                    .checked_add(merge.rect.width)
                    .ok_or(RenderError::CoordinateOverflow)?,
                anchor_outside: !positive_intersection(anchor_rect, window)?,
            },
            options,
            &mut budget,
        )?;
    }
    for cell in &candidates {
        budget.visit()?;
        if !viewport_text_overflows(cell.value)
            || !prepared.rows.run(cell.row)?.included
            || !prepared.columns.run(u32::from(cell.col))?.included
            || merge_at(prepared, cell.row, cell.col, &mut budget)?.is_some()
        {
            continue;
        }
        // Candidates already present in the visible grid do not need a second owner.
        if cell.col >= halo_first_col && cell.col <= halo_last_col {
            continue;
        }
        let mut owner = ordinary_geometry(prepared, cell.row, cell.col)?;
        if owner.rect.height <= Fixed::ZERO {
            continue;
        }
        set_spill_bounds(prepared, &mut owner, &candidates, &mut budget)?;
        let spill = Rect {
            x: owner.overflow_left,
            y: owner.rect.y,
            width: owner
                .overflow_right
                .checked_sub(owner.overflow_left)
                .ok_or(RenderError::CoordinateOverflow)?,
            height: owner.rect.height,
        };
        if positive_intersection(spill, window)? {
            push_geometry(&mut geometry, owner, options, &mut budget)?;
        }
    }
    for owner in &mut geometry {
        set_spill_bounds(prepared, owner, &candidates, &mut budget)?;
    }
    // Sorted lookup supports conditional style deciding the actual spill direction later.
    let count = geometry.len() as u64;
    budget.charge(
        count
            .checked_mul(u64::from(usize::BITS - geometry.len().leading_zeros()))
            .ok_or(RenderError::CoordinateOverflow)?,
    )?;
    // A second bounded ordering in the constructor puts zero-width claims
    // before positive regions sharing the same visual origin (also for RTL).
    budget.charge(
        count
            .checked_mul(u64::from(usize::BITS - geometry.len().leading_zeros()))
            .ok_or(RenderError::CoordinateOverflow)?,
    )?;
    geometry.sort_unstable_by_key(|cell| cell.source);
    budget.charge(count)?;
    if geometry
        .windows(2)
        .any(|pair| pair[0].source == pair[1].source)
    {
        return unsupported("duplicate_sparse_scene_source");
    }
    budget.charge(count)?; // point display/style lookups performed by the shared constructor
    let complete_bounds = Rect {
        x: Fixed::ZERO,
        y: Fixed::ZERO,
        width: prepared.columns.extent,
        height: prepared.rows.extent,
    };
    let build = build_sheet_scene_sparse_viewport(
        prepared.sheet,
        prepared.sheet_index,
        options,
        SparseSceneGeometry {
            rows: &rows,
            columns: &columns,
            cells: &geometry,
            window,
            complete_bounds,
            digit_width: prepared.digit_width,
            range,
        },
    )?;
    let halo_rows = (halo_row_slots.len() as u64)
        .checked_sub(rows.len() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    let halo_columns = (halo_column_slots.len() as u64)
        .checked_sub(columns.len() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    let halo_cells = halo_product
        .checked_sub(
            (rows.len() as u64)
                .checked_mul(columns.len() as u64)
                .ok_or(RenderError::CoordinateOverflow)?,
        )
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok(SparseTileBuild {
        build,
        range,
        coordinate_visits: budget.visits,
        geometry_bytes: budget.geometry_bytes,
        halo_rows,
        halo_columns,
        halo_cells,
    })
}

#[cfg(test)]
mod sparse_scene_tests {
    use super::*;

    #[test]
    fn preparation_typography_and_conditional_counts_remain_separate_from_tiles() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("automatic");
        sheet.set_col_width(0, 2.0);
        sheet.write_styled(
            6_000,
            0,
            "long long wrapped words",
            &rxls::CellStyle::new().wrap(),
        );
        sheet.add_conditional_format(rxls::CondFormat::new(
            (6_000, 0, 6_000, 0),
            rxls::CfRule::expression("1=1", rxls::Color::rgb(20, 30, 40)),
        ));
        let pack = crate::font::synthetic_test_pack();
        let options = RenderOptions {
            default_font_family: pack.default_family().to_owned(),
            font_pack: Some(pack),
            ..RenderOptions::default()
        };
        let range = RenderRange::new(0, 0, 6_000, 0);
        let prepared =
            prepare_viewport(&workbook, 0, range, &options, ViewportLimits::default()).unwrap();
        let report = prepared.preparation_report().clone();
        assert!(report.text_bytes >= "long long wrapped words".len() as u64);
        assert!(report.shaped_runs > 0);
        assert!(report.text_work > 0);
        assert!(!report.font_faces.is_empty());
        assert!(report.conditional_evaluations > 0);
        let (y, height) = prepared.rows.track(6_000).unwrap();
        let width = prepared.columns.extent;
        let tile = render_viewport_tile(
            &prepared,
            Rect {
                x: Fixed::ZERO,
                y,
                width,
                height,
            },
            111,
        )
        .unwrap()
        .unwrap();
        assert_eq!(tile.report.visible_rows, 1);
        assert!(tile.coordinate_visits > 0);
        assert_eq!(prepared.preparation_report(), &report);
        assert_eq!(
            prepared.preparation_report().coordinate_visits,
            prepared.coordinate_visits()
        );
        assert_eq!(
            prepared.preparation_report().geometry_bytes,
            prepared.geometry_bytes()
        );
    }

    #[test]
    fn overlapping_merge_choice_is_global_even_when_overlap_is_outside_tile() {
        let mut workbook = Workbook::new();
        let sheet = workbook.add_sheet("overlap");
        sheet.set_default_row_height(15.0);
        sheet.merge(0, 0, 0, 2);
        sheet.merge(0, 2, 0, 4);
        sheet.write(0, 0, "FIRST");
        sheet.write(0, 2, "SECOND");
        let options = RenderOptions::default();
        let prepared = prepare_viewport(
            &workbook,
            0,
            RenderRange::new(0, 0, 0, 4),
            &options,
            ViewportLimits::default(),
        )
        .unwrap();
        assert_eq!(prepared.paint_merges.len(), 1);
        let (x, width) = prepared.columns.track(4).unwrap();
        let tile = render_viewport_tile(
            &prepared,
            Rect {
                x,
                y: Fixed::ZERO,
                width,
                height: Fixed::from_pixels(20),
            },
            112,
        )
        .unwrap()
        .unwrap();
        assert_eq!(tile.report.merged_regions, 0);
        let warning = prepared
            .preparation_report()
            .warnings
            .iter()
            .find(|warning| warning.code == crate::WarningCode::MergeSkipped)
            .unwrap();
        assert_eq!(warning.occurrences, 1);
        assert_eq!(
            warning.first_cell,
            Some(crate::CellCoordinate { row: 0, col: 2 })
        );
    }

    #[test]
    fn sparse_query_work_guard_does_not_publish_partial_output() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("work").set_default_row_height(15.0);
        let options = RenderOptions::default();
        let mut prepared = prepare_viewport(
            &workbook,
            0,
            RenderRange::new(0, 0, 0, 3),
            &options,
            ViewportLimits::default(),
        )
        .unwrap();
        prepared.limits.max_coordinate_visits = 1;
        let error = render_viewport_tile(
            &prepared,
            Rect {
                x: Fixed::ZERO,
                y: Fixed::ZERO,
                width: Fixed::from_pixels(100),
                height: Fixed::from_pixels(20),
            },
            113,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ViewportError::Limit {
                resource: "coordinate_visits",
                ..
            }
        ));
    }

    #[test]
    fn retained_physical_zero_tracks_are_bounded_while_hidden_zero_tracks_jump() {
        for included in [true, false] {
            let axis = ViewportAxis {
                runs: vec![
                    ViewportAxisRun {
                        first: 0,
                        last: 3,
                        offset: Fixed::ZERO,
                        size: Fixed::ZERO,
                        end: Fixed::ZERO,
                        included,
                    },
                    ViewportAxisRun {
                        first: 4,
                        last: 4,
                        offset: Fixed::ZERO,
                        size: Fixed::from_pixels(10),
                        end: Fixed::from_pixels(10),
                        included: true,
                    },
                ],
                extent: Fixed::from_pixels(10),
            };
            let mut budget = Budget::new(ViewportLimits::default());
            let slots =
                materialize_window::<u32>(&axis, Fixed::ZERO, axis.extent, 8, &mut budget).unwrap();
            assert_eq!(
                slots.iter().map(|slot| slot.index).collect::<Vec<_>>(),
                if included {
                    vec![0, 1, 2, 3, 4]
                } else {
                    vec![4]
                }
            );
            let mut budget = Budget::new(ViewportLimits::default());
            if included {
                assert!(matches!(
                    materialize_window::<u32>(&axis, Fixed::ZERO, axis.extent, 3, &mut budget),
                    Err(ViewportError::Limit {
                        resource: "visible_axis_slots",
                        actual: 4,
                        ..
                    })
                ));
            }
        }
    }
}

#[cfg(test)]
mod prepared_index_tests {
    use super::*;

    #[test]
    fn manual_and_font_backed_preparation_share_global_source_preflight() {
        for manual in [false, true] {
            for font_backed in [false, true] {
                let mut workbook = Workbook::new();
                let sheet = workbook.add_sheet("source-preflight");
                if manual {
                    sheet.set_default_row_height(15.0);
                }
                for _ in 0..3 {
                    sheet.write_number(0, 0, 1);
                }
                let mut options = RenderOptions::default();
                if font_backed {
                    let pack = crate::font::synthetic_test_pack();
                    options.default_font_family = pack.default_family().to_owned();
                    options.font_pack = Some(pack);
                }
                options.limits.max_cells = 2;
                let limits = ViewportLimits {
                    max_geometry_bytes: 0,
                    ..ViewportLimits::default()
                };
                assert!(matches!(
                    prepare_viewport(&workbook, 0, RenderRange::new(0, 0, 0, 0), &options, limits),
                    Err(ViewportError::Render(RenderError::LimitExceeded {
                        kind: LimitKind::Cells,
                        actual: 3,
                        ..
                    }))
                ));
            }
        }
    }

    #[test]
    fn empty_source_constructor_proof_has_no_index_heap_and_one_warm_visit() {
        let mut workbook = Workbook::new();
        workbook.add_sheet("empty");
        let options = RenderOptions::default();
        let mut budget = Budget::new(ViewportLimits::default());
        let index = preflight_source_index(&workbook.sheets[0], &options, &mut budget).unwrap();
        assert_eq!(
            (index.raw_cells, index.hyperlinks, index.build_peak_bytes),
            (0, 0, 0)
        );
        assert_eq!(budget.visits, 1);
        assert_eq!(budget.geometry_bytes, 0);
        assert!(std::ptr::eq(index.sheet, &workbook.sheets[0]));
    }
}
