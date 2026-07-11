//! Drawing parts: per-sheet images and charts (`xl/drawings/*`, `xl/charts/*`,
//! `xl/media/*`) and their content-type / media-extension contributions.

use crate::write::xml::{
    esc_text, CT_CHART, CT_DRAWING, NS_A, NS_C, NS_PKG_REL, NS_R, NS_XDR, REL_CHART, REL_IMAGE,
    XML_DECL,
};
use crate::write::{MAX_COL, MAX_ROW};
use crate::{Chart, ChartKind, ImageFmt, Series, Workbook};

/// Parts + content-type info contributed by a workbook's images and charts.
pub(super) struct Drawings {
    pub(super) parts: Vec<(String, Vec<u8>)>,
    pub(super) ct_overrides: Vec<(String, String)>,
    pub(super) sheet_has_drawing: Vec<bool>,
    pub(super) need_png: bool,
    pub(super) need_jpeg: bool,
    /// Whether any sheet has comments, so a `vml` content-type Default is emitted
    /// (set by `to_xlsx`, not `build_drawings`, since comments are a separate part).
    pub(super) need_vml: bool,
}

pub(super) fn build_drawings_with_budget(
    wb: &Workbook,
    sheet_count: usize,
    budget: &mut usize,
) -> Drawings {
    let mut d = Drawings {
        parts: Vec::new(),
        ct_overrides: Vec::new(),
        sheet_has_drawing: vec![false; sheet_count],
        need_png: false,
        need_jpeg: false,
        need_vml: false,
    };
    let (mut media_n, mut chart_n) = (0usize, 0usize);
    for (i, sheet) in wb.sheets.iter().take(sheet_count).enumerate() {
        if sheet.images.is_empty() && sheet.charts.is_empty() {
            continue;
        }
        let dn = i + 1;
        let mut sheet_parts: Vec<(String, Vec<u8>)> = Vec::new();
        let mut sheet_ct_overrides: Vec<(String, String)> = Vec::new();
        let mut sheet_need_png = false;
        let mut sheet_need_jpeg = false;
        let mut dxml = String::new();
        dxml.push_str(XML_DECL);
        dxml.push_str(&format!(
            r#"<xdr:wsDr xmlns:xdr="{NS_XDR}" xmlns:a="{NS_A}" xmlns:r="{NS_R}">"#
        ));
        let mut drels = String::new();
        let mut rid = 0usize;
        let mut stop_records = false;
        for img in &sheet.images {
            if stop_records || *budget == 0 {
                break;
            }
            let next_rid = rid + 1;
            let next_media_n = media_n + 1;
            let (ext, content_type_cost) = match img.format {
                ImageFmt::Png => (
                    "png",
                    if d.need_png || sheet_need_png {
                        0
                    } else {
                        super::content_type_default_cost("png", "image/png")
                    },
                ),
                ImageFmt::Jpeg => (
                    "jpeg",
                    if d.need_jpeg || sheet_need_jpeg {
                        0
                    } else {
                        super::content_type_default_cost("jpeg", "image/jpeg")
                    },
                ),
            };
            let (fr, fc) = img.from;
            let (tr, tc) = img
                .to
                .unwrap_or((fr.saturating_add(10), fc.saturating_add(4)));
            let anchor = pic_anchor(fr, fc, tr, tc, next_rid, next_media_n);
            let rel = format!(
                r#"<Relationship Id="rId{next_rid}" Type="{REL_IMAGE}" Target="../media/image{next_media_n}.{ext}"/>"#
            );
            let cost = img
                .data
                .len()
                .saturating_add(anchor.len())
                .saturating_add(rel.len())
                .saturating_add(content_type_cost);
            if cost > *budget {
                if rid == 0 {
                    *budget = 0;
                }
                stop_records = true;
                break;
            }
            *budget -= cost;
            rid = next_rid;
            media_n = next_media_n;
            match img.format {
                ImageFmt::Png => sheet_need_png = true,
                ImageFmt::Jpeg => sheet_need_jpeg = true,
            }
            sheet_parts.push((
                format!("xl/media/image{next_media_n}.{ext}"),
                img.data.clone(),
            ));
            dxml.push_str(&anchor);
            drels.push_str(&rel);
        }
        for chart in &sheet.charts {
            if stop_records || *budget == 0 {
                break;
            }
            let next_rid = rid + 1;
            let next_chart_n = chart_n + 1;
            let chart_body = chart_xml(chart);
            let (fr, fc) = chart.from;
            let (tr, tc) = chart.to;
            let anchor = graphic_frame_anchor(fr, fc, tr, tc, next_rid, next_chart_n);
            let rel = format!(
                r#"<Relationship Id="rId{next_rid}" Type="{REL_CHART}" Target="../charts/chart{next_chart_n}.xml"/>"#
            );
            let chart_part_name = format!("/xl/charts/chart{next_chart_n}.xml");
            let cost = chart_body
                .len()
                .saturating_add(anchor.len())
                .saturating_add(rel.len())
                .saturating_add(super::content_type_override_cost(
                    &chart_part_name,
                    CT_CHART,
                ));
            if cost > *budget {
                if rid == 0 {
                    *budget = 0;
                }
                break;
            }
            *budget -= cost;
            rid = next_rid;
            chart_n = next_chart_n;
            sheet_parts.push((
                format!("xl/charts/chart{next_chart_n}.xml"),
                chart_body.into_bytes(),
            ));
            sheet_ct_overrides.push((chart_part_name, CT_CHART.to_string()));
            dxml.push_str(&anchor);
            drels.push_str(&rel);
        }
        if rid == 0 {
            continue;
        }
        dxml.push_str("</xdr:wsDr>");
        let drels_xml =
            format!(r#"{XML_DECL}<Relationships xmlns="{NS_PKG_REL}">{drels}</Relationships>"#);
        let drawing_part_name = format!("/xl/drawings/drawing{dn}.xml");
        if !consume_budget(
            budget,
            dxml.len().saturating_add(drels_xml.len()).saturating_add(
                super::content_type_override_cost(&drawing_part_name, CT_DRAWING),
            ),
        ) {
            continue;
        }
        sheet_parts.push((format!("xl/drawings/drawing{dn}.xml"), dxml.into_bytes()));
        sheet_parts.push((
            format!("xl/drawings/_rels/drawing{dn}.xml.rels"),
            drels_xml.into_bytes(),
        ));
        sheet_ct_overrides.push((drawing_part_name, CT_DRAWING.to_string()));
        d.parts.extend(sheet_parts);
        d.ct_overrides.extend(sheet_ct_overrides);
        d.need_png |= sheet_need_png;
        d.need_jpeg |= sheet_need_jpeg;
        d.sheet_has_drawing[i] = true;
    }
    d
}

fn consume_budget(budget: &mut usize, cost: usize) -> bool {
    if cost > *budget {
        *budget = 0;
        return false;
    }
    *budget -= cost;
    true
}

/// The `<xdr:from>`/`<xdr:to>` cell-box anchor shared by pictures and charts.
fn anchor_from_to(fr: u32, fc: u16, tr: u32, tc: u16) -> String {
    format!(
        r#"<xdr:from><xdr:col>{fc}</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>{fr}</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from><xdr:to><xdr:col>{tc}</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>{tr}</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>"#,
        fc = fc.min(MAX_COL),
        fr = fr.min(MAX_ROW),
        tc = tc.min(MAX_COL),
        tr = tr.min(MAX_ROW),
    )
}

fn pic_anchor(fr: u32, fc: u16, tr: u32, tc: u16, rid: usize, n: usize) -> String {
    format!(
        r#"<xdr:twoCellAnchor editAs="oneCell">{ft}<xdr:pic><xdr:nvPicPr><xdr:cNvPr id="{id}" name="Image {n}"/><xdr:cNvPicPr/></xdr:nvPicPr><xdr:blipFill><a:blip r:embed="rId{rid}"/><a:stretch><a:fillRect/></a:stretch></xdr:blipFill><xdr:spPr><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></xdr:spPr></xdr:pic><xdr:clientData/></xdr:twoCellAnchor>"#,
        ft = anchor_from_to(fr, fc, tr, tc),
        id = rid + 1,
    )
}

fn graphic_frame_anchor(fr: u32, fc: u16, tr: u32, tc: u16, rid: usize, n: usize) -> String {
    format!(
        r#"<xdr:twoCellAnchor>{ft}<xdr:graphicFrame macro=""><xdr:nvGraphicFramePr><xdr:cNvPr id="{id}" name="Chart {n}"/><xdr:cNvGraphicFramePr/></xdr:nvGraphicFramePr><xdr:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/></xdr:xfrm><a:graphic><a:graphicData uri="{NS_C}"><c:chart xmlns:c="{NS_C}" xmlns:r="{NS_R}" r:id="rId{rid}"/></a:graphicData></a:graphic></xdr:graphicFrame><xdr:clientData/></xdr:twoCellAnchor>"#,
        ft = anchor_from_to(fr, fc, tr, tc),
        id = rid + 1,
    )
}

fn chart_series_xml(series: &[Series], scatter: bool, bubble: bool) -> String {
    let mut s = String::new();
    for (idx, ser) in series.iter().enumerate() {
        s.push_str(&format!(
            r#"<c:ser><c:idx val="{idx}"/><c:order val="{idx}"/>"#
        ));
        if let Some(name) = &ser.name {
            s.push_str(&format!(r#"<c:tx><c:v>{}</c:v></c:tx>"#, esc_text(name)));
        }
        if scatter {
            if let Some(cat) = &ser.categories {
                s.push_str(&format!(
                    r#"<c:xVal><c:numRef><c:f>{}</c:f></c:numRef></c:xVal>"#,
                    esc_text(cat)
                ));
            }
            s.push_str(&format!(
                r#"<c:yVal><c:numRef><c:f>{}</c:f></c:numRef></c:yVal>"#,
                esc_text(&ser.values)
            ));
            if bubble {
                if let Some(size) = &ser.bubble_sizes {
                    s.push_str(&format!(
                        r#"<c:bubbleSize><c:numRef><c:f>{}</c:f></c:numRef></c:bubbleSize>"#,
                        esc_text(size)
                    ));
                }
            }
        } else {
            if let Some(cat) = &ser.categories {
                s.push_str(&format!(
                    r#"<c:cat><c:strRef><c:f>{}</c:f></c:strRef></c:cat>"#,
                    esc_text(cat)
                ));
            }
            s.push_str(&format!(
                r#"<c:val><c:numRef><c:f>{}</c:f></c:numRef></c:val>"#,
                esc_text(&ser.values)
            ));
        }
        s.push_str("</c:ser>");
    }
    s
}

fn chart_xml(chart: &Chart) -> String {
    let title = match &chart.title {
        Some(t) => format!(
            r#"<c:title><c:tx><c:rich><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>{}</a:t></a:r></a:p></c:rich></c:tx><c:overlay val="0"/></c:title><c:autoTitleDeleted val="0"/>"#,
            esc_text(t)
        ),
        None => r#"<c:autoTitleDeleted val="1"/>"#.to_string(),
    };
    // An axis `<c:title>` (after `<c:axPos>`, per CT_CatAx/CT_ValAx order).
    let ax_title = |t: &Option<String>| match t {
        Some(s) => format!(
            r#"<c:title><c:tx><c:rich><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>{}</a:t></a:r></a:p></c:rich></c:tx><c:overlay val="0"/></c:title>"#,
            esc_text(s)
        ),
        None => String::new(),
    };
    let xt = ax_title(&chart.x_axis_title);
    let yt = ax_title(&chart.y_axis_title);
    let cat_val_axes = format!(
        r#"<c:catAx><c:axId val="1"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="b"/>{xt}<c:crossAx val="2"/></c:catAx><c:valAx><c:axId val="2"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="l"/>{yt}<c:crossAx val="1"/></c:valAx>"#
    );
    // Data labels (after the series, before axId / firstSliceAng per the schema).
    let dlbls = if chart.data_labels {
        r#"<c:dLbls><c:showLegendKey val="0"/><c:showVal val="1"/><c:showCatName val="0"/><c:showSerName val="0"/><c:showPercent val="0"/><c:showBubbleSize val="0"/></c:dLbls>"#
    } else {
        ""
    };
    let plot = match chart.kind {
        ChartKind::Bar => format!(
            r#"<c:barChart><c:barDir val="col"/><c:grouping val="clustered"/>{ser}{dlbls}<c:axId val="1"/><c:axId val="2"/></c:barChart>{cat_val_axes}"#,
            ser = chart_series_xml(&chart.series, false, false)
        ),
        ChartKind::Line => format!(
            r#"<c:lineChart><c:grouping val="standard"/>{ser}{dlbls}<c:axId val="1"/><c:axId val="2"/></c:lineChart>{cat_val_axes}"#,
            ser = chart_series_xml(&chart.series, false, false)
        ),
        ChartKind::Area => format!(
            r#"<c:areaChart><c:grouping val="standard"/>{ser}{dlbls}<c:axId val="1"/><c:axId val="2"/></c:areaChart>{cat_val_axes}"#,
            ser = chart_series_xml(&chart.series, false, false)
        ),
        ChartKind::Pie => format!(
            r#"<c:pieChart><c:varyColors val="1"/>{ser}{dlbls}</c:pieChart>"#,
            ser = chart_series_xml(&chart.series, false, false)
        ),
        ChartKind::Doughnut => format!(
            r#"<c:doughnutChart><c:varyColors val="1"/>{ser}{dlbls}<c:firstSliceAng val="0"/><c:holeSize val="50"/></c:doughnutChart>"#,
            ser = chart_series_xml(&chart.series, false, false)
        ),
        ChartKind::Scatter => format!(
            r#"<c:scatterChart><c:scatterStyle val="lineMarker"/>{ser}{dlbls}<c:axId val="1"/><c:axId val="2"/></c:scatterChart><c:valAx><c:axId val="1"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="b"/>{xt}<c:crossAx val="2"/></c:valAx><c:valAx><c:axId val="2"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="l"/>{yt}<c:crossAx val="1"/></c:valAx>"#,
            ser = chart_series_xml(&chart.series, true, false)
        ),
        ChartKind::Radar => format!(
            r#"<c:radarChart><c:radarStyle val="marker"/>{ser}{dlbls}<c:axId val="1"/><c:axId val="2"/></c:radarChart>{cat_val_axes}"#,
            ser = chart_series_xml(&chart.series, false, false)
        ),
        // Bubble charts use the scatter-style xVal/yVal series shape, with
        // optional explicit bubble sizes per series.
        ChartKind::Bubble => format!(
            r#"<c:bubbleChart>{ser}{dlbls}<c:axId val="1"/><c:axId val="2"/></c:bubbleChart><c:valAx><c:axId val="1"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="b"/>{xt}<c:crossAx val="2"/></c:valAx><c:valAx><c:axId val="2"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="l"/>{yt}<c:crossAx val="1"/></c:valAx>"#,
            ser = chart_series_xml(&chart.series, true, true)
        ),
    };
    let legend = if chart.legend {
        r#"<c:legend><c:legendPos val="r"/><c:overlay val="0"/></c:legend>"#
    } else {
        ""
    };
    format!(
        r#"{XML_DECL}<c:chartSpace xmlns:c="{NS_C}" xmlns:a="{NS_A}" xmlns:r="{NS_R}"><c:chart>{title}<c:plotArea><c:layout/>{plot}</c:plotArea>{legend}<c:plotVisOnly val="1"/></c:chart></c:chartSpace>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Chart, ChartKind, Image, ImageFmt, Series, Workbook};

    #[test]
    fn drawing_budget_counts_image_content_type_records() {
        let mut wb = Workbook::new();
        wb.add_sheet("viz").add_image(Image {
            data: vec![7],
            format: ImageFmt::Png,
            from: (0, 0),
            to: Some((1, 1)),
        });
        let img = &wb.sheets[0].images[0];
        let anchor = pic_anchor(0, 0, 1, 1, 1, 1);
        let rel =
            format!(r#"<Relationship Id="rId1" Type="{REL_IMAGE}" Target="../media/image1.png"/>"#);
        let dxml = format!(
            r#"{XML_DECL}<xdr:wsDr xmlns:xdr="{NS_XDR}" xmlns:a="{NS_A}" xmlns:r="{NS_R}">{anchor}</xdr:wsDr>"#
        );
        let drels_xml =
            format!(r#"{XML_DECL}<Relationships xmlns="{NS_PKG_REL}">{rel}</Relationships>"#);
        let old_cost = img
            .data
            .len()
            .saturating_add(anchor.len())
            .saturating_add(rel.len())
            .saturating_add(dxml.len())
            .saturating_add(drels_xml.len());

        let mut budget = old_cost;
        let drawings = build_drawings_with_budget(&wb, wb.sheets.len(), &mut budget);

        assert_eq!(budget, 0);
        assert!(
            drawings.parts.is_empty(),
            "image default and drawing content-type XML must consume output budget"
        );
        assert!(drawings.ct_overrides.is_empty());
        assert!(!drawings.need_png);
        assert!(!drawings.sheet_has_drawing[0]);
    }

    #[test]
    fn drawing_budget_counts_chart_content_type_records() {
        let chart = Chart {
            kind: ChartKind::Bar,
            title: None,
            series: vec![Series {
                name: None,
                categories: None,
                values: "viz!$A$1:$A$2".into(),
                bubble_sizes: None,
            }],
            legend: false,
            data_labels: false,
            x_axis_title: None,
            y_axis_title: None,
            from: (0, 0),
            to: (5, 5),
        };
        let chart_body = chart_xml(&chart);
        let anchor = graphic_frame_anchor(0, 0, 5, 5, 1, 1);
        let rel = format!(
            r#"<Relationship Id="rId1" Type="{REL_CHART}" Target="../charts/chart1.xml"/>"#
        );
        let dxml = format!(
            r#"{XML_DECL}<xdr:wsDr xmlns:xdr="{NS_XDR}" xmlns:a="{NS_A}" xmlns:r="{NS_R}">{anchor}</xdr:wsDr>"#
        );
        let drels_xml =
            format!(r#"{XML_DECL}<Relationships xmlns="{NS_PKG_REL}">{rel}</Relationships>"#);
        let old_cost = chart_body
            .len()
            .saturating_add(anchor.len())
            .saturating_add(rel.len())
            .saturating_add(dxml.len())
            .saturating_add(drels_xml.len());
        let mut wb = Workbook::new();
        wb.add_sheet("viz").add_chart(chart);

        let mut budget = old_cost;
        let drawings = build_drawings_with_budget(&wb, wb.sheets.len(), &mut budget);

        assert_eq!(budget, 0);
        assert!(
            drawings.parts.is_empty(),
            "chart and drawing content-type override XML must consume output budget"
        );
        assert!(drawings.ct_overrides.is_empty());
        assert!(!drawings.sheet_has_drawing[0]);
    }

    #[test]
    fn drawing_budget_counts_drawing_records_incrementally() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("viz");
        sheet.add_image(Image {
            data: vec![7],
            format: ImageFmt::Png,
            from: (0, 0),
            to: Some((1, 1)),
        });
        sheet.add_image(Image {
            data: vec![8; 1024],
            format: ImageFmt::Png,
            from: (2, 0),
            to: Some((3, 1)),
        });

        let anchor = pic_anchor(0, 0, 1, 1, 1, 1);
        let rel =
            format!(r#"<Relationship Id="rId1" Type="{REL_IMAGE}" Target="../media/image1.png"/>"#);
        let dxml = format!(
            r#"{XML_DECL}<xdr:wsDr xmlns:xdr="{NS_XDR}" xmlns:a="{NS_A}" xmlns:r="{NS_R}">{anchor}</xdr:wsDr>"#
        );
        let drels_xml =
            format!(r#"{XML_DECL}<Relationships xmlns="{NS_PKG_REL}">{rel}</Relationships>"#);
        let mut budget = 1usize
            .saturating_add(anchor.len())
            .saturating_add(rel.len())
            .saturating_add(super::super::content_type_default_cost("png", "image/png"))
            .saturating_add(dxml.len())
            .saturating_add(drels_xml.len())
            .saturating_add(super::super::content_type_override_cost(
                "/xl/drawings/drawing1.xml",
                CT_DRAWING,
            ));

        let drawings = build_drawings_with_budget(&wb, wb.sheets.len(), &mut budget);

        assert_eq!(budget, 0);
        assert!(drawings.sheet_has_drawing[0]);
        assert!(drawings.need_png);
        assert_eq!(drawings.ct_overrides.len(), 1);
        assert!(drawings
            .parts
            .iter()
            .any(|(path, bytes)| path == "xl/media/image1.png" && bytes == &[7]));
        assert!(
            !drawings
                .parts
                .iter()
                .any(|(path, _)| path == "xl/media/image2.png"),
            "the over-budget second image payload must be omitted"
        );
        let drawing_xml = drawings
            .parts
            .iter()
            .find_map(|(path, bytes)| {
                (path == "xl/drawings/drawing1.xml")
                    .then(|| String::from_utf8(bytes.clone()).expect("drawing xml"))
            })
            .expect("drawing xml part");
        assert!(
            drawing_xml.contains(r#"name="Image 1""#),
            "the first budgeted image anchor should remain"
        );
        assert!(
            !drawing_xml.contains(r#"name="Image 2""#),
            "the over-budget second image anchor should be omitted"
        );
    }
}
