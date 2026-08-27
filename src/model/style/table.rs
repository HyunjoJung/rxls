//! Table-style regions and deterministic cascade resolution.

use std::collections::BTreeMap;

use super::CellStyle;

/// A supported OOXML table-style region.
///
/// The declaration order is not used as rendering precedence. Resolution uses
/// an explicit sequence in [`TableStyleApplication::resolve`] so output remains
/// stable if this enum grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum TableStyleRegion {
    WholeTable,
    FirstColumnStripe,
    SecondColumnStripe,
    FirstRowStripe,
    SecondRowStripe,
    FirstColumn,
    LastColumn,
    HeaderRow,
    TotalRow,
    FirstHeaderCell,
    LastHeaderCell,
    FirstTotalCell,
    LastTotalCell,
}

/// One differential table style plus its stripe width where applicable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableStyleElement {
    pub(crate) style: CellStyle,
    pub(crate) stripe_size: u32,
}

impl TableStyleElement {
    #[cfg_attr(not(feature = "xlsx"), allow(dead_code))]
    pub(crate) fn new(style: CellStyle, stripe_size: u32) -> Self {
        Self {
            style,
            stripe_size: stripe_size.max(1),
        }
    }
}

/// Bounded table-style elements keyed by semantic region.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TableStyleDefinition {
    pub(crate) elements: BTreeMap<TableStyleRegion, TableStyleElement>,
}

impl TableStyleDefinition {
    #[cfg_attr(not(feature = "xlsx"), allow(dead_code))]
    pub(crate) fn insert(
        &mut self,
        region: TableStyleRegion,
        style: CellStyle,
        stripe_size: u32,
    ) -> Option<TableStyleElement> {
        self.elements
            .insert(region, TableStyleElement::new(style, stripe_size))
    }

    pub(crate) fn get(&self, region: TableStyleRegion) -> Option<&TableStyleElement> {
        self.elements.get(&region)
    }
}

/// Per-table switches from OOXML `<table>` and `<tableStyleInfo>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableStyleApplication {
    pub(crate) definition: TableStyleDefinition,
    pub(crate) header_row: bool,
    pub(crate) totals_row: bool,
    pub(crate) show_first_column: bool,
    pub(crate) show_last_column: bool,
    pub(crate) show_row_stripes: bool,
    pub(crate) show_column_stripes: bool,
}

impl Default for TableStyleApplication {
    fn default() -> Self {
        Self {
            definition: TableStyleDefinition::default(),
            header_row: true,
            totals_row: false,
            show_first_column: false,
            show_last_column: false,
            show_row_stripes: false,
            show_column_stripes: false,
        }
    }
}

impl TableStyleApplication {
    fn merge_layer(resolved: &mut Option<CellStyle>, layer: Option<&TableStyleElement>) {
        let Some(layer) = layer else {
            return;
        };
        *resolved = Some(match resolved.take() {
            Some(base) => base.merge(&layer.style),
            None => layer.style.clone(),
        });
    }

    fn stripe_layer(
        &self,
        first: TableStyleRegion,
        second: TableStyleRegion,
        offset: u32,
    ) -> Option<&TableStyleElement> {
        let first_style = self.definition.get(first);
        let second_style = self.definition.get(second);
        if first_style.is_none() && second_style.is_none() {
            return None;
        }
        // A missing half still occupies the default one-slot phase. This is
        // required for styles that deliberately format only alternating bands.
        let first_size = first_style.map_or(1, |style| style.stripe_size).max(1);
        let second_size = second_style.map_or(1, |style| style.stripe_size).max(1);
        let period = first_size.saturating_add(second_size).max(1);
        if offset % period < first_size {
            first_style
        } else {
            second_style
        }
    }

    /// Resolve table regions in a fixed low-to-high precedence order:
    /// whole table, column band, row band, first/last column, header/totals,
    /// then the four header/totals corner intersections.
    pub(crate) fn resolve(
        &self,
        range: (u32, u16, u32, u16),
        row: u32,
        col: u16,
    ) -> Option<CellStyle> {
        let (first_row, first_col, last_row, last_col) = range;
        if first_row > last_row
            || first_col > last_col
            || row < first_row
            || row > last_row
            || col < first_col
            || col > last_col
        {
            return None;
        }

        let is_header = self.header_row && row == first_row;
        let is_totals = self.totals_row && row == last_row;
        let body_first = first_row.saturating_add(u32::from(self.header_row));
        let body_last = last_row.saturating_sub(u32::from(self.totals_row));
        let is_body = body_first <= body_last && row >= body_first && row <= body_last;
        let is_first_col = col == first_col;
        let is_last_col = col == last_col;

        let mut resolved = None;
        Self::merge_layer(
            &mut resolved,
            self.definition.get(TableStyleRegion::WholeTable),
        );
        if is_body && self.show_column_stripes {
            Self::merge_layer(
                &mut resolved,
                self.stripe_layer(
                    TableStyleRegion::FirstColumnStripe,
                    TableStyleRegion::SecondColumnStripe,
                    u32::from(col - first_col),
                ),
            );
        }
        if is_body && self.show_row_stripes {
            Self::merge_layer(
                &mut resolved,
                self.stripe_layer(
                    TableStyleRegion::FirstRowStripe,
                    TableStyleRegion::SecondRowStripe,
                    row - body_first,
                ),
            );
        }
        if self.show_first_column && is_first_col {
            Self::merge_layer(
                &mut resolved,
                self.definition.get(TableStyleRegion::FirstColumn),
            );
        }
        if self.show_last_column && is_last_col {
            Self::merge_layer(
                &mut resolved,
                self.definition.get(TableStyleRegion::LastColumn),
            );
        }
        if is_header {
            Self::merge_layer(
                &mut resolved,
                self.definition.get(TableStyleRegion::HeaderRow),
            );
        }
        if is_totals {
            Self::merge_layer(
                &mut resolved,
                self.definition.get(TableStyleRegion::TotalRow),
            );
        }
        if is_header && is_first_col {
            Self::merge_layer(
                &mut resolved,
                self.definition.get(TableStyleRegion::FirstHeaderCell),
            );
        }
        if is_header && is_last_col {
            Self::merge_layer(
                &mut resolved,
                self.definition.get(TableStyleRegion::LastHeaderCell),
            );
        }
        if is_totals && is_first_col {
            Self::merge_layer(
                &mut resolved,
                self.definition.get(TableStyleRegion::FirstTotalCell),
            );
        }
        if is_totals && is_last_col {
            Self::merge_layer(
                &mut resolved,
                self.definition.get(TableStyleRegion::LastTotalCell),
            );
        }
        resolved
    }
}
