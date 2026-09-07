//! Deterministic shared cell-border and print-gridline composition.

use std::collections::BTreeMap;

use rxls::{Border, BorderStyle, Color};

use crate::error::{LimitKind, RenderError};
use crate::scene::{Fixed, LineNode, Rect, Rgb, SceneNode};

use super::{
    enforce, push_node, rgb, AxisSlot, CellCoordinate, GridlinePolicy, Region, RenderOptions,
    PRINT_GRIDLINE_FRAME_LEFT_INSET, PRINT_GRIDLINE_FRAME_TOP_INSET,
    PRINT_GRIDLINE_FRAME_TRAILING_INSET, PRINT_GRIDLINE_WIDTH,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ComposedEdgeOrientation {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ComposedEdgeKey {
    pub(super) orientation: ComposedEdgeOrientation,
    pub(super) axis: Fixed,
    pub(super) start: Fixed,
    pub(super) end: Fixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum CellEdge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct EdgeClaim {
    pub(super) kind: EdgeClaimKind,
    pub(super) style: BorderStyle,
    pub(super) color: Rgb,
    pub(super) owner: CellCoordinate,
    pub(super) side: CellEdge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum EdgeClaimKind {
    Gridline,
    GridlineSuppression,
    Explicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EdgeCompositionMode {
    Combined,
    CalcMetafileGrid,
}

pub(super) fn compose_edges(
    regions: &[Region],
    suppresses_gridlines: &[bool],
    show_gridlines: bool,
    gridline_policy: GridlinePolicy,
    mode: EdgeCompositionMode,
    options: &RenderOptions,
) -> Result<Vec<(ComposedEdgeKey, EdgeClaim)>, RenderError> {
    #[derive(Debug, Clone, Copy)]
    struct RawEdgeClaim {
        start: Fixed,
        end: Fixed,
        claim: EdgeClaim,
    }

    #[derive(Default)]
    struct EdgeEvents {
        starts: Vec<(usize, EdgeClaim)>,
        ends: Vec<usize>,
    }

    // Four source-side claims are one bounded compositor work unit. This
    // admits a coalesced 2x2 grid at its exact six-node output limit (16
    // claims = four work units) while stopping large low-output grids from
    // consuming unbounded intermediate memory.
    const CLAIMS_PER_COMPOSITOR_WORK_UNIT: u64 = 4;
    let mut claim_count = 0_u64;
    let mut raw_claims = BTreeMap::<(ComposedEdgeOrientation, Fixed), Vec<RawEdgeClaim>>::new();
    for (region_index, region) in regions.iter().enumerate() {
        let right = region
            .rect
            .x
            .checked_add(region.rect.width)
            .ok_or(RenderError::CoordinateOverflow)?;
        let bottom = region
            .rect
            .y
            .checked_add(region.rect.height)
            .ok_or(RenderError::CoordinateOverflow)?;
        for (side, orientation, axis, start, end) in [
            (
                CellEdge::Left,
                ComposedEdgeOrientation::Vertical,
                region.rect.x,
                region.rect.y,
                bottom,
            ),
            (
                CellEdge::Right,
                ComposedEdgeOrientation::Vertical,
                right,
                region.rect.y,
                bottom,
            ),
            (
                CellEdge::Top,
                ComposedEdgeOrientation::Horizontal,
                region.rect.y,
                region.rect.x,
                right,
            ),
            (
                CellEdge::Bottom,
                ComposedEdgeOrientation::Horizontal,
                bottom,
                region.rect.x,
                right,
            ),
        ] {
            let Some(claim) = region_edge_claim(
                region,
                side,
                suppresses_gridlines
                    .get(region_index)
                    .copied()
                    .unwrap_or(false),
                show_gridlines,
                mode,
            ) else {
                continue;
            };
            if start >= end {
                continue;
            }
            claim_count = claim_count
                .checked_add(1)
                .ok_or(RenderError::CoordinateOverflow)?;
            let work_units = claim_count
                .checked_add(CLAIMS_PER_COMPOSITOR_WORK_UNIT - 1)
                .ok_or(RenderError::CoordinateOverflow)?
                / CLAIMS_PER_COMPOSITOR_WORK_UNIT;
            enforce(
                LimitKind::SceneNodes,
                options.limits.max_scene_nodes,
                work_units,
            )?;
            raw_claims
                .entry((orientation, axis))
                .or_default()
                .push(RawEdgeClaim { start, end, claim });
        }
    }

    // Resolve only claims sharing the same geometric axis. The event sweep is
    // linear in retained claims and replaces global Cartesian segmentation.
    let mut composed = BTreeMap::<ComposedEdgeKey, EdgeClaim>::new();
    for ((orientation, axis), claims) in raw_claims {
        let mut events = BTreeMap::<Fixed, EdgeEvents>::new();
        for (claim_index, raw) in claims.into_iter().enumerate() {
            events
                .entry(raw.start)
                .or_default()
                .starts
                .push((claim_index, raw.claim));
            events.entry(raw.end).or_default().ends.push(claim_index);
        }
        let mut active = BTreeMap::<usize, EdgeClaim>::new();
        let mut events = events.into_iter().peekable();
        while let Some((start, event)) = events.next() {
            for claim_index in event.ends {
                active.remove(&claim_index);
            }
            for (claim_index, claim) in event.starts {
                active.insert(claim_index, claim);
            }
            let Some((end, _)) = events.peek() else {
                break;
            };
            let end = *end;
            if start >= end {
                continue;
            }
            let Some(winner) = active.values().copied().reduce(|current, candidate| {
                if edge_claim_precedes(candidate, current, gridline_policy) {
                    candidate
                } else {
                    current
                }
            }) else {
                continue;
            };
            composed.insert(
                ComposedEdgeKey {
                    orientation,
                    axis,
                    start,
                    end,
                },
                winner,
            );
        }
    }
    Ok(coalesce_composed_edges(composed))
}

fn calc_axis_boundary_remap<I: Copy + PartialEq>(
    cell_slots: &[AxisSlot<I>],
    grid_slots: &[AxisSlot<I>],
) -> Result<BTreeMap<Fixed, Fixed>, RenderError> {
    if cell_slots.len() != grid_slots.len() {
        return Err(RenderError::CoordinateOverflow);
    }
    let mut boundaries = BTreeMap::new();
    for (cell, grid) in cell_slots.iter().zip(grid_slots) {
        if cell.index != grid.index {
            return Err(RenderError::CoordinateOverflow);
        }
        let cell_end = cell
            .offset
            .checked_add(cell.size)
            .ok_or(RenderError::CoordinateOverflow)?;
        let grid_end = grid
            .offset
            .checked_add(grid.size)
            .ok_or(RenderError::CoordinateOverflow)?;
        for (source, target) in [(cell.offset, grid.offset), (cell_end, grid_end)] {
            if boundaries
                .get(&source)
                .is_some_and(|existing| *existing != target)
            {
                return Err(RenderError::CoordinateOverflow);
            }
            boundaries.insert(source, target);
        }
    }
    Ok(boundaries)
}

/// Keep the compositor's logical edge decisions, but move Calc
/// SinglePageSheets grid claims onto the separate `DrawToDev` metafile axis.
/// Every endpoint remains a cell-track boundary, so merge, fill, overflow, and
/// hidden-track suppression survive unchanged.
pub(super) fn remap_calc_metafile_grid_edges(
    composed: &[(ComposedEdgeKey, EdgeClaim)],
    cell_rows: &[AxisSlot<u32>],
    cell_columns: &[AxisSlot<u16>],
    grid_rows: &[AxisSlot<u32>],
    grid_columns: &[AxisSlot<u16>],
) -> Result<Vec<(ComposedEdgeKey, EdgeClaim)>, RenderError> {
    let rows = calc_axis_boundary_remap(cell_rows, grid_rows)?;
    let columns = calc_axis_boundary_remap(cell_columns, grid_columns)?;
    composed
        .iter()
        .map(|&(mut key, claim)| {
            if claim.kind == EdgeClaimKind::Gridline {
                let (axis, spans) = match key.orientation {
                    ComposedEdgeOrientation::Vertical => (&columns, &rows),
                    ComposedEdgeOrientation::Horizontal => (&rows, &columns),
                };
                key.axis = axis
                    .get(&key.axis)
                    .copied()
                    .ok_or(RenderError::CoordinateOverflow)?;
                key.start = spans
                    .get(&key.start)
                    .copied()
                    .ok_or(RenderError::CoordinateOverflow)?;
                key.end = spans
                    .get(&key.end)
                    .copied()
                    .ok_or(RenderError::CoordinateOverflow)?;
                if key.start > key.end {
                    std::mem::swap(&mut key.start, &mut key.end);
                }
            }
            Ok((key, claim))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn push_composed_edges(
    nodes: &mut Vec<SceneNode>,
    composed: &[(ComposedEdgeKey, EdgeClaim)],
    layer: EdgeClaimKind,
    gridline_policy: GridlinePolicy,
    grid_bounds: Rect,
    scene_bounds: Rect,
    right_to_left: bool,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    debug_assert!(matches!(
        layer,
        EdgeClaimKind::Gridline | EdgeClaimKind::Explicit
    ));
    for &(key, claim) in composed {
        if claim.kind != layer {
            continue;
        }
        let print_gridline =
            layer == EdgeClaimKind::Gridline && gridline_policy != GridlinePolicy::WorksheetView;
        if print_gridline && is_print_gridline_leading_edge(key, grid_bounds, right_to_left)? {
            continue;
        }
        let key = if layer == EdgeClaimKind::Gridline
            && gridline_policy == GridlinePolicy::CalcSinglePagePrint
        {
            let Some(clipped) = clip_composed_edge(key, scene_bounds, PRINT_GRIDLINE_WIDTH)? else {
                continue;
            };
            clipped
        } else {
            key
        };
        if print_gridline {
            push_node(
                nodes,
                SceneNode::Line(edge_line(
                    key,
                    Fixed::ZERO,
                    Rgb::BLACK,
                    PRINT_GRIDLINE_WIDTH,
                )?),
                options,
            )?;
            continue;
        }
        if claim.style == BorderStyle::Double {
            push_double_edge(nodes, key, claim, options)?;
            continue;
        }
        let Some(width) = border_width(claim.style) else {
            continue;
        };
        push_node(
            nodes,
            SceneNode::Line(edge_line(key, Fixed::ZERO, claim.color, width)?),
            options,
        )?;
    }
    Ok(())
}

fn is_print_gridline_leading_edge(
    key: ComposedEdgeKey,
    grid_bounds: Rect,
    right_to_left: bool,
) -> Result<bool, RenderError> {
    let right = grid_bounds
        .x
        .checked_add(grid_bounds.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok(match key.orientation {
        ComposedEdgeOrientation::Vertical => {
            key.axis == if right_to_left { right } else { grid_bounds.x }
        }
        ComposedEdgeOrientation::Horizontal => key.axis == grid_bounds.y,
    })
}

fn clip_composed_edge(
    mut key: ComposedEdgeKey,
    clip: Rect,
    stroke_width: Fixed,
) -> Result<Option<ComposedEdgeKey>, RenderError> {
    let right = clip
        .x
        .checked_add(clip.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let bottom = clip
        .y
        .checked_add(clip.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    let axis_slop = Fixed::from_raw(
        stroke_width
            .raw()
            .checked_add(1)
            .ok_or(RenderError::CoordinateOverflow)?
            / 2,
    );
    let left_axis_limit = clip
        .x
        .checked_sub(axis_slop)
        .ok_or(RenderError::CoordinateOverflow)?;
    let right_axis_limit = right
        .checked_add(axis_slop)
        .ok_or(RenderError::CoordinateOverflow)?;
    let top_axis_limit = clip
        .y
        .checked_sub(axis_slop)
        .ok_or(RenderError::CoordinateOverflow)?;
    let bottom_axis_limit = bottom
        .checked_add(axis_slop)
        .ok_or(RenderError::CoordinateOverflow)?;
    match key.orientation {
        ComposedEdgeOrientation::Vertical => {
            if key.axis < left_axis_limit || key.axis > right_axis_limit {
                return Ok(None);
            }
            key.start = std::cmp::max(key.start, clip.y);
            key.end = std::cmp::min(key.end, bottom);
        }
        ComposedEdgeOrientation::Horizontal => {
            if key.axis < top_axis_limit || key.axis > bottom_axis_limit {
                return Ok(None);
            }
            key.start = std::cmp::max(key.start, clip.x);
            key.end = std::cmp::min(key.end, right);
        }
    }
    Ok((key.start < key.end).then_some(key))
}

pub(super) fn push_print_gridline_leading_frame(
    nodes: &mut Vec<SceneNode>,
    grid_bounds: Rect,
    scene_bounds: Rect,
    right_to_left: bool,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    let grid_right = grid_bounds
        .x
        .checked_add(grid_bounds.width)
        .ok_or(RenderError::CoordinateOverflow)?;
    let grid_bottom = grid_bounds
        .y
        .checked_add(grid_bounds.height)
        .ok_or(RenderError::CoordinateOverflow)?;
    // Calc keeps the leading frame in normal page space, but it is inset by
    // its integer Map100thMM stroke rectangle rather than centered on the
    // MediaBox. This makes both frame edges survive PDF clipping at 96 DPI.
    let left = grid_bounds
        .x
        .checked_add(PRINT_GRIDLINE_FRAME_LEFT_INSET)
        .ok_or(RenderError::CoordinateOverflow)?;
    let right = grid_right
        .checked_sub(PRINT_GRIDLINE_FRAME_TRAILING_INSET)
        .ok_or(RenderError::CoordinateOverflow)?;
    let top = grid_bounds
        .y
        .checked_add(PRINT_GRIDLINE_FRAME_TOP_INSET)
        .ok_or(RenderError::CoordinateOverflow)?;
    let bottom = grid_bottom
        .checked_sub(PRINT_GRIDLINE_FRAME_TRAILING_INSET)
        .ok_or(RenderError::CoordinateOverflow)?;
    if left >= right || top >= bottom {
        return Ok(());
    }
    let leading_x = if right_to_left { right } else { left };
    for key in [
        ComposedEdgeKey {
            orientation: ComposedEdgeOrientation::Vertical,
            axis: leading_x,
            start: top,
            end: bottom,
        },
        ComposedEdgeKey {
            orientation: ComposedEdgeOrientation::Horizontal,
            axis: top,
            start: left,
            end: right,
        },
    ] {
        let Some(key) = clip_composed_edge(key, scene_bounds, PRINT_GRIDLINE_WIDTH)? else {
            continue;
        };
        push_node(
            nodes,
            SceneNode::Line(edge_line(
                key,
                Fixed::ZERO,
                Rgb::BLACK,
                PRINT_GRIDLINE_WIDTH,
            )?),
            options,
        )?;
    }
    Ok(())
}

fn region_edge_claim(
    region: &Region,
    side: CellEdge,
    suppresses_gridlines: bool,
    show_gridlines: bool,
    mode: EdgeCompositionMode,
) -> Option<EdgeClaim> {
    if mode == EdgeCompositionMode::CalcMetafileGrid {
        return show_gridlines.then_some(EdgeClaim {
            kind: if suppresses_gridlines {
                EdgeClaimKind::GridlineSuppression
            } else {
                EdgeClaimKind::Gridline
            },
            style: BorderStyle::Thin,
            color: Rgb::GRIDLINE,
            owner: region.source,
            side,
        });
    }
    let (style, color) = region
        .style
        .as_ref()
        .and_then(|style| style.border.as_ref())
        .map_or((BorderStyle::None, None), |border| {
            border_edge_style_and_color(border, side)
        });
    if style != BorderStyle::None {
        return Some(EdgeClaim {
            kind: EdgeClaimKind::Explicit,
            style,
            color: color.map(rgb).unwrap_or(Rgb::BLACK),
            owner: region.source,
            side,
        });
    }
    show_gridlines.then_some(EdgeClaim {
        kind: if suppresses_gridlines {
            EdgeClaimKind::GridlineSuppression
        } else {
            EdgeClaimKind::Gridline
        },
        style: BorderStyle::Thin,
        color: Rgb::GRIDLINE,
        owner: region.source,
        side,
    })
}

fn border_edge_style_and_color(border: &Border, side: CellEdge) -> (BorderStyle, Option<Color>) {
    match side {
        CellEdge::Left => (border.left, border.left_color.or(border.color)),
        CellEdge::Right => (border.right, border.right_color.or(border.color)),
        CellEdge::Top => (border.top, border.top_color.or(border.color)),
        CellEdge::Bottom => (border.bottom, border.bottom_color.or(border.color)),
    }
}

fn edge_claim_precedes(
    candidate: EdgeClaim,
    current: EdgeClaim,
    gridline_policy: GridlinePolicy,
) -> bool {
    let candidate_strength = (
        edge_claim_kind_precedence(candidate.kind, gridline_policy),
        border_precedence(candidate.style),
    );
    let current_strength = (
        edge_claim_kind_precedence(current.kind, gridline_policy),
        border_precedence(current.style),
    );
    candidate_strength > current_strength
        || (candidate_strength == current_strength
            && (candidate.owner, candidate.side) < (current.owner, current.side))
}

fn edge_claim_kind_precedence(kind: EdgeClaimKind, gridline_policy: GridlinePolicy) -> u8 {
    match (kind, gridline_policy) {
        (EdgeClaimKind::Explicit, _) => 2,
        (EdgeClaimKind::GridlineSuppression, GridlinePolicy::WorksheetView) => 1,
        (EdgeClaimKind::Gridline, GridlinePolicy::WorksheetView) => 0,
        (EdgeClaimKind::Gridline, _) => 1,
        (EdgeClaimKind::GridlineSuppression, _) => 0,
    }
}

fn border_precedence(style: BorderStyle) -> u8 {
    match style {
        BorderStyle::None => 0,
        BorderStyle::Thin => 1,
        BorderStyle::Medium => 2,
        BorderStyle::Thick => 3,
        BorderStyle::Double => 4,
    }
}

fn coalesce_composed_edges(
    composed: BTreeMap<ComposedEdgeKey, EdgeClaim>,
) -> Vec<(ComposedEdgeKey, EdgeClaim)> {
    let mut coalesced: Vec<(ComposedEdgeKey, EdgeClaim)> = Vec::new();
    for (key, claim) in composed {
        if let Some((previous_key, previous_claim)) = coalesced.last_mut() {
            if previous_key.orientation == key.orientation
                && previous_key.axis == key.axis
                && previous_key.end == key.start
                && claims_are_visually_identical(*previous_claim, claim)
            {
                previous_key.end = key.end;
                continue;
            }
        }
        coalesced.push((key, claim));
    }
    coalesced
}

fn claims_are_visually_identical(left: EdgeClaim, right: EdgeClaim) -> bool {
    left.kind == right.kind && left.style == right.style && left.color == right.color
}

fn push_double_edge(
    nodes: &mut Vec<SceneNode>,
    key: ComposedEdgeKey,
    claim: EdgeClaim,
    options: &RenderOptions,
) -> Result<(), RenderError> {
    // Calc centers a shared double rule on the geometric boundary. Symmetric
    // placement makes equivalent A.right/B.left (and top/bottom) authorship
    // identical, including after RTL reflection.
    for offset in [Fixed::from_pixels(-1), Fixed::from_pixels(1)] {
        push_node(
            nodes,
            SceneNode::Line(edge_line(key, offset, claim.color, Fixed::from_pixels(1))?),
            options,
        )?;
    }
    Ok(())
}

fn edge_line(
    key: ComposedEdgeKey,
    axis_offset: Fixed,
    color: Rgb,
    width: Fixed,
) -> Result<LineNode, RenderError> {
    let axis = key
        .axis
        .checked_add(axis_offset)
        .ok_or(RenderError::CoordinateOverflow)?;
    Ok(match key.orientation {
        ComposedEdgeOrientation::Vertical => LineNode {
            x1: axis,
            y1: key.start,
            x2: axis,
            y2: key.end,
            color,
            width,
        },
        ComposedEdgeOrientation::Horizontal => LineNode {
            x1: key.start,
            y1: axis,
            x2: key.end,
            y2: axis,
            color,
            width,
        },
    })
}

fn border_width(style: BorderStyle) -> Option<Fixed> {
    match style {
        BorderStyle::None => None,
        BorderStyle::Thin => Some(Fixed::from_pixels(1)),
        BorderStyle::Medium => Some(Fixed::from_pixels(2)),
        BorderStyle::Thick | BorderStyle::Double => Some(Fixed::from_pixels(3)),
    }
}
