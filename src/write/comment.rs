//! Legacy cell comments / notes: per sheet that has comments, the
//! `xl/comments{N}.xml` part (authors + comment list) and the matching
//! `xl/drawings/vmlDrawing{N}.vml` part (the legacy VML shapes Excel needs to
//! position the note pop-up). N is the 1-based sheet index.

use std::collections::BTreeMap;

use crate::write::xml::{a1, esc_text, XML_DECL};
use crate::write::{MAX_COL, MAX_ROW};
use crate::Comment;
#[cfg(test)]
use crate::Sheet;

/// VML namespaces for the legacy drawing part.
const NS_V: &str = "urn:schemas-microsoft-com:vml";
const NS_O: &str = "urn:schemas-microsoft-com:office:office";
const NS_X: &str = "urn:schemas-microsoft-com:office:excel";

/// The deduped, ordered author list for a sheet's comments. Returns the unique
/// author names (blank for `None`) plus a per-comment `authorId` index.
fn authors(comments: &[Comment]) -> (Vec<String>, Vec<usize>) {
    let mut names: Vec<String> = Vec::new();
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    let mut ids: Vec<usize> = Vec::with_capacity(comments.len());
    for c in comments {
        let name = c.author.clone().unwrap_or_default();
        let id = *index.entry(name.clone()).or_insert_with(|| {
            names.push(name);
            names.len() - 1
        });
        ids.push(id);
    }
    (names, ids)
}

/// `xl/comments{N}.xml` for a sheet with comments: the `<authors>` table plus a
/// `<commentList>` of `<comment ref=… authorId=…>` notes.
#[cfg(test)]
pub(super) fn comments_xml(sheet: &Sheet) -> String {
    comments_xml_for_comments(&sheet.comments)
}

pub(crate) fn comments_xml_for_comments(comments: &[Comment]) -> String {
    let (names, ids) = authors(comments);
    let mut s = String::new();
    s.push_str(XML_DECL);
    s.push_str(
        r#"<comments xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><authors>"#,
    );
    for name in &names {
        s.push_str(&format!("<author>{}</author>", esc_text(name)));
    }
    s.push_str("</authors><commentList>");
    for (c, &author_id) in comments.iter().zip(&ids) {
        let cref = a1(c.row.min(MAX_ROW), c.col.min(MAX_COL));
        s.push_str(&format!(
            r#"<comment ref="{cref}" authorId="{author_id}"><text><r><t xml:space="preserve">{}</t></r></text></comment>"#,
            esc_text(&c.text)
        ));
    }
    s.push_str("</commentList></comments>");
    s
}

/// `xl/drawings/vmlDrawing{N}.vml` for a sheet with comments: the standard
/// `<o:shapelayout>` / `<v:shapetype id="_x0000_t202">` preamble followed by one
/// `<v:shape type="#_x0000_t202">` per comment, each carrying the `<x:ClientData
/// ObjectType="Note">` anchor Excel reads to place the pop-up.
#[cfg(test)]
pub(super) fn vml_drawing_xml(sheet: &Sheet) -> String {
    vml_drawing_xml_for_comments(&sheet.comments)
}

pub(crate) fn vml_drawing_xml_for_comments(comments: &[Comment]) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        r#"<xml xmlns:v="{NS_V}" xmlns:o="{NS_O}" xmlns:x="{NS_X}">"#
    ));
    s.push_str(r#"<o:shapelayout v:ext="edit"><o:idmap v:ext="edit" data="1"/></o:shapelayout>"#);
    s.push_str(
        r#"<v:shapetype id="_x0000_t202" coordsize="21600,21600" o:spt="202" path="m,l,21600r21600,l21600,xe"><v:stroke joinstyle="miter"/><v:path gradientshapeok="t" o:connecttype="rect"/></v:shapetype>"#,
    );
    // Shape ids must be unique within the part; start past the shapetype.
    for (i, c) in comments.iter().enumerate() {
        let row = c.row.min(MAX_ROW);
        let col = c.col.min(MAX_COL);
        let shape_id = 1025 + i; // _x0000_s1025.. — Excel's conventional base
                                 // The anchor is a cell-box rectangle one column/row to the right/below the
                                 // commented cell (the classic note offset).
        let anchor = format!(
            "{lc}, 15, {lr}, 2, {rc}, 15, {rr}, 16",
            lc = col + 1,
            lr = row,
            rc = col + 3,
            rr = row + 4,
        );
        s.push_str(&format!(
            r##"<v:shape id="_x0000_s{shape_id}" type="#_x0000_t202" style="position:absolute;visibility:hidden" fillcolor="#ffffe1" o:insetmode="auto"><v:fill color2="#ffffe1"/><v:shadow on="t" color="black" obscured="t"/><v:path o:connecttype="none"/><v:textbox style="mso-direction-alt:auto"><div style="text-align:left"></div></v:textbox><x:ClientData ObjectType="Note"><x:MoveWithCells/><x:SizeWithCells/><x:Anchor>{anchor}</x:Anchor><x:AutoFill>False</x:AutoFill><x:Row>{row}</x:Row><x:Column>{col}</x:Column></x:ClientData></v:shape>"##,
        ));
    }
    s.push_str("</xml>");
    s
}
