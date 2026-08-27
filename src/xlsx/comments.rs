use quick_xml::events::Event;
use quick_xml::Reader;

use crate::Comment;

use super::refs::parse_ref;
use super::relationships::{unique_internal_relationship_target, RelationshipTarget};
use super::{append_general_ref, attr, local, text_of};

/// Find the comments part target in a worksheet relationship part.
pub(super) fn comments_target(xml: &str) -> Option<String> {
    match unique_internal_relationship_target(xml, "comments") {
        RelationshipTarget::Internal(target) => Some(target),
        RelationshipTarget::Missing | RelationshipTarget::Invalid => None,
    }
}

/// Parse an OOXML comments part into authoring comments.
pub(super) fn parse_comments(xml: &str) -> Vec<Comment> {
    let mut r = Reader::from_str(xml);
    let mut authors: Vec<String> = Vec::new();
    let mut out: Vec<Comment> = Vec::new();
    let mut in_authors = false;
    let mut in_author = false;
    let mut cur_author = String::new();
    let mut cur_rc: Option<(u32, u16)> = None;
    let mut cur_author_id: usize = 0;
    let mut cur_text = String::new();
    let mut in_t = false;
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match local(e.name().as_ref()) {
                b"authors" => in_authors = true,
                b"author" => {
                    in_author = true;
                    cur_author.clear();
                }
                b"comment" => {
                    cur_rc = attr(&e, b"ref").as_deref().and_then(parse_ref);
                    cur_author_id = attr(&e, b"authorId")
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    cur_text.clear();
                }
                b"t" if cur_rc.is_some() => in_t = true,
                _ => {}
            },
            Ok(Event::Text(t)) if in_author => cur_author.push_str(&text_of(&t)),
            Ok(Event::Text(t)) if in_t => cur_text.push_str(&text_of(&t)),
            Ok(Event::GeneralRef(reference)) if in_author => {
                append_general_ref(&mut cur_author, &reference);
            }
            Ok(Event::GeneralRef(reference)) if in_t => {
                append_general_ref(&mut cur_text, &reference);
            }
            Ok(Event::CData(t)) if in_t => {
                cur_text.push_str(&String::from_utf8_lossy(t.into_inner().as_ref()));
            }
            Ok(Event::End(e)) => match local(e.name().as_ref()) {
                b"authors" => in_authors = false,
                b"author" => {
                    if in_authors {
                        authors.push(std::mem::take(&mut cur_author));
                    }
                    in_author = false;
                }
                b"t" => in_t = false,
                b"comment" => {
                    if let Some((row, col)) = cur_rc.take() {
                        let author = authors
                            .get(cur_author_id)
                            .filter(|a| !a.is_empty())
                            .cloned();
                        out.push(Comment {
                            row,
                            col,
                            text: std::mem::take(&mut cur_text),
                            author,
                        });
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}
