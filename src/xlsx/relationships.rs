use std::collections::{BTreeSet, HashMap};

use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};

use super::{canonical_part_name, local, qualified_prefix};

pub(super) const OOXML_RELATIONSHIPS_NAMESPACE_TRANSITIONAL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
pub(super) const OOXML_RELATIONSHIPS_NAMESPACE_STRICT: &str =
    "http://purl.oclc.org/ooxml/officeDocument/relationships";
const OOXML_PACKAGE_RELATIONSHIPS_NAMESPACE_TRANSITIONAL: &str =
    "http://schemas.openxmlformats.org/package/2006/relationships";
const OOXML_PACKAGE_RELATIONSHIPS_NAMESPACE_STRICT: &str =
    "http://purl.oclc.org/ooxml/package/relationships";

/// Resolve an OPC internal relationship target to a package part name.
///
/// Relationship targets are URI references, not raw ZIP paths. Package-part
/// lookup therefore uses only the URI path component (a query or fragment
/// identifies content within the resolved part), rejects absolute/network URI
/// references, and resolves dot segments against the source part's directory.
/// Backslashes remain accepted as a compatibility extension because package
/// lookup elsewhere in rxls already canonicalizes them.
pub(crate) fn resolve_internal_relationship_part(base: &str, target: &str) -> Option<String> {
    let base = base.replace('\\', "/");
    let target = target.replace('\\', "/");
    let path_end = target.find(['?', '#']).unwrap_or(target.len());
    let path = &target[..path_end];

    // RFC 3986 relative references cannot contain a scheme, and a network-path
    // reference (`//authority/path`) is likewise not an OPC package-part name.
    if path.starts_with("//") || has_uri_scheme(path) {
        return None;
    }

    // A fragment-only or query-only reference denotes the source part itself.
    if path.is_empty() {
        return (!base.is_empty()).then(|| canonical_part_name(&base));
    }

    let mut parts: Vec<&str> = if path.starts_with('/') {
        Vec::new()
    } else {
        base.rsplit_once('/')
            .map(|(dir, _)| {
                dir.split('/')
                    .filter(|segment| !segment.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    };
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(segment),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn has_uri_scheme(path: &str) -> bool {
    let Some(colon) = path.find(':') else {
        return false;
    };
    let candidate = &path[..colon];
    !candidate.is_empty()
        && candidate.as_bytes()[0].is_ascii_alphabetic()
        && candidate
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RelationshipTarget {
    Missing,
    Internal(String),
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OoxmlRelationship {
    pub(crate) id: String,
    pub(crate) rel_type: Option<String>,
    pub(crate) target: String,
    pub(crate) external: bool,
}

pub(crate) fn relationship_type_matches(value: &str, rel_kind: &str) -> bool {
    [
        OOXML_RELATIONSHIPS_NAMESPACE_TRANSITIONAL,
        OOXML_RELATIONSHIPS_NAMESPACE_STRICT,
    ]
    .into_iter()
    .any(|namespace| {
        value
            .strip_prefix(namespace)
            .and_then(|suffix| suffix.strip_prefix('/'))
            == Some(rel_kind)
    })
}

struct RelationshipRootContext {
    qualified_name: Vec<u8>,
    namespace: Option<String>,
    namespaces: HashMap<Vec<u8>, String>,
}

fn relationship_root_context(
    element: &quick_xml::events::BytesStart<'_>,
    allow_extension_attributes: bool,
) -> Option<RelationshipRootContext> {
    if local(element.name().as_ref()) != b"Relationships" {
        return None;
    }
    let mut namespaces = HashMap::<Vec<u8>, String>::new();
    for attribute in element.attributes() {
        let attribute = attribute.ok()?;
        let qualified_name = attribute.key.as_ref();
        let prefix = if qualified_name == b"xmlns" {
            Vec::new()
        } else if let Some(prefix) = qualified_name.strip_prefix(b"xmlns:") {
            prefix.to_vec()
        } else if allow_extension_attributes {
            continue;
        } else {
            return None;
        };
        let value = attribute
            .decoded_and_normalized_value_with(
                XmlVersion::Implicit1_0,
                element.decoder(),
                1,
                quick_xml::escape::resolve_xml_entity,
            )
            .ok()?
            .into_owned();
        if namespaces.insert(prefix, value).is_some() {
            return None;
        }
    }
    let root_name = element.name().as_ref().to_vec();
    let prefix = qualified_prefix(&root_name).unwrap_or_default();
    let namespace = namespaces.get(prefix).cloned();
    if (!prefix.is_empty() && namespace.is_none())
        || namespace.as_deref().is_some_and(|namespace| {
            !matches!(
                namespace,
                OOXML_PACKAGE_RELATIONSHIPS_NAMESPACE_TRANSITIONAL
                    | OOXML_PACKAGE_RELATIONSHIPS_NAMESPACE_STRICT
            )
        })
    {
        return None;
    }
    Some(RelationshipRootContext {
        qualified_name: root_name,
        namespace,
        namespaces,
    })
}

/// Parse a package relationship part without applying last-entry-wins
/// semantics. Duplicate IDs, malformed structure, unknown target modes, and
/// foreign relationship namespaces invalidate the complete part. `Type`
/// remains optional here for compatibility with producer fixtures; selectors
/// that depend on a type require an exact Transitional or Strict URI below.
/// Unmodeled producer attributes are ignored, but they cannot substitute for
/// or override the required unqualified `Id` and `Target` attributes.
pub(crate) fn parse_ooxml_relationships(xml: &str) -> Option<Vec<OoxmlRelationship>> {
    parse_ooxml_relationships_with_policy(xml, true)
}

pub(crate) fn parse_ooxml_relationships_preserving_extensions(
    xml: &str,
) -> Option<Vec<OoxmlRelationship>> {
    parse_ooxml_relationships_with_policy(xml, true)
}

fn parse_ooxml_relationships_with_policy(
    xml: &str,
    allow_extension_attributes: bool,
) -> Option<Vec<OoxmlRelationship>> {
    const MAX_RELATIONSHIPS: usize = 65_536;
    const MAX_RELATIONSHIP_FIELD_BYTES: usize = 4_096;

    if xml.trim().is_empty() || !crate::xml_reference_work_within_budget(xml) {
        return None;
    }

    let mut reader = Reader::from_str(xml);
    let mut ids = BTreeSet::new();
    let mut relationships = Vec::new();
    let mut root: Option<RelationshipRootContext> = None;
    let mut root_open = false;
    let mut root_closed = false;
    let mut open_relationship: Option<(Vec<u8>, OoxmlRelationship)> = None;
    loop {
        match reader.read_event() {
            Ok(Event::End(element)) if open_relationship.is_some() => {
                let (qualified_name, relationship) = open_relationship.take()?;
                if element.name().as_ref() != qualified_name.as_slice() {
                    return None;
                }
                if relationships.len() >= MAX_RELATIONSHIPS || !ids.insert(relationship.id.clone())
                {
                    return None;
                }
                relationships.push(relationship);
            }
            Ok(Event::Text(text))
                if open_relationship.is_some()
                    && text.as_ref().iter().all(u8::is_ascii_whitespace) => {}
            Ok(Event::PI(_) | Event::Comment(_)) if open_relationship.is_some() => {}
            Ok(_) if open_relationship.is_some() => return None,
            Ok(Event::Start(element)) if root.is_none() && !root_closed => {
                root = Some(relationship_root_context(
                    &element,
                    allow_extension_attributes,
                )?);
                root_open = true;
            }
            Ok(Event::Empty(element)) if root.is_none() && !root_closed => {
                root = Some(relationship_root_context(
                    &element,
                    allow_extension_attributes,
                )?);
                root_closed = true;
            }
            Ok(Event::Empty(element)) if root_open => {
                let root = root.as_ref()?;
                let relationship = parse_ooxml_relationship_element(
                    &element,
                    root,
                    allow_extension_attributes,
                    MAX_RELATIONSHIP_FIELD_BYTES,
                )?;
                if relationships.len() >= MAX_RELATIONSHIPS || !ids.insert(relationship.id.clone())
                {
                    return None;
                }
                relationships.push(relationship);
            }
            Ok(Event::Start(element)) if root_open => {
                let root = root.as_ref()?;
                let relationship = parse_ooxml_relationship_element(
                    &element,
                    root,
                    allow_extension_attributes,
                    MAX_RELATIONSHIP_FIELD_BYTES,
                )?;
                open_relationship = Some((element.name().as_ref().to_vec(), relationship));
            }
            Ok(Event::End(element)) if root_open => {
                let root = root.as_ref()?;
                if element.name().as_ref() != root.qualified_name.as_slice() {
                    return None;
                }
                root_open = false;
                root_closed = true;
            }
            Ok(Event::Text(text)) if text.as_ref().iter().all(u8::is_ascii_whitespace) => {}
            Ok(Event::Decl(_) | Event::PI(_) | Event::Comment(_)) => {}
            Ok(Event::Eof) => break,
            Err(_) | Ok(_) => return None,
        }
    }
    if root.is_none() || root_open || !root_closed || open_relationship.is_some() {
        None
    } else {
        Some(relationships)
    }
}

fn parse_ooxml_relationship_element(
    element: &quick_xml::events::BytesStart<'_>,
    root: &RelationshipRootContext,
    allow_extension_attributes: bool,
    max_field_bytes: usize,
) -> Option<OoxmlRelationship> {
    if local(element.name().as_ref()) != b"Relationship" {
        return None;
    }
    let element_name = element.name();
    let prefix = qualified_prefix(element_name.as_ref()).unwrap_or_default();
    let namespace = root.namespaces.get(prefix).map(String::as_str);
    if (!prefix.is_empty() && namespace.is_none()) || namespace != root.namespace.as_deref() {
        return None;
    }

    let mut id = None;
    let mut rel_type = None;
    let mut target = None;
    let mut target_mode = None;
    for attribute in element.attributes() {
        let attribute = attribute.ok()?;
        let slot = match attribute.key.as_ref() {
            b"Id" => Some(&mut id),
            b"Type" => Some(&mut rel_type),
            b"Target" => Some(&mut target),
            b"TargetMode" => Some(&mut target_mode),
            _ if allow_extension_attributes => None,
            _ => return None,
        };
        let Some(slot) = slot else {
            continue;
        };
        if slot.is_some() {
            return None;
        }
        let value = attribute
            .decoded_and_normalized_value_with(
                XmlVersion::Implicit1_0,
                element.decoder(),
                1,
                quick_xml::escape::resolve_xml_entity,
            )
            .ok()?
            .into_owned();
        if value.len() > max_field_bytes {
            return None;
        }
        *slot = Some(value);
    }

    let id = id.filter(|id| !id.is_empty())?;
    let target = target.filter(|target| !target.is_empty())?;
    let external = match target_mode.as_deref() {
        None | Some("Internal") => false,
        Some("External") => true,
        Some(_) => return None,
    };
    Some(OoxmlRelationship {
        id,
        rel_type,
        target,
        external,
    })
}

/// Select exactly one internal package relationship of `rel_kind` in source
/// order. Ambiguous, malformed, duplicate-ID, or external relationships are
/// rejected instead of inheriting `HashMap` iteration order or falling back to
/// a conventional part path.
pub(crate) fn unique_internal_relationship_target(xml: &str, rel_kind: &str) -> RelationshipTarget {
    if xml.trim().is_empty() {
        return RelationshipTarget::Missing;
    }
    let Some(relationships) = parse_ooxml_relationships(xml) else {
        return RelationshipTarget::Invalid;
    };
    let mut selected = None;
    for relationship in relationships {
        if relationship
            .rel_type
            .as_deref()
            .is_some_and(|value| relationship_type_matches(value, rel_kind))
            && (relationship.external || selected.replace(relationship.target).is_some())
        {
            return RelationshipTarget::Invalid;
        }
    }
    selected.map_or(RelationshipTarget::Missing, RelationshipTarget::Internal)
}

pub(crate) fn internal_relationship_target_by_id(
    relationships: &[OoxmlRelationship],
    id: &str,
    rel_kind: &str,
) -> RelationshipTarget {
    let Some(relationship) = relationships
        .iter()
        .find(|relationship| relationship.id == id)
    else {
        return RelationshipTarget::Missing;
    };
    if relationship.external
        || !relationship
            .rel_type
            .as_deref()
            .is_some_and(|value| relationship_type_matches(value, rel_kind))
    {
        RelationshipTarget::Invalid
    } else {
        RelationshipTarget::Internal(relationship.target.clone())
    }
}
