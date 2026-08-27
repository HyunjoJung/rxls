use std::io::Read;

use quick_xml::events::{BytesRef, Event};
use quick_xml::{Reader, XmlVersion};

use crate::{
    Chart, DrawingAnchorBehavior, DrawingCrop, DrawingMetadata, DrawingObjectKind, Image, ImageFmt,
    StyleLoss, StyleLossKind,
};

use super::style::{add_style_loss, XlsbTheme};
use super::{
    normalize_part_target, part_index, sheet_rels_path, MAX_XLSB_DRAWINGS, MAX_XLSB_DRAWING_TEXT,
};

fn xml_local(name: &[u8]) -> &[u8] {
    name.iter()
        .rposition(|byte| *byte == b':')
        .map_or(name, |index| &name[index + 1..])
}

fn xml_attr(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|attribute| {
        (xml_local(attribute.key.as_ref()) == key)
            .then(|| {
                attribute
                    .decoded_and_normalized_value_with(
                        XmlVersion::Implicit1_0,
                        e.decoder(),
                        1,
                        quick_xml::escape::resolve_xml_entity,
                    )
                    .ok()
                    .map(|value| value.into_owned())
            })
            .flatten()
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum XlsbDrawingKind {
    Image,
    Chart,
    Shape,
}

#[derive(Clone)]
pub(super) struct XlsbDrawingRef {
    pub(super) kind: XlsbDrawingKind,
    pub(super) rid: Option<String>,
    pub(super) from: (u32, u16),
    pub(super) to: Option<(u32, u16)>,
    pub(super) metadata: DrawingMetadata,
}

#[derive(Clone, Copy)]
enum DrawingMarker {
    From,
    To,
}

#[derive(Clone, Copy)]
enum DrawingMarkerField {
    Row,
    Col,
    RowOffset,
    ColOffset,
}

pub(super) fn drawing_relationship_target(xml: &str) -> crate::xlsx::RelationshipTarget {
    crate::xlsx::unique_internal_relationship_target(xml, "drawing")
}

fn drawing_text(e: &quick_xml::events::BytesText<'_>) -> String {
    e.decode().map(|text| text.into_owned()).unwrap_or_default()
}

fn truncate_drawing_text(value: &mut String) {
    if value.len() <= MAX_XLSB_DRAWING_TEXT {
        return;
    }
    let mut end = MAX_XLSB_DRAWING_TEXT;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

fn append_drawing_ref(out: &mut String, reference: &BytesRef<'_>) {
    if out.len() >= MAX_XLSB_DRAWING_TEXT {
        return;
    }
    match reference.resolve_char_ref() {
        Ok(Some(ch)) => out.push(ch),
        Ok(None) => {
            if let Ok(name) = reference.decode() {
                if let Some(value) = quick_xml::escape::resolve_xml_entity(&name) {
                    out.push_str(value);
                }
            }
        }
        Err(_) => {}
    }
    truncate_drawing_text(out);
}

fn parse_anchor_behavior(
    element: &[u8],
    e: &quick_xml::events::BytesStart<'_>,
) -> DrawingAnchorBehavior {
    match element {
        b"absoluteAnchor" => DrawingAnchorBehavior::Absolute,
        b"oneCellAnchor" => DrawingAnchorBehavior::MoveOnly,
        b"twoCellAnchor" => match xml_attr(e, b"editAs").as_deref() {
            Some("absolute") => DrawingAnchorBehavior::Absolute,
            Some("oneCell") => DrawingAnchorBehavior::MoveOnly,
            _ => DrawingAnchorBehavior::MoveAndSize,
        },
        _ => DrawingAnchorBehavior::MoveAndSize,
    }
}

pub(super) fn parse_xlsb_drawing_refs(xml: &str) -> Vec<XlsbDrawingRef> {
    const MAX_ROW: u32 = 1_048_575;
    const MAX_COL: u16 = 16_383;
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut current: Option<XlsbDrawingRef> = None;
    let mut marker = None;
    let mut field = None;
    let mut field_text = String::new();
    let mut from_offset = (0i64, 0i64);
    let mut to_offset = (0i64, 0i64);
    let mut from_offset_seen = false;
    let mut to_offset_seen = false;
    let mut from_row_seen = false;
    let mut from_col_seen = false;
    let mut to_row_seen = false;
    let mut to_col_seen = false;
    let mut desc_depth = 0usize;
    let mut desc_text = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.name();
                let local = xml_local(name.as_ref());
                match local {
                    b"twoCellAnchor" | b"oneCellAnchor" | b"absoluteAnchor" => {
                        if out.len() >= MAX_XLSB_DRAWINGS {
                            break;
                        }
                        current = Some(XlsbDrawingRef {
                            kind: XlsbDrawingKind::Shape,
                            rid: None,
                            from: (0, 0),
                            to: None,
                            metadata: DrawingMetadata {
                                behavior: parse_anchor_behavior(local, &e),
                                z_order: Some(out.len().min(i32::MAX as usize) as i32),
                                ..Default::default()
                            },
                        });
                        from_offset = (0, 0);
                        to_offset = (0, 0);
                        from_offset_seen = false;
                        to_offset_seen = false;
                        from_row_seen = false;
                        from_col_seen = false;
                        to_row_seen = false;
                        to_col_seen = false;
                    }
                    b"from" if current.is_some() => marker = Some(DrawingMarker::From),
                    b"to" if current.is_some() => marker = Some(DrawingMarker::To),
                    b"row" if current.is_some() => field = Some(DrawingMarkerField::Row),
                    b"col" if current.is_some() => field = Some(DrawingMarkerField::Col),
                    b"rowOff" if current.is_some() => field = Some(DrawingMarkerField::RowOffset),
                    b"colOff" if current.is_some() => field = Some(DrawingMarkerField::ColOffset),
                    b"blip" if current.is_some() => {
                        let item = current.as_mut().expect("drawing");
                        if item.rid.is_none() {
                            item.rid = xml_attr(&e, b"embed");
                            item.kind = XlsbDrawingKind::Image;
                        }
                    }
                    b"chart" if current.is_some() => {
                        let item = current.as_mut().expect("drawing");
                        if item.rid.is_none() {
                            item.rid = xml_attr(&e, b"id");
                            item.kind = XlsbDrawingKind::Chart;
                        }
                    }
                    b"cNvPr" if current.is_some() => {
                        let item = current.as_mut().expect("drawing");
                        item.metadata.name = xml_attr(&e, b"name")
                            .filter(|value| value.len() <= MAX_XLSB_DRAWING_TEXT);
                        item.metadata.alt_text = xml_attr(&e, b"descr")
                            .filter(|value| value.len() <= MAX_XLSB_DRAWING_TEXT);
                    }
                    b"xfrm" if current.is_some() => {
                        current.as_mut().expect("drawing").metadata.rotation_mdeg =
                            xml_attr(&e, b"rot")
                                .and_then(|value| value.parse::<i32>().ok())
                                .map(|value| value / 60);
                    }
                    b"ext"
                        if current.as_ref().is_some_and(|item| {
                            item.metadata.behavior != DrawingAnchorBehavior::MoveAndSize
                        }) =>
                    {
                        let width = xml_attr(&e, b"cx").and_then(|value| value.parse::<u64>().ok());
                        let height =
                            xml_attr(&e, b"cy").and_then(|value| value.parse::<u64>().ok());
                        let item = current.as_mut().expect("drawing");
                        if item.metadata.absolute_size_emu.is_none() {
                            item.metadata.absolute_size_emu = width.zip(height);
                        }
                    }
                    b"pos" if current.is_some() => {
                        let x = xml_attr(&e, b"x").and_then(|value| value.parse::<i64>().ok());
                        let y = xml_attr(&e, b"y").and_then(|value| value.parse::<i64>().ok());
                        current.as_mut().expect("drawing").metadata.from_offset_emu = x.zip(y);
                    }
                    b"srcRect" if current.is_some() => {
                        let edge = |name| {
                            xml_attr(&e, name)
                                .and_then(|value| value.parse::<u32>().ok())
                                .map(|value| value.saturating_mul(10).min(1_000_000))
                                .unwrap_or(0)
                        };
                        current.as_mut().expect("drawing").metadata.crop = Some(DrawingCrop {
                            left_ppm: edge(b"l"),
                            top_ppm: edge(b"t"),
                            right_ppm: edge(b"r"),
                            bottom_ppm: edge(b"b"),
                        });
                    }
                    b"desc" if current.is_some() => {
                        desc_depth = 1;
                        desc_text.clear();
                    }
                    _ if desc_depth > 0 => desc_depth += 1,
                    _ => {}
                }
                if field.is_some() {
                    field_text.clear();
                }
            }
            Ok(Event::Empty(e)) if current.is_some() => match xml_local(e.name().as_ref()) {
                b"blip" => {
                    let item = current.as_mut().expect("drawing");
                    if item.rid.is_none() {
                        item.rid = xml_attr(&e, b"embed");
                        item.kind = XlsbDrawingKind::Image;
                    }
                }
                b"chart" => {
                    let item = current.as_mut().expect("drawing");
                    if item.rid.is_none() {
                        item.rid = xml_attr(&e, b"id");
                        item.kind = XlsbDrawingKind::Chart;
                    }
                }
                b"cNvPr" => {
                    let item = current.as_mut().expect("drawing");
                    item.metadata.name =
                        xml_attr(&e, b"name").filter(|value| value.len() <= MAX_XLSB_DRAWING_TEXT);
                    item.metadata.alt_text =
                        xml_attr(&e, b"descr").filter(|value| value.len() <= MAX_XLSB_DRAWING_TEXT);
                }
                b"ext"
                    if current.as_ref().is_some_and(|item| {
                        item.metadata.behavior != DrawingAnchorBehavior::MoveAndSize
                    }) =>
                {
                    let width = xml_attr(&e, b"cx").and_then(|value| value.parse::<u64>().ok());
                    let height = xml_attr(&e, b"cy").and_then(|value| value.parse::<u64>().ok());
                    let item = current.as_mut().expect("drawing");
                    if item.metadata.absolute_size_emu.is_none() {
                        item.metadata.absolute_size_emu = width.zip(height);
                    }
                }
                b"pos" => {
                    let x = xml_attr(&e, b"x").and_then(|value| value.parse::<i64>().ok());
                    let y = xml_attr(&e, b"y").and_then(|value| value.parse::<i64>().ok());
                    current.as_mut().expect("drawing").metadata.from_offset_emu = x.zip(y);
                }
                b"srcRect" => {
                    let edge = |name| {
                        xml_attr(&e, name)
                            .and_then(|value| value.parse::<u32>().ok())
                            .map(|value| value.saturating_mul(10).min(1_000_000))
                            .unwrap_or(0)
                    };
                    current.as_mut().expect("drawing").metadata.crop = Some(DrawingCrop {
                        left_ppm: edge(b"l"),
                        top_ppm: edge(b"t"),
                        right_ppm: edge(b"r"),
                        bottom_ppm: edge(b"b"),
                    });
                }
                _ => {}
            },
            Ok(Event::Text(text)) if field.is_some() => {
                field_text.push_str(&drawing_text(&text));
            }
            Ok(Event::Text(text)) if desc_depth > 0 && desc_text.len() < MAX_XLSB_DRAWING_TEXT => {
                desc_text.push_str(&drawing_text(&text));
                truncate_drawing_text(&mut desc_text);
            }
            Ok(Event::GeneralRef(reference)) if field.is_some() => {
                append_drawing_ref(&mut field_text, &reference);
            }
            Ok(Event::GeneralRef(reference)) if desc_depth > 0 => {
                append_drawing_ref(&mut desc_text, &reference);
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                let local = xml_local(name.as_ref());
                match local {
                    b"row" | b"col" | b"rowOff" | b"colOff" if current.is_some() => {
                        if let (Some(marker), Some(field), Ok(value)) =
                            (marker, field, field_text.trim().parse::<i64>())
                        {
                            let item = current.as_mut().expect("drawing");
                            match (marker, field) {
                                (DrawingMarker::From, DrawingMarkerField::Row) => {
                                    item.from.0 = value.max(0).min(i64::from(MAX_ROW)) as u32;
                                    from_row_seen = true;
                                }
                                (DrawingMarker::From, DrawingMarkerField::Col) => {
                                    item.from.1 = value.max(0).min(i64::from(MAX_COL)) as u16;
                                    from_col_seen = true;
                                }
                                (DrawingMarker::To, DrawingMarkerField::Row) => {
                                    item.to.get_or_insert((0, 0)).0 =
                                        value.max(0).min(i64::from(MAX_ROW)) as u32;
                                    to_row_seen = true;
                                }
                                (DrawingMarker::To, DrawingMarkerField::Col) => {
                                    item.to.get_or_insert((0, 0)).1 =
                                        value.max(0).min(i64::from(MAX_COL)) as u16;
                                    to_col_seen = true;
                                }
                                (DrawingMarker::From, DrawingMarkerField::RowOffset) => {
                                    from_offset.1 = value;
                                    from_offset_seen = true;
                                }
                                (DrawingMarker::From, DrawingMarkerField::ColOffset) => {
                                    from_offset.0 = value;
                                    from_offset_seen = true;
                                }
                                (DrawingMarker::To, DrawingMarkerField::RowOffset) => {
                                    to_offset.1 = value;
                                    to_offset_seen = true;
                                }
                                (DrawingMarker::To, DrawingMarkerField::ColOffset) => {
                                    to_offset.0 = value;
                                    to_offset_seen = true;
                                }
                            }
                        }
                        field = None;
                        field_text.clear();
                    }
                    b"from" | b"to" => marker = None,
                    b"desc" if desc_depth > 0 => {
                        if !desc_text.trim().is_empty() {
                            current.as_mut().expect("drawing").metadata.alt_text =
                                Some(desc_text.trim().to_string());
                        }
                        desc_depth = 0;
                    }
                    _ if desc_depth > 0 => desc_depth -= 1,
                    b"twoCellAnchor" | b"oneCellAnchor" | b"absoluteAnchor" => {
                        if let Some(mut item) = current.take() {
                            if item.metadata.from_offset_emu.is_none() && from_offset_seen {
                                item.metadata.from_offset_emu = Some(from_offset);
                            }
                            if to_offset_seen {
                                item.metadata.to_offset_emu = Some(to_offset);
                            }
                            if from_row_seen && from_col_seen {
                                item.metadata.from_cell = Some(item.from);
                            }
                            if to_row_seen && to_col_seen {
                                item.metadata.to_cell = item.to;
                            }
                            out.push(item);
                        }
                        marker = None;
                        field = None;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

fn xlsb_image_format(path: &str) -> Option<ImageFmt> {
    match path.rsplit('.').next()?.to_ascii_lowercase().as_str() {
        "png" => Some(ImageFmt::Png),
        "jpg" | "jpeg" => Some(ImageFmt::Jpeg),
        _ => None,
    }
}

fn part_bytes_limited(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    name: &str,
    max: u64,
) -> Option<Vec<u8>> {
    let index = part_index(zip, name)?;
    let file = zip.by_index(index).ok()?;
    if file.size() > max {
        return None;
    }
    let mut out = Vec::with_capacity(usize::try_from(file.size()).ok()?);
    file.take(max.saturating_add(1))
        .read_to_end(&mut out)
        .ok()?;
    (u64::try_from(out.len()).ok()? <= max).then_some(out)
}

fn part_declared_size(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    name: &str,
) -> Option<u64> {
    let index = part_index(zip, name)?;
    zip.by_index(index).ok().map(|file| file.size())
}

type DrawingReadResult = (Vec<Image>, Vec<Chart>, Vec<DrawingMetadata>, Vec<StyleLoss>);

fn retain_xlsb_unrepresented_drawing(
    mut sidecar: DrawingMetadata,
    metadata: &mut Vec<DrawingMetadata>,
) {
    sidecar.kind = DrawingObjectKind::Shape;
    sidecar.object_index = 0;
    metadata.push(sidecar);
}

pub(super) fn read_sheet_drawings(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    sheet_path: &str,
    sheet_rels_xml: &str,
    theme: &XlsbTheme,
    chart_budget: &mut crate::xlsx::ChartImportBudget,
) -> DrawingReadResult {
    const MAX_IMAGE_PART: u64 = 64 << 20;
    const MAX_IMAGE_TOTAL: usize = 256 << 20;
    let target = match drawing_relationship_target(sheet_rels_xml) {
        crate::xlsx::RelationshipTarget::Internal(target) => target,
        crate::xlsx::RelationshipTarget::Missing => {
            return (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        }
        crate::xlsx::RelationshipTarget::Invalid => {
            return (
                Vec::new(),
                Vec::new(),
                Vec::new(),
                vec![StyleLoss {
                    kind: StyleLossKind::DrawingMetadataPartial,
                    occurrences: 1,
                }],
            );
        }
    };
    let drawing_path = normalize_part_target(sheet_path, &target);
    let Some(drawing_xml) = crate::xlsx::part_str(zip, &drawing_path) else {
        return (
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![StyleLoss {
                kind: StyleLossKind::DrawingMetadataPartial,
                occurrences: 1,
            }],
        );
    };
    let refs = parse_xlsb_drawing_refs(&drawing_xml);
    let rels_xml = crate::xlsx::part_str(zip, &sheet_rels_path(&drawing_path)).unwrap_or_default();
    let rels = crate::xlsx::parse_ooxml_relationships(&rels_xml);
    let mut images = Vec::new();
    let mut charts = Vec::new();
    let mut metadata = Vec::new();
    let mut losses = Vec::new();
    let mut image_bytes = 0usize;
    let chart_theme = crate::xlsx::chart_theme(
        [
            theme.colors[1],
            theme.colors[0],
            theme.colors[3],
            theme.colors[2],
            theme.colors[4],
            theme.colors[5],
            theme.colors[6],
            theme.colors[7],
            theme.colors[8],
            theme.colors[9],
            theme.colors[10],
            theme.colors[11],
        ],
        theme.major_latin_font_family.as_deref(),
        theme.minor_latin_font_family.as_deref(),
        theme.source_valid,
    );
    for drawing in refs {
        match drawing.kind {
            XlsbDrawingKind::Image => {
                let target = match drawing.rid.as_deref().map_or(
                    crate::xlsx::RelationshipTarget::Missing,
                    |rid| {
                        rels.as_deref().map_or(
                            crate::xlsx::RelationshipTarget::Invalid,
                            |relationships| {
                                crate::xlsx::internal_relationship_target_by_id(
                                    relationships,
                                    rid,
                                    "image",
                                )
                            },
                        )
                    },
                ) {
                    crate::xlsx::RelationshipTarget::Internal(target) => target,
                    crate::xlsx::RelationshipTarget::Missing
                    | crate::xlsx::RelationshipTarget::Invalid => {
                        retain_xlsb_unrepresented_drawing(drawing.metadata, &mut metadata);
                        add_style_loss(&mut losses, StyleLossKind::DrawingMetadataPartial, 1);
                        continue;
                    }
                };
                let path = normalize_part_target(&drawing_path, &target);
                let Some(format) = xlsb_image_format(&path) else {
                    retain_xlsb_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_style_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
                    continue;
                };
                let Some(data) = part_bytes_limited(zip, &path, MAX_IMAGE_PART) else {
                    retain_xlsb_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_style_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                };
                if image_bytes.saturating_add(data.len()) > MAX_IMAGE_TOTAL {
                    retain_xlsb_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_style_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                }
                image_bytes += data.len();
                let index = images.len();
                images.push(Image {
                    data,
                    format,
                    from: drawing.from,
                    to: drawing.to,
                });
                let mut sidecar = drawing.metadata;
                sidecar.kind = DrawingObjectKind::Image;
                sidecar.object_index = index;
                metadata.push(sidecar);
            }
            XlsbDrawingKind::Chart => {
                let target = match drawing.rid.as_deref().map_or(
                    crate::xlsx::RelationshipTarget::Missing,
                    |rid| {
                        rels.as_deref().map_or(
                            crate::xlsx::RelationshipTarget::Invalid,
                            |relationships| {
                                crate::xlsx::internal_relationship_target_by_id(
                                    relationships,
                                    rid,
                                    "chart",
                                )
                            },
                        )
                    },
                ) {
                    crate::xlsx::RelationshipTarget::Internal(target) => target,
                    crate::xlsx::RelationshipTarget::Missing
                    | crate::xlsx::RelationshipTarget::Invalid => {
                        retain_xlsb_unrepresented_drawing(drawing.metadata, &mut metadata);
                        add_style_loss(&mut losses, StyleLossKind::DrawingMetadataPartial, 1);
                        continue;
                    }
                };
                if !chart_budget.reserve_chart() {
                    retain_xlsb_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_style_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                }
                let path = normalize_part_target(&drawing_path, &target);
                let Some(declared_size) = part_declared_size(zip, &path) else {
                    retain_xlsb_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_style_loss(&mut losses, StyleLossKind::DrawingMetadataPartial, 1);
                    continue;
                };
                let Some(declared_work) = usize::try_from(declared_size)
                    .ok()
                    .and_then(|size| size.checked_mul(crate::xlsx::XLSX_CHART_XML_SCAN_PASSES))
                else {
                    retain_xlsb_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_style_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                };
                if declared_size > crate::xlsx::MAX_XLSX_CHART_XML_BYTES
                    || !chart_budget.reserve_xml_work(declared_work)
                {
                    retain_xlsb_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_style_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                }
                let Some(chart_bytes) =
                    part_bytes_limited(zip, &path, crate::xlsx::MAX_XLSX_CHART_XML_BYTES)
                else {
                    retain_xlsb_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_style_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                };
                let Some(chart_work) = chart_bytes
                    .len()
                    .checked_mul(crate::xlsx::XLSX_CHART_XML_SCAN_PASSES)
                else {
                    retain_xlsb_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_style_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                };
                if !chart_budget.reconcile_xml_work(declared_work, chart_work) {
                    retain_xlsb_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_style_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                }
                let Ok(chart_xml) = String::from_utf8(chart_bytes) else {
                    retain_xlsb_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_style_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
                    continue;
                };
                if !crate::xml_reference_work_within_budget(&chart_xml) {
                    retain_xlsb_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_style_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                }
                let Some(parsed) = crate::xlsx::parse_chart_with_theme(
                    &chart_xml,
                    drawing.from,
                    drawing.to.unwrap_or(drawing.from),
                    &mut chart_budget.cache_points_remaining,
                    &mut chart_budget.series_remaining,
                    &chart_theme,
                ) else {
                    retain_xlsb_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_style_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
                    continue;
                };
                let has_unsupported_chart_content = !parsed.unsupported_reasons.is_empty()
                    || !parsed.frame_style_losses.is_empty()
                    || parsed
                        .series_styles
                        .iter()
                        .any(|style| !style.losses.is_empty());
                let index = charts.len();
                charts.push(parsed.chart);
                let mut sidecar = drawing.metadata;
                sidecar.kind = DrawingObjectKind::Chart;
                sidecar.object_index = index;
                sidecar.chart_palette = theme.chart_palette();
                sidecar.chart_default_latin_font_family =
                    Some(theme.chart_default_latin_font_family().to_string());
                sidecar.chart_text_styles = parsed.text_styles;
                sidecar.chart_series_caches = parsed.series_caches;
                sidecar.chart_series_styles = parsed.series_styles;
                sidecar.chart_frame_fill = parsed.frame_fill;
                sidecar.chart_frame_style_losses = parsed.frame_style_losses;
                sidecar.chart_category_major_gridlines = Some(parsed.category_major_gridlines);
                sidecar.chart_value_major_gridlines = Some(parsed.value_major_gridlines);
                sidecar.chart_category_axis_visible = parsed.category_axis_visible;
                sidecar.chart_category_axis_shifted = parsed.category_axis_shifted;
                sidecar.chart_value_axis_visible = parsed.value_axis_visible;
                sidecar.chart_unsupported_reasons = parsed.unsupported_reasons;
                sidecar.chart_bar_direction = parsed.bar_direction;
                metadata.push(sidecar);
                if parsed.limit_exceeded {
                    add_style_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                }
                if has_unsupported_chart_content {
                    add_style_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
                }
            }
            XlsbDrawingKind::Shape => {
                let mut sidecar = drawing.metadata;
                sidecar.kind = DrawingObjectKind::Shape;
                sidecar.object_index = 0;
                metadata.push(sidecar);
                add_style_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
            }
        }
    }
    (images, charts, metadata, losses)
}
