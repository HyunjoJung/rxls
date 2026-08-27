//! XLSX drawing anchors, images, shapes, and chart attachments.

use std::io::Read;

use quick_xml::events::{BytesRef, Event};
use quick_xml::Reader;

use super::chart::{
    parse_chart_with_theme, ChartImportBudget, MAX_XLSX_CHART_XML_BYTES, XLSX_CHART_XML_SCAN_PASSES,
};
use super::relationships::{
    internal_relationship_target_by_id, parse_ooxml_relationships,
    unique_internal_relationship_target, RelationshipTarget,
};
use super::{
    attr, local, normalize_part_target, part, part_index, sheet_rels_path, text_of, ThemeColors,
};
use crate::{
    Chart, DrawingAnchorBehavior, DrawingCrop, DrawingMetadata, DrawingObjectKind, Image, ImageFmt,
    StyleLoss, StyleLossKind,
};

pub(super) const MAX_XLSX_DRAWINGS: usize = 16_384;
pub(super) const MAX_XLSX_DRAWING_TEXT: usize = 4_096;
const MAX_XLSX_DRAWING_NUMBER_TEXT: usize = 128;
pub(super) struct DrawingRef {
    kind: DrawingRefKind,
    pub(super) rid: Option<String>,
    pub(super) from: (u32, u16),
    pub(super) to: Option<(u32, u16)>,
    pub(super) metadata: DrawingMetadata,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DrawingRefKind {
    Image,
    Chart,
    Shape,
}

#[derive(Clone, Copy)]
enum AnchorSection {
    From,
    To,
}

#[derive(Clone, Copy)]
enum AnchorField {
    Row,
    Col,
    RowOffset,
    ColOffset,
}

pub(super) fn add_drawing_loss(losses: &mut Vec<StyleLoss>, kind: StyleLossKind, occurrences: u32) {
    if occurrences == 0 {
        return;
    }
    if let Some(loss) = losses.iter_mut().find(|loss| loss.kind == kind) {
        loss.occurrences = loss.occurrences.saturating_add(occurrences);
    } else {
        losses.push(StyleLoss { kind, occurrences });
    }
}

fn truncate_drawing_text(value: &mut String, max: usize) -> bool {
    if value.len() <= max {
        return false;
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    true
}

fn bounded_drawing_attr(
    e: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
    losses: &mut Vec<StyleLoss>,
) -> Option<String> {
    attr(e, key).map(|mut value| {
        if truncate_drawing_text(&mut value, MAX_XLSX_DRAWING_TEXT) {
            add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
        }
        value
    })
}

fn append_bounded_drawing_ref(
    out: &mut String,
    reference: &BytesRef<'_>,
    max: usize,
    losses: &mut Vec<StyleLoss>,
) {
    if out.len() >= max {
        add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
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
    if truncate_drawing_text(out, max) {
        add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
    }
}

fn drawing_anchor_behavior(
    element: &[u8],
    e: &quick_xml::events::BytesStart<'_>,
) -> DrawingAnchorBehavior {
    match element {
        b"absoluteAnchor" => DrawingAnchorBehavior::Absolute,
        b"oneCellAnchor" => DrawingAnchorBehavior::MoveOnly,
        b"twoCellAnchor" => match attr(e, b"editAs").as_deref() {
            Some("absolute") => DrawingAnchorBehavior::Absolute,
            Some("oneCell") => DrawingAnchorBehavior::MoveOnly,
            _ => DrawingAnchorBehavior::MoveAndSize,
        },
        _ => DrawingAnchorBehavior::MoveAndSize,
    }
}

fn drawing_crop(e: &quick_xml::events::BytesStart<'_>) -> DrawingCrop {
    let edge = |name| {
        attr(e, name)
            .and_then(|value| value.parse::<u32>().ok())
            .map(|value| value.saturating_mul(10).min(1_000_000))
            .unwrap_or(0)
    };
    DrawingCrop {
        left_ppm: edge(b"l"),
        top_ppm: edge(b"t"),
        right_ppm: edge(b"r"),
        bottom_ppm: edge(b"b"),
    }
}

pub(super) fn parse_drawing_refs_bounded(
    xml: &str,
    losses: &mut Vec<StyleLoss>,
) -> Vec<DrawingRef> {
    const XLSX_MAX_ROW: i64 = 1_048_575;
    const XLSX_MAX_COL: i64 = 16_383;

    let mut r = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut current: Option<DrawingRef> = None;
    let mut anchor_depth = 0usize;
    let mut anchor_requires_from = false;
    let mut anchor_requires_to = false;
    let mut section: Option<AnchorSection> = None;
    let mut field: Option<AnchorField> = None;
    let mut field_text = String::new();
    let mut from_row_seen = false;
    let mut from_col_seen = false;
    let mut to_row_seen = false;
    let mut to_col_seen = false;
    let mut from_offset = (0i64, 0i64);
    let mut to_offset = (0i64, 0i64);
    let mut from_row_offset_seen = false;
    let mut from_col_offset_seen = false;
    let mut to_row_offset_seen = false;
    let mut to_col_offset_seen = false;
    let mut desc_depth = 0usize;
    let mut desc_text = String::new();

    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.name();
                let local_name = local(name.as_ref());
                if matches!(
                    local_name,
                    b"twoCellAnchor" | b"oneCellAnchor" | b"absoluteAnchor"
                ) {
                    if current.is_some() {
                        anchor_depth = anchor_depth.saturating_add(1);
                        add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        continue;
                    }
                    if out.len() >= MAX_XLSX_DRAWINGS {
                        add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
                        break;
                    }
                    anchor_depth = 1;
                    anchor_requires_from = local_name != b"absoluteAnchor";
                    anchor_requires_to = local_name == b"twoCellAnchor";
                    current = Some(DrawingRef {
                        kind: DrawingRefKind::Shape,
                        rid: None,
                        from: (0, 0),
                        to: None,
                        metadata: DrawingMetadata {
                            behavior: drawing_anchor_behavior(local_name, &e),
                            z_order: Some(out.len().min(i32::MAX as usize) as i32),
                            ..Default::default()
                        },
                    });
                    section = None;
                    field = None;
                    field_text.clear();
                    from_row_seen = false;
                    from_col_seen = false;
                    to_row_seen = false;
                    to_col_seen = false;
                    from_offset = (0, 0);
                    to_offset = (0, 0);
                    from_row_offset_seen = false;
                    from_col_offset_seen = false;
                    to_row_offset_seen = false;
                    to_col_offset_seen = false;
                    desc_depth = 0;
                    desc_text.clear();
                    continue;
                }
                if current.is_none() || anchor_depth > 1 {
                    continue;
                }
                match local_name {
                    b"from" => section = Some(AnchorSection::From),
                    b"to" => section = Some(AnchorSection::To),
                    b"row" => {
                        field = Some(AnchorField::Row);
                        field_text.clear();
                    }
                    b"col" => {
                        field = Some(AnchorField::Col);
                        field_text.clear();
                    }
                    b"rowOff" => {
                        field = Some(AnchorField::RowOffset);
                        field_text.clear();
                    }
                    b"colOff" => {
                        field = Some(AnchorField::ColOffset);
                        field_text.clear();
                    }
                    b"pic" => current.as_mut().expect("drawing").kind = DrawingRefKind::Image,
                    b"blip" => {
                        let item = current.as_mut().expect("drawing");
                        if item.kind == DrawingRefKind::Image && item.rid.is_none() {
                            item.rid = bounded_drawing_attr(&e, b"embed", losses);
                        }
                    }
                    b"chart" => {
                        let item = current.as_mut().expect("drawing");
                        item.kind = DrawingRefKind::Chart;
                        if item.rid.is_none() {
                            item.rid = bounded_drawing_attr(&e, b"id", losses);
                        }
                    }
                    b"cNvPr" => {
                        let item = current.as_mut().expect("drawing");
                        if item.metadata.name.is_none() {
                            item.metadata.name = bounded_drawing_attr(&e, b"name", losses);
                        }
                        if item.metadata.alt_text.is_none() {
                            item.metadata.alt_text = bounded_drawing_attr(&e, b"descr", losses)
                                .or_else(|| bounded_drawing_attr(&e, b"title", losses));
                        }
                    }
                    b"xfrm" => {
                        current.as_mut().expect("drawing").metadata.rotation_mdeg =
                            attr(&e, b"rot")
                                .and_then(|value| value.parse::<i32>().ok())
                                .map(|value| value / 60);
                    }
                    b"ext"
                        if !anchor_requires_to
                            || current.as_ref().is_some_and(|item| {
                                item.metadata.behavior != DrawingAnchorBehavior::MoveAndSize
                            }) =>
                    {
                        let width = attr(&e, b"cx").and_then(|value| value.parse::<u64>().ok());
                        let height = attr(&e, b"cy").and_then(|value| value.parse::<u64>().ok());
                        let item = current.as_mut().expect("drawing");
                        if item.metadata.absolute_size_emu.is_none() {
                            item.metadata.absolute_size_emu = width.zip(height);
                        }
                        if width.is_some() ^ height.is_some() {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                    }
                    b"pos" => {
                        let x = attr(&e, b"x").and_then(|value| value.parse::<i64>().ok());
                        let y = attr(&e, b"y").and_then(|value| value.parse::<i64>().ok());
                        current.as_mut().expect("drawing").metadata.from_offset_emu = x.zip(y);
                        if x.is_some() ^ y.is_some() {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                    }
                    b"srcRect" => {
                        current.as_mut().expect("drawing").metadata.crop = Some(drawing_crop(&e));
                    }
                    b"desc" => {
                        desc_depth = 1;
                        desc_text.clear();
                    }
                    _ if desc_depth > 0 => desc_depth += 1,
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) if current.is_some() && anchor_depth == 1 => {
                match local(e.name().as_ref()) {
                    b"pic" => current.as_mut().expect("drawing").kind = DrawingRefKind::Image,
                    b"blip" => {
                        let item = current.as_mut().expect("drawing");
                        if item.kind == DrawingRefKind::Image && item.rid.is_none() {
                            item.rid = bounded_drawing_attr(&e, b"embed", losses);
                        }
                    }
                    b"chart" => {
                        let item = current.as_mut().expect("drawing");
                        item.kind = DrawingRefKind::Chart;
                        if item.rid.is_none() {
                            item.rid = bounded_drawing_attr(&e, b"id", losses);
                        }
                    }
                    b"cNvPr" => {
                        let item = current.as_mut().expect("drawing");
                        if item.metadata.name.is_none() {
                            item.metadata.name = bounded_drawing_attr(&e, b"name", losses);
                        }
                        if item.metadata.alt_text.is_none() {
                            item.metadata.alt_text = bounded_drawing_attr(&e, b"descr", losses)
                                .or_else(|| bounded_drawing_attr(&e, b"title", losses));
                        }
                    }
                    b"xfrm" => {
                        current.as_mut().expect("drawing").metadata.rotation_mdeg =
                            attr(&e, b"rot")
                                .and_then(|value| value.parse::<i32>().ok())
                                .map(|value| value / 60);
                    }
                    b"ext"
                        if !anchor_requires_to
                            || current.as_ref().is_some_and(|item| {
                                item.metadata.behavior != DrawingAnchorBehavior::MoveAndSize
                            }) =>
                    {
                        let width = attr(&e, b"cx").and_then(|value| value.parse::<u64>().ok());
                        let height = attr(&e, b"cy").and_then(|value| value.parse::<u64>().ok());
                        let item = current.as_mut().expect("drawing");
                        if item.metadata.absolute_size_emu.is_none() {
                            item.metadata.absolute_size_emu = width.zip(height);
                        }
                        if width.is_some() ^ height.is_some() {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                    }
                    b"pos" => {
                        let x = attr(&e, b"x").and_then(|value| value.parse::<i64>().ok());
                        let y = attr(&e, b"y").and_then(|value| value.parse::<i64>().ok());
                        current.as_mut().expect("drawing").metadata.from_offset_emu = x.zip(y);
                        if x.is_some() ^ y.is_some() {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                    }
                    b"srcRect" => {
                        current.as_mut().expect("drawing").metadata.crop = Some(drawing_crop(&e));
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) if field.is_some() => {
                field_text.push_str(&text_of(&t));
                if truncate_drawing_text(&mut field_text, MAX_XLSX_DRAWING_NUMBER_TEXT) {
                    add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
                }
            }
            Ok(Event::Text(t)) if desc_depth > 0 => {
                desc_text.push_str(&text_of(&t));
                if truncate_drawing_text(&mut desc_text, MAX_XLSX_DRAWING_TEXT) {
                    add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
                }
            }
            Ok(Event::GeneralRef(reference)) if field.is_some() => {
                append_bounded_drawing_ref(
                    &mut field_text,
                    &reference,
                    MAX_XLSX_DRAWING_NUMBER_TEXT,
                    losses,
                );
            }
            Ok(Event::GeneralRef(reference)) if desc_depth > 0 => {
                append_bounded_drawing_ref(
                    &mut desc_text,
                    &reference,
                    MAX_XLSX_DRAWING_TEXT,
                    losses,
                );
            }
            Ok(Event::CData(t)) if field.is_some() => {
                field_text.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
                if truncate_drawing_text(&mut field_text, MAX_XLSX_DRAWING_NUMBER_TEXT) {
                    add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
                }
            }
            Ok(Event::CData(t)) if desc_depth > 0 => {
                desc_text.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
                if truncate_drawing_text(&mut desc_text, MAX_XLSX_DRAWING_TEXT) {
                    add_drawing_loss(losses, StyleLossKind::LimitExceeded, 1);
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                let local_name = local(name.as_ref());
                if matches!(
                    local_name,
                    b"twoCellAnchor" | b"oneCellAnchor" | b"absoluteAnchor"
                ) && current.is_some()
                {
                    if anchor_depth > 1 {
                        anchor_depth -= 1;
                        continue;
                    }
                    if let Some(mut item) = current.take() {
                        if from_row_offset_seen || from_col_offset_seen {
                            item.metadata.from_offset_emu = Some(from_offset);
                            if from_row_offset_seen ^ from_col_offset_seen {
                                add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                            }
                        }
                        if to_row_offset_seen || to_col_offset_seen {
                            item.metadata.to_offset_emu = Some(to_offset);
                            if to_row_offset_seen ^ to_col_offset_seen {
                                add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                            }
                        }
                        if anchor_requires_from && !(from_row_seen && from_col_seen) {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                        if anchor_requires_to && !(to_row_seen && to_col_seen) {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                        if from_row_seen && from_col_seen {
                            item.metadata.from_cell = Some(item.from);
                        }
                        if to_row_seen && to_col_seen {
                            item.metadata.to_cell = item.to;
                        }
                        out.push(item);
                    }
                    anchor_depth = 0;
                    section = None;
                    field = None;
                    desc_depth = 0;
                    continue;
                }
                if current.is_none() || anchor_depth > 1 {
                    continue;
                }
                match local_name {
                    b"row" | b"col" | b"rowOff" | b"colOff" => {
                        if let (Some(section), Some(field), Ok(value)) =
                            (section, field, field_text.trim().parse::<i64>())
                        {
                            let item = current.as_mut().expect("drawing");
                            match (section, field) {
                                (AnchorSection::From, AnchorField::Row) => {
                                    item.from.0 = value.clamp(0, XLSX_MAX_ROW) as u32;
                                    from_row_seen = true;
                                }
                                (AnchorSection::From, AnchorField::Col) => {
                                    item.from.1 = value.clamp(0, XLSX_MAX_COL) as u16;
                                    from_col_seen = true;
                                }
                                (AnchorSection::To, AnchorField::Row) => {
                                    item.to.get_or_insert((0, 0)).0 =
                                        value.clamp(0, XLSX_MAX_ROW) as u32;
                                    to_row_seen = true;
                                }
                                (AnchorSection::To, AnchorField::Col) => {
                                    item.to.get_or_insert((0, 0)).1 =
                                        value.clamp(0, XLSX_MAX_COL) as u16;
                                    to_col_seen = true;
                                }
                                (AnchorSection::From, AnchorField::RowOffset) => {
                                    from_offset.1 = value;
                                    from_row_offset_seen = true;
                                }
                                (AnchorSection::From, AnchorField::ColOffset) => {
                                    from_offset.0 = value;
                                    from_col_offset_seen = true;
                                }
                                (AnchorSection::To, AnchorField::RowOffset) => {
                                    to_offset.1 = value;
                                    to_row_offset_seen = true;
                                }
                                (AnchorSection::To, AnchorField::ColOffset) => {
                                    to_offset.0 = value;
                                    to_col_offset_seen = true;
                                }
                            }
                        } else if field.is_some() {
                            add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                        }
                        field = None;
                        field_text.clear();
                    }
                    b"from" | b"to" => section = None,
                    b"desc" if desc_depth > 0 => {
                        if current
                            .as_ref()
                            .expect("drawing")
                            .metadata
                            .alt_text
                            .is_none()
                            && !desc_text.trim().is_empty()
                        {
                            current.as_mut().expect("drawing").metadata.alt_text =
                                Some(desc_text.trim().to_string());
                        }
                        desc_depth = 0;
                    }
                    _ if desc_depth > 0 => desc_depth -= 1,
                    _ => {}
                }
            }
            Ok(Event::Eof) => {
                if current.is_some() {
                    add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                }
                break;
            }
            Err(_) => {
                add_drawing_loss(losses, StyleLossKind::DrawingMetadataPartial, 1);
                break;
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
pub(super) fn parse_drawing_refs(xml: &str) -> Vec<DrawingRef> {
    parse_drawing_refs_bounded(xml, &mut Vec::new())
}

fn image_format(path: &str) -> Option<ImageFmt> {
    match path
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some(ImageFmt::Png),
        Some("jpg" | "jpeg") => Some(ImageFmt::Jpeg),
        _ => None,
    }
}

enum DrawingPartRead {
    Missing,
    LimitExceeded,
    Data(Vec<u8>),
}

fn drawing_part_declared_size(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    path: &str,
) -> Option<u64> {
    let index = part_index(zip, path)?;
    zip.by_index(index).ok().map(|file| file.size())
}

fn drawing_part_bytes(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    path: &str,
    max: u64,
) -> DrawingPartRead {
    let Some(index) = part_index(zip, path) else {
        return DrawingPartRead::Missing;
    };
    let Ok(file) = zip.by_index(index) else {
        return DrawingPartRead::Missing;
    };
    if file.size() > max {
        return DrawingPartRead::LimitExceeded;
    }
    let mut data = Vec::new();
    if file
        .take(max.saturating_add(1))
        .read_to_end(&mut data)
        .is_err()
    {
        return DrawingPartRead::Missing;
    }
    if data.len() as u64 > max {
        DrawingPartRead::LimitExceeded
    } else {
        DrawingPartRead::Data(data)
    }
}

fn retain_unrepresented_drawing(mut sidecar: DrawingMetadata, metadata: &mut Vec<DrawingMetadata>) {
    sidecar.kind = DrawingObjectKind::Shape;
    sidecar.object_index = 0;
    metadata.push(sidecar);
}

type DrawingReadResult = (Vec<Image>, Vec<Chart>, Vec<DrawingMetadata>, Vec<StyleLoss>);

pub(super) fn read_sheet_drawings(
    zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    sheet_path: &str,
    sheet_rels_xml: Option<&str>,
    theme: &ThemeColors,
    chart_budget: &mut ChartImportBudget,
) -> DrawingReadResult {
    const MAX_IMAGE_PART: u64 = 64 << 20;
    const MAX_IMAGE_TOTAL: usize = 256 << 20;
    let drawing_target = match sheet_rels_xml
        .map(|xml| unique_internal_relationship_target(xml, "drawing"))
        .unwrap_or(RelationshipTarget::Missing)
    {
        RelationshipTarget::Internal(target) => target,
        RelationshipTarget::Missing => {
            return (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        }
        RelationshipTarget::Invalid => {
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
    let drawing_path = normalize_part_target(sheet_path, &drawing_target);
    let Some(drawing_xml) = part(zip, &drawing_path) else {
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
    let mut losses = Vec::new();
    let refs = parse_drawing_refs_bounded(&drawing_xml, &mut losses);
    let drawing_rels =
        part(zip, &sheet_rels_path(&drawing_path)).and_then(|xml| parse_ooxml_relationships(&xml));
    let mut images = Vec::new();
    let mut charts = Vec::new();
    let mut metadata = Vec::new();
    let mut image_bytes = 0usize;

    for drawing in refs {
        match drawing.kind {
            DrawingRefKind::Image => {
                let target =
                    match drawing
                        .rid
                        .as_deref()
                        .map_or(RelationshipTarget::Missing, |rid| {
                            drawing_rels.as_deref().map_or(
                                RelationshipTarget::Invalid,
                                |relationships| {
                                    internal_relationship_target_by_id(relationships, rid, "image")
                                },
                            )
                        }) {
                        RelationshipTarget::Internal(target) => target,
                        RelationshipTarget::Missing | RelationshipTarget::Invalid => {
                            retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                            add_drawing_loss(&mut losses, StyleLossKind::DrawingMetadataPartial, 1);
                            continue;
                        }
                    };
                let media_path = normalize_part_target(&drawing_path, &target);
                let Some(format) = image_format(&media_path) else {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
                    continue;
                };
                let data = match drawing_part_bytes(zip, &media_path, MAX_IMAGE_PART) {
                    DrawingPartRead::Data(data) => data,
                    DrawingPartRead::Missing => {
                        retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                        add_drawing_loss(&mut losses, StyleLossKind::DrawingMetadataPartial, 1);
                        continue;
                    }
                    DrawingPartRead::LimitExceeded => {
                        retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                        add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                        continue;
                    }
                };
                if image_bytes.saturating_add(data.len()) > MAX_IMAGE_TOTAL {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
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
            DrawingRefKind::Chart => {
                let target =
                    match drawing
                        .rid
                        .as_deref()
                        .map_or(RelationshipTarget::Missing, |rid| {
                            drawing_rels.as_deref().map_or(
                                RelationshipTarget::Invalid,
                                |relationships| {
                                    internal_relationship_target_by_id(relationships, rid, "chart")
                                },
                            )
                        }) {
                        RelationshipTarget::Internal(target) => target,
                        RelationshipTarget::Missing | RelationshipTarget::Invalid => {
                            retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                            add_drawing_loss(&mut losses, StyleLossKind::DrawingMetadataPartial, 1);
                            continue;
                        }
                    };
                if !chart_budget.reserve_chart() {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                }
                let chart_path = normalize_part_target(&drawing_path, &target);
                let Some(declared_size) = drawing_part_declared_size(zip, &chart_path) else {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::DrawingMetadataPartial, 1);
                    continue;
                };
                let Some(declared_work) = usize::try_from(declared_size)
                    .ok()
                    .and_then(|size| size.checked_mul(XLSX_CHART_XML_SCAN_PASSES))
                else {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                };
                if declared_size > MAX_XLSX_CHART_XML_BYTES
                    || !chart_budget.reserve_xml_work(declared_work)
                {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                }
                let chart_bytes =
                    match drawing_part_bytes(zip, &chart_path, MAX_XLSX_CHART_XML_BYTES) {
                        DrawingPartRead::Data(data) => data,
                        DrawingPartRead::Missing => {
                            retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                            add_drawing_loss(&mut losses, StyleLossKind::DrawingMetadataPartial, 1);
                            continue;
                        }
                        DrawingPartRead::LimitExceeded => {
                            retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                            add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                            continue;
                        }
                    };
                let Some(chart_work) = chart_bytes.len().checked_mul(XLSX_CHART_XML_SCAN_PASSES)
                else {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                };
                if !chart_budget.reconcile_xml_work(declared_work, chart_work) {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                }
                let Ok(chart_xml) = String::from_utf8(chart_bytes) else {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
                    continue;
                };
                if !crate::xml_reference_work_within_budget(&chart_xml) {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                    continue;
                }
                let Some(parsed) = parse_chart_with_theme(
                    &chart_xml,
                    drawing.from,
                    drawing.to.unwrap_or(drawing.from),
                    &mut chart_budget.cache_points_remaining,
                    &mut chart_budget.series_remaining,
                    theme,
                ) else {
                    retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                    add_drawing_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
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
                    add_drawing_loss(&mut losses, StyleLossKind::LimitExceeded, 1);
                }
                if has_unsupported_chart_content {
                    add_drawing_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
                }
            }
            DrawingRefKind::Shape => {
                retain_unrepresented_drawing(drawing.metadata, &mut metadata);
                add_drawing_loss(&mut losses, StyleLossKind::UnsupportedProperty, 1);
            }
        }
    }

    (images, charts, metadata, losses)
}
