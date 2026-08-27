use quick_xml::events::BytesStart;

use super::super::refs::{parse_range, SheetRange};
use super::super::style::Styles;
use super::super::{attr, attr_false, attr_true};
use super::ParsedSheet;
use crate::{
    CfRule, Color, CondFormat, ConditionalFormatMetadata, DataValidation, DvKind, DvOp,
    FormatPattern, StyleLoss, StyleLossKind,
};

type ParsedDataValidation = (DataValidation, Vec<SheetRange>);

#[derive(Debug)]
enum PendingCfKind {
    CellIs {
        op: DvOp,
        fill: Color,
    },
    ColorScale,
    DataBar,
    TopBottom {
        rank: u32,
        bottom: bool,
        percent: bool,
        fill: Color,
    },
    AboveAverage {
        below: bool,
        fill: Color,
    },
    DuplicateValues {
        unique: bool,
        fill: Color,
    },
    Expression {
        fill: Color,
    },
}

#[derive(Debug)]
pub(super) struct PendingCfRule {
    pub(super) ranges: Vec<SheetRange>,
    kind: PendingCfKind,
    pub(super) formulas: Vec<String>,
    pub(super) colors: Vec<Color>,
    metadata: ConditionalFormatMetadata,
}

impl PendingCfRule {
    fn build_rule(&self) -> Option<CfRule> {
        match &self.kind {
            PendingCfKind::CellIs { op, fill } => Some(CfRule::CellIs {
                op: *op,
                formula1: self.formulas.first()?.clone(),
                formula2: self.formulas.get(1).filter(|s| !s.is_empty()).cloned(),
                fill: *fill,
            }),
            PendingCfKind::ColorScale => match self.colors.as_slice() {
                [min, max] => Some(CfRule::ColorScale2 {
                    min: *min,
                    max: *max,
                }),
                [min, mid, max, ..] => Some(CfRule::ColorScale3 {
                    min: *min,
                    mid: *mid,
                    max: *max,
                }),
                _ => None,
            },
            PendingCfKind::DataBar => self
                .colors
                .first()
                .copied()
                .map(|color| CfRule::DataBar { color }),
            PendingCfKind::TopBottom {
                rank,
                bottom,
                percent,
                fill,
            } => Some(CfRule::TopBottom {
                rank: *rank,
                bottom: *bottom,
                percent: *percent,
                fill: *fill,
            }),
            PendingCfKind::AboveAverage { below, fill } => Some(CfRule::AboveAverage {
                below: *below,
                fill: *fill,
            }),
            PendingCfKind::DuplicateValues { unique, fill } => Some(CfRule::DuplicateValues {
                unique: *unique,
                fill: *fill,
            }),
            PendingCfKind::Expression { fill } => Some(CfRule::Expression {
                formula: self.formulas.first()?.clone(),
                fill: *fill,
            }),
        }
    }
}

fn parse_conditional_metadata(
    e: &quick_xml::events::BytesStart<'_>,
    styles: &Styles,
) -> ConditionalFormatMetadata {
    let mut metadata = ConditionalFormatMetadata {
        priority: attr(e, b"priority")
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|priority| *priority != 0),
        stop_if_true: attr(e, b"stopIfTrue").as_deref().is_some_and(attr_true),
        ..ConditionalFormatMetadata::default()
    };
    let Some(dxf_id) = attr(e, b"dxfId") else {
        return metadata;
    };
    let Some(dxf) = dxf_id
        .parse::<usize>()
        .ok()
        .and_then(|id| styles.differential_style(id))
    else {
        metadata.style_losses.push(StyleLoss {
            kind: StyleLossKind::MissingReference,
            occurrences: 1,
        });
        return metadata;
    };
    metadata.differential_style = Some(dxf.style.clone());
    metadata.style_losses = dxf.losses.clone();
    metadata
}

fn conditional_compatibility_fill(metadata: &ConditionalFormatMetadata) -> Color {
    metadata
        .differential_style
        .as_ref()
        .and_then(|style| {
            style.fill.or_else(|| {
                style.pattern_fill.and_then(|fill| {
                    (fill.pattern == FormatPattern::Solid)
                        .then(|| fill.foreground.or(fill.background))
                        .flatten()
                })
            })
        })
        .unwrap_or_default()
}

pub(super) fn parse_conditional_rule(
    e: &quick_xml::events::BytesStart<'_>,
    ranges: &[SheetRange],
    styles: &Styles,
) -> Option<PendingCfRule> {
    if ranges.is_empty() {
        return None;
    }
    let ty = attr(e, b"type")?;
    let metadata = parse_conditional_metadata(e, styles);
    let compatibility_fill = conditional_compatibility_fill(&metadata);
    let kind = match ty.as_str() {
        "cellIs" => PendingCfKind::CellIs {
            op: attr(e, b"operator")
                .as_deref()
                .and_then(parse_dv_op)
                .unwrap_or(DvOp::Between),
            fill: compatibility_fill,
        },
        "colorScale" => PendingCfKind::ColorScale,
        "dataBar" => PendingCfKind::DataBar,
        "top10" => PendingCfKind::TopBottom {
            rank: attr(e, b"rank")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(10),
            bottom: attr(e, b"bottom").as_deref().is_some_and(attr_true),
            percent: attr(e, b"percent").as_deref().is_some_and(attr_true),
            fill: compatibility_fill,
        },
        "aboveAverage" => PendingCfKind::AboveAverage {
            below: attr(e, b"aboveAverage").as_deref().is_some_and(attr_false),
            fill: compatibility_fill,
        },
        "duplicateValues" => PendingCfKind::DuplicateValues {
            unique: false,
            fill: compatibility_fill,
        },
        "uniqueValues" => PendingCfKind::DuplicateValues {
            unique: true,
            fill: compatibility_fill,
        },
        "expression" => PendingCfKind::Expression {
            fill: compatibility_fill,
        },
        _ => return None,
    };
    Some(PendingCfRule {
        ranges: ranges.to_vec(),
        kind,
        formulas: Vec::new(),
        colors: Vec::new(),
        metadata,
    })
}

pub(super) fn push_current_conditional_format(
    parsed: &mut ParsedSheet,
    current: Option<PendingCfRule>,
) {
    let Some(current) = current else {
        return;
    };
    let Some(rule) = current.build_rule() else {
        return;
    };
    for sqref in current.ranges.into_iter().take(1 << 16) {
        parsed.cond_formats.push(CondFormat {
            sqref,
            rule: rule.clone(),
        });
        parsed.cond_format_metadata.push(current.metadata.clone());
    }
}

pub(super) fn parse_data_validation(e: &BytesStart<'_>) -> Option<ParsedDataValidation> {
    let ranges: Vec<_> = attr(e, b"sqref")?
        .split_whitespace()
        .filter_map(parse_range)
        .collect();
    let (&sqref, rest) = ranges.split_first()?;
    let kind = attr(e, b"type").as_deref().and_then(parse_dv_kind)?;
    let operator = attr(e, b"operator")
        .as_deref()
        .and_then(parse_dv_op)
        .unwrap_or(DvOp::Between);
    let allow_blank = attr(e, b"allowBlank")
        .as_deref()
        .map(attr_true)
        .unwrap_or(false);
    let show_input_message = attr(e, b"showInputMessage")
        .as_deref()
        .map(attr_true)
        .unwrap_or(false);
    let show_error_message = attr(e, b"showErrorMessage")
        .as_deref()
        .map(attr_true)
        .unwrap_or(false);
    let prompt = match (attr(e, b"promptTitle"), attr(e, b"prompt")) {
        (None, None) => None,
        (title, message) => Some((title.unwrap_or_default(), message.unwrap_or_default())),
    };
    let error = match (attr(e, b"errorTitle"), attr(e, b"error")) {
        (None, None) => None,
        (title, message) => Some((title.unwrap_or_default(), message.unwrap_or_default())),
    };
    Some((
        DataValidation {
            sqref,
            kind,
            operator,
            formula1: String::new(),
            formula2: None,
            allow_blank,
            show_input_message,
            show_error_message,
            prompt,
            error,
        },
        rest.to_vec(),
    ))
}

pub(super) fn push_current_data_validation(
    parsed: &mut ParsedSheet,
    current: Option<DataValidation>,
    extra_ranges: &mut Vec<SheetRange>,
) {
    let Some(mut dv) = current else {
        extra_ranges.clear();
        return;
    };
    if dv.formula1.is_empty() {
        extra_ranges.clear();
        return;
    }
    if dv.formula2.as_deref() == Some("") {
        dv.formula2 = None;
    }
    parsed.data_validations.push(dv.clone());
    for sqref in extra_ranges.drain(..) {
        let mut clone = dv.clone();
        clone.sqref = sqref;
        parsed.data_validations.push(clone);
    }
}

fn parse_dv_kind(value: &str) -> Option<DvKind> {
    match value {
        "list" => Some(DvKind::List),
        "whole" => Some(DvKind::Whole),
        "decimal" => Some(DvKind::Decimal),
        "date" => Some(DvKind::Date),
        "time" => Some(DvKind::Time),
        "textLength" => Some(DvKind::TextLength),
        "custom" => Some(DvKind::Custom),
        _ => None,
    }
}

fn parse_dv_op(value: &str) -> Option<DvOp> {
    match value {
        "between" => Some(DvOp::Between),
        "notBetween" => Some(DvOp::NotBetween),
        "equal" => Some(DvOp::Equal),
        "notEqual" => Some(DvOp::NotEqual),
        "greaterThan" => Some(DvOp::GreaterThan),
        "lessThan" => Some(DvOp::LessThan),
        "greaterThanOrEqual" => Some(DvOp::GreaterThanOrEqual),
        "lessThanOrEqual" => Some(DvOp::LessThanOrEqual),
        _ => None,
    }
}
