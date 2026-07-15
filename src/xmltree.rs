//! A faithful, edit-preserving XML tree for package-preserving `.xlsx` editing.
//!
//! The `.xlsx` reader (`crate::xlsx`) projects each worksheet part into a *lossy*
//! [`crate::Sheet`]/[`crate::Cell`] view — perfect for reading, but it drops
//! everything an edit would need to put back untouched (unknown attributes,
//! extension lists, foreign namespaces, comments, processing instructions...).
//! This module is the opposite: a generic element tree that keeps **every**
//! element, attribute (in order), namespace declaration, comment, and processing
//! instruction from a part's XML. Editing mutates nodes in place; serialization
//! re-emits the same tree, so unmodeled siblings ride along untouched.
//!
//! Fidelity is *structural*, not byte-exact: element/attribute identity and
//! order, namespace declarations, and `Raw` markup (declaration/comment/PI/CDATA)
//! are preserved, but attribute values and text are stored unescaped and
//! re-emitted with **canonical** escaping (`&amp; &lt; &gt; &quot;`), so entity
//! spelling and quote style are normalized on any part that is re-serialized.
//! Crucially, only an *edited* part need ever be promoted through this tree —
//! an untouched part can stay raw bytes in the package and round-trip
//! byte-for-byte (that promotion/passthrough split is a later batch's concern;
//! this module only builds the tree itself).
//!
//! Design (Rust-idiomatic, no `Rc<RefCell>`): an **arena** of nodes addressed by
//! `Copy` [`NodeId`] handles. Edits go through `&mut XmlTree`, so there is no
//! aliasing/borrow tangle. There are deliberately no parent back-pointers — a
//! parent lookup, on the rare occasion one is needed, is a bounded DFS from
//! `roots` rather than an O(1) pointer chase; given [`MAX_DEPTH`] this is bounded
//! and it means the arena never needs `Weak`/parent-invalidation bookkeeping on
//! removal. Parsing is depth-bounded and panic-free on hostile input, matching
//! the rest of the crate.

use quick_xml::events::{BytesRef, Event};
use quick_xml::{Reader, XmlVersion};

use crate::error::{Error, Result};

/// Maximum element nesting accepted while parsing (stack-overflow / zip-bomb
/// guard). Deeper input is rejected, never crashes.
const MAX_DEPTH: usize = 256;

/// Maximum node count — a fast-fail ceiling that rejects absurd inputs early and
/// keeps the [`NodeId`] `u32` index safe. A worksheet part can legitimately need
/// on the order of this many nodes for a large, dense sheet, so setting it much
/// lower would reject valid large workbooks. The actual out-of-memory guard is
/// **fallible allocation** ([`Vec::try_reserve`] in [`XmlTree::push`]): a hostile
/// part that would exhaust memory returns an [`Error`] rather than aborting the
/// process, so this cap need not (and cannot, without rejecting valid input) be
/// the memory bound. Enforced at parse *and* re-checked on every fragment-insert
/// edit; [`XmlTree::set_element_text`] reuses a text slot rather than growing the
/// arena, so repeated text edits stay within budget too.
const MAX_NODES: usize = 8_000_000;

// A lowerable copy of the node budget for tests, so the over-budget path can be
// exercised without building an 8-million-node fixture. Production always uses
// `MAX_NODES`.
#[cfg(test)]
thread_local! {
    static TEST_NODE_BUDGET: std::cell::Cell<usize> = const { std::cell::Cell::new(MAX_NODES) };
}

/// Set the per-tree node budget for the current test thread.
#[cfg(test)]
pub(crate) fn set_test_node_budget(n: usize) {
    TEST_NODE_BUDGET.with(|c| c.set(n));
}

/// Restore the node budget to the production value (test threads are reused, so
/// a test that lowered it must reset to `MAX_NODES`, not leave it lowered).
#[cfg(test)]
pub(crate) fn reset_test_node_budget() {
    TEST_NODE_BUDGET.with(|c| c.set(MAX_NODES));
}

/// The effective node budget — `MAX_NODES` in production, the test override
/// under `cfg(test)`. A tree may hold at most this many nodes (parse and edits
/// agree on the boundary).
pub(crate) fn node_budget() -> usize {
    #[cfg(test)]
    {
        TEST_NODE_BUDGET.with(|c| c.get())
    }
    #[cfg(not(test))]
    {
        MAX_NODES
    }
}

// A test-only seam that forces the Nth *commit-time* tree edit
// (`set_element_text` / `insert_fragment_at`) to fail, simulating the
// genuine-but-hard-to-trigger `try_reserve` out-of-memory those paths guard
// against. It lets a future transactional clone-and-swap package layer be
// tested: an edit must leave the tree completely unchanged when a commit step
// fails. `set_test_fail_commit_after(k)` lets the first `k` commit edits
// succeed, then the next one fails (one-shot, self-disarming). Production never
// compiles this.
#[cfg(test)]
thread_local! {
    static FAIL_COMMIT_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Arm the commit-failure seam: succeed `n` more commit edits, then fail the
/// next one.
#[cfg(test)]
pub(crate) fn set_test_fail_commit_after(n: usize) {
    FAIL_COMMIT_AFTER.with(|c| c.set(Some(n)));
}

/// Disarm the commit-failure seam (test threads are reused).
#[cfg(test)]
pub(crate) fn reset_test_fail_commit() {
    FAIL_COMMIT_AFTER.with(|c| c.set(None));
}

/// Whether the current commit edit should fail (decrements the countdown; fires
/// once at 0).
#[cfg(test)]
fn commit_should_fail() -> bool {
    FAIL_COMMIT_AFTER.with(|c| match c.get() {
        None => false,
        Some(0) => {
            c.set(None);
            true
        }
        Some(n) => {
            c.set(Some(n - 1));
            false
        }
    })
}

/// A `Copy` handle into an [`XmlTree`]'s arena. Opaque outside this module — the
/// only way to obtain one is from an [`XmlTree`] method, so an id is always
/// valid for the tree it came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub(crate) struct NodeId(u32);

/// A single XML node. Element attribute values and text are stored
/// **unescaped** and canonically re-escaped on serialization; `Raw` markup
/// (declaration, comment, PI, CDATA, doctype) is kept verbatim.
#[derive(Debug, Clone)]
pub(crate) enum Node {
    /// An element: raw qualified name (e.g. `b"c"` or `b"r:id"` as an attribute
    /// key elsewhere), attributes in source order (raw key, unescaped value —
    /// `xmlns:*` declarations included), and whether it was written self-closing
    /// (`<x/>`).
    Element {
        name: Vec<u8>,
        attrs: Vec<(Vec<u8>, Vec<u8>)>,
        self_closing: bool,
    },
    /// Character data, stored unescaped.
    Text(Vec<u8>),
    /// Verbatim markup that is re-emitted exactly: `<?xml …?>`, `<!-- … -->`,
    /// `<?pi …?>`, `<![CDATA[ … ]]>`, `<!DOCTYPE …>`.
    Raw(Vec<u8>),
}

#[derive(Debug, Clone)]
struct NodeData {
    node: Node,
    children: Vec<NodeId>,
    /// Nesting depth of this node, 1-indexed from a root (a top-level root
    /// is depth `1`, its child is depth `2`, ...). Mirrors exactly what
    /// `parse()`'s own `stack.len()` (after push) would be for an element
    /// chain, so the same [`MAX_DEPTH`] bound applies uniformly whether a
    /// node arrived via `parse()` or was grafted in later by
    /// [`XmlTree::insert_fragment_at`]/[`XmlTree::graft`]. Computed once at
    /// push time (`parent.depth + 1`, or `1` for a root) so a caller can
    /// check a prospective graft's resulting depth in O(1) instead of
    /// walking up the tree on every edit.
    depth: usize,
}

/// A parsed XML document as an arena tree. Top-level nodes (declaration, root
/// element, trailing comments…) are held in `roots` in order.
#[derive(Debug, Clone)]
pub(crate) struct XmlTree {
    nodes: Vec<NodeData>,
    roots: Vec<NodeId>,
}

impl XmlTree {
    /// Parse XML bytes into a faithful tree. Depth-bounded and panic-free:
    /// malformed or hostile input yields an [`Error`], never a crash. Rejects
    /// rather than repairs: a mismatched end tag, truncated input, invalid
    /// UTF-8 in element text or an attribute value, a malformed entity
    /// reference, an XML-1.0-illegal character decoded from an entity, a
    /// second/misplaced XML declaration, or excess depth/nodes/attributes
    /// all return `Err`.
    ///
    /// Exception: content captured verbatim as [`Node::Raw`] — comments,
    /// processing instructions, CDATA, the doctype, the XML declaration, and
    /// prolog/epilog text outside the root element — is stored and
    /// re-emitted byte-for-byte exactly as read, **not** UTF-8-validated.
    /// This is consistent across every `Raw`-producing branch of this
    /// function (not a special case for any one of them): none of them
    /// unescape entities or interpret their payload as character data the
    /// way element `Text` does, so there is nothing here to canonicalize —
    /// verbatim passthrough is the correct, intentional behavior for all of
    /// them.
    pub(crate) fn parse(xml: &[u8]) -> Result<XmlTree> {
        let mut reader = Reader::from_reader(xml);
        let cfg = reader.config_mut();
        cfg.expand_empty_elements = false; // keep `<x/>` distinct from `<x></x>`
        cfg.check_end_names = true; // reject mismatched end tags rather than
                                    // silently "repairing" malformed input into
                                    // a different tree

        let mut tree = XmlTree {
            nodes: Vec::new(),
            roots: Vec::new(),
        };
        // Stack of open element ids (the current insertion path).
        let mut stack: Vec<NodeId> = Vec::new();
        let mut general_refs = 0usize;
        let mut buf = Vec::new();

        loop {
            // Bound total node count (a size-capped part can still hold millions
            // of tiny elements); keeps arena memory finite and the `NodeId` u32
            // safe. The boundary is `> budget` (not `>=`) so a tree with exactly
            // `node_budget()` nodes — the most an edit preflight will allow —
            // also re-parses cleanly.
            if tree.nodes.len() > node_budget() {
                return Err(Error::Xml("xml has too many nodes"));
            }
            let ev = reader
                .read_event_into(&mut buf)
                .map_err(|_| Error::Xml("malformed xml"))?;
            match ev {
                Event::Start(e) => {
                    if stack.len() >= MAX_DEPTH {
                        return Err(Error::Xml("xml nesting too deep"));
                    }
                    let node = element_node(&e, false)?;
                    let id = tree.push(node, stack.last().copied())?;
                    stack.push(id);
                }
                Event::Empty(e) => {
                    // A self-closing element occupies a level too: enforce the
                    // same depth cap as `Start` so an empty element can't sit
                    // one level past MAX_DEPTH.
                    if stack.len() >= MAX_DEPTH {
                        return Err(Error::Xml("xml nesting too deep"));
                    }
                    let node = element_node(&e, true)?;
                    tree.push(node, stack.last().copied())?;
                }
                Event::End(_) => {
                    stack.pop();
                }
                Event::Text(t) => {
                    let raw = t.into_inner();
                    if raw.is_empty() {
                        continue;
                    }
                    match stack.last().copied() {
                        // Element content: store unescaped (re-escaped on write).
                        Some(parent) => {
                            tree.push_text_fragment(parent, &unescape_bytes(&raw)?)?;
                        }
                        // Prolog/epilog text (e.g. the `\r\n` between the XML
                        // declaration and the root element) is whitespace where
                        // character references are NOT allowed — keep it
                        // verbatim.
                        None => {
                            tree.push(Node::Raw(raw.into_owned()), None)?;
                        }
                    }
                }
                Event::GeneralRef(reference) => {
                    general_refs += 1;
                    if general_refs > max_general_refs() {
                        return Err(Error::Xml("xml has too many entity references"));
                    }
                    match stack.last().copied() {
                        Some(parent) => {
                            push_resolved_general_ref(&mut tree, parent, &reference)?;
                        }
                        None => {
                            // Match the old quick-xml behavior: references outside the
                            // document element belonged to the raw prolog/epilog text.
                            let mut raw = Vec::new();
                            raw.try_reserve(reference.as_ref().len() + 2)
                                .map_err(|_| Error::Xml("xml: out of memory growing raw text"))?;
                            raw.push(b'&');
                            raw.extend_from_slice(reference.as_ref());
                            raw.push(b';');
                            tree.push(Node::Raw(raw), None)?;
                        }
                    }
                }
                Event::CData(c) => {
                    let mut raw = b"<![CDATA[".to_vec();
                    raw.extend_from_slice(&c.into_inner());
                    raw.extend_from_slice(b"]]>");
                    tree.push(Node::Raw(raw), stack.last().copied())?;
                }
                Event::Comment(c) => {
                    let mut raw = b"<!--".to_vec();
                    raw.extend_from_slice(c.as_ref());
                    raw.extend_from_slice(b"-->");
                    tree.push(Node::Raw(raw), stack.last().copied())?;
                }
                Event::PI(p) => {
                    let mut raw = b"<?".to_vec();
                    raw.extend_from_slice(p.as_ref());
                    raw.extend_from_slice(b"?>");
                    tree.push(Node::Raw(raw), stack.last().copied())?;
                }
                Event::Decl(d) => {
                    if !stack.is_empty() || !tree.roots.is_empty() {
                        return Err(Error::Xml(
                            "xml declaration is only allowed at the start of the document",
                        ));
                    }
                    let mut raw = b"<?".to_vec();
                    raw.extend_from_slice(d.as_ref());
                    raw.extend_from_slice(b"?>");
                    tree.push(Node::Raw(raw), stack.last().copied())?;
                }
                Event::DocType(d) => {
                    let mut raw = b"<!DOCTYPE ".to_vec();
                    raw.extend_from_slice(d.as_ref());
                    raw.extend_from_slice(b">");
                    tree.push(Node::Raw(raw), stack.last().copied())?;
                }
                Event::Eof => {
                    // Truncated input: `quick_xml` returns Eof even with
                    // elements still open (e.g. `<a><b>`). Reject it rather
                    // than inventing close tags — an edit must never silently
                    // rewrite a damaged part into new content.
                    if !stack.is_empty() {
                        return Err(Error::Xml("xml ended with unclosed elements"));
                    }
                    break;
                }
            }
            buf.clear();
        }
        Ok(tree)
    }

    /// Serialize the tree back to XML bytes. Untouched nodes re-emit with their
    /// original structure (attribute order/names/values, namespace decls,
    /// comments, PIs preserved); attribute values and text are canonically
    /// escaped.
    pub(crate) fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for &root in &self.roots {
            self.write_node(root, &mut out);
        }
        out
    }

    fn write_node(&self, id: NodeId, out: &mut Vec<u8>) {
        let data = &self.nodes[id.0 as usize];
        match &data.node {
            Node::Raw(bytes) => out.extend_from_slice(bytes),
            Node::Text(bytes) => esc_text_into(bytes, out),
            Node::Element {
                name,
                attrs,
                self_closing,
            } => {
                out.push(b'<');
                out.extend_from_slice(name);
                for (k, v) in attrs {
                    out.push(b' ');
                    out.extend_from_slice(k);
                    out.extend_from_slice(b"=\"");
                    esc_attr_into(v, out);
                    out.push(b'"');
                }
                // Only honor `self_closing` if the element still has no
                // children on write: an edit that grafts children onto a
                // previously self-closing element must force an explicit
                // close tag, or the grafted content would vanish on output.
                if *self_closing && data.children.is_empty() {
                    out.extend_from_slice(b"/>");
                } else {
                    out.push(b'>');
                    for &c in &data.children {
                        self.write_node(c, out);
                    }
                    out.extend_from_slice(b"</");
                    out.extend_from_slice(name);
                    out.push(b'>');
                }
            }
        }
    }

    /// Number of nodes currently in the arena.
    pub(crate) fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// The first `Element` among the top-level `roots` (skips `Raw` prolog
    /// nodes such as the XML declaration).
    pub(crate) fn root_element(&self) -> Option<NodeId> {
        self.roots
            .iter()
            .copied()
            .find(|&id| matches!(self.nodes[id.0 as usize].node, Node::Element { .. }))
    }

    /// Direct children of `id`, in source order.
    pub(crate) fn children_of(&self, id: NodeId) -> &[NodeId] {
        &self.nodes[id.0 as usize].children
    }

    /// The first direct child of `parent` that is an element with exactly this
    /// qualified name. A plain (non-namespace-resolving) comparison: SpreadsheetML
    /// parts don't have the heavy namespace-prefix variability WordprocessingML
    /// does, so callers use plain names like `b"f"`/`b"v"`/`b"row"` directly.
    pub(crate) fn child_by_name(&self, parent: NodeId, name: &[u8]) -> Option<NodeId> {
        self.nodes[parent.0 as usize]
            .children
            .iter()
            .copied()
            .find(|&c| match &self.nodes[c.0 as usize].node {
                Node::Element { name: n, .. } => n.as_slice() == name,
                _ => false,
            })
    }

    /// An element's raw qualified name (e.g. `b"sheet"`), or `None` for a
    /// non-element node. Lets a caller compute a same-named-sibling ordinal
    /// (e.g. "the Nth `<sheet>`") over a mixed children list — one that may
    /// have interleaved whitespace `Text` nodes from a pretty-printed part —
    /// without [`XmlTree::child_by_name`]'s first-match-only search.
    pub(crate) fn element_name(&self, id: NodeId) -> Option<&[u8]> {
        match &self.nodes[id.0 as usize].node {
            Node::Element { name, .. } => Some(name.as_slice()),
            _ => None,
        }
    }

    /// An element's attribute value by exact key match (e.g. `r`, `s`, `t` on a
    /// `<c>` element). `None` if absent or `id` is not an element.
    pub(crate) fn attr_value(&self, id: NodeId, key: &[u8]) -> Option<&[u8]> {
        match &self.nodes[id.0 as usize].node {
            Node::Element { attrs, .. } => attrs
                .iter()
                .find(|(k, _)| k.as_slice() == key)
                .map(|(_, v)| v.as_slice()),
            _ => None,
        }
    }

    /// An element's ordered decoded attribute pairs, or `None` for a
    /// non-element node. Structural editors use this to split a range-shaped
    /// element while retaining attributes they do not interpret.
    pub(crate) fn attributes(&self, id: NodeId) -> Option<&[(Vec<u8>, Vec<u8>)]> {
        match &self.nodes[id.0 as usize].node {
            Node::Element { attrs, .. } => Some(attrs.as_slice()),
            _ => None,
        }
    }

    /// Concatenated text of every `Text`/CDATA descendant of `id` (lossy UTF-8).
    pub(crate) fn text_of(&self, id: NodeId) -> String {
        let mut out = String::new();
        self.collect_text(id, &mut out);
        out
    }

    fn collect_text(&self, id: NodeId, out: &mut String) {
        match &self.nodes[id.0 as usize].node {
            Node::Text(t) => out.push_str(&String::from_utf8_lossy(t)),
            // A `Raw` `<![CDATA[…]]>` is character data too — decode its
            // payload so `<t><![CDATA[OLD]]></t>` reads as `"OLD"`. Other `Raw`
            // markup (comments/PIs) carries no character data.
            Node::Raw(r) => {
                if let Some(inner) = r
                    .strip_prefix(b"<![CDATA[".as_slice())
                    .and_then(|s| s.strip_suffix(b"]]>".as_slice()))
                {
                    out.push_str(&String::from_utf8_lossy(inner));
                }
            }
            Node::Element { .. } => {
                for i in 0..self.nodes[id.0 as usize].children.len() {
                    let c = self.nodes[id.0 as usize].children[i];
                    self.collect_text(c, out);
                }
            }
        }
    }

    /// Set an element's text to a single text node. **Reuses** an existing
    /// text-carrying child slot in place when present (a `Text` node, or a
    /// CDATA `Raw` node converted to `Text`) — so repeated edits do not grow
    /// the arena — otherwise clears any other children and pushes one new text
    /// node.
    pub(crate) fn set_element_text(&mut self, id: NodeId, text: &str) -> Result<()> {
        // Test seam: simulate a commit-time allocation failure (see
        // `commit_should_fail`).
        #[cfg(test)]
        if commit_should_fail() {
            return Err(Error::Xml(
                "simulated commit-time allocation failure (test seam)",
            ));
        }
        let reuse = self.nodes[id.0 as usize]
            .children
            .iter()
            .copied()
            .find(|&c| match &self.nodes[c.0 as usize].node {
                Node::Text(_) => true,
                Node::Raw(r) => r.starts_with(b"<![CDATA["),
                Node::Element { .. } => false,
            });
        match reuse {
            Some(tid) => {
                self.nodes[tid.0 as usize].node = Node::Text(text.as_bytes().to_vec());
                self.nodes[id.0 as usize].children = vec![tid];
            }
            None => {
                // Preflight every fallible step `push` below would need
                // *before* the destructive `children.clear()`. Clearing
                // first (the old order) meant a `push` failure — a genuine
                // `try_reserve` OOM — reported `Err` while the element's
                // prior content was already gone; a caller that treats
                // `Err` as "nothing happened" would be wrong. Doing the
                // fallible reservations first means a rejected edit leaves
                // the tree completely unchanged.
                #[cfg(test)]
                if commit_should_fail() {
                    return Err(Error::Xml(
                        "simulated commit-time allocation failure (test seam)",
                    ));
                }
                self.nodes
                    .try_reserve(1)
                    .map_err(|_| Error::Xml("xml: out of memory growing node arena"))?;
                self.nodes[id.0 as usize]
                    .children
                    .try_reserve(1)
                    .map_err(|_| Error::Xml("xml: out of memory growing child list"))?;
                self.nodes[id.0 as usize].children.clear();
                // `push` cannot fail from here: both reservations it needs
                // (the arena and this element's child list) already
                // succeeded above, so this can only re-confirm them.
                self.push(Node::Text(text.as_bytes().to_vec()), Some(id))?;
            }
        }
        Ok(())
    }

    /// Set (or add) an attribute on an element, preserving the order of
    /// existing attributes. No-op on non-elements. **Errors** when *adding* a
    /// new attribute would exceed [`max_attrs`] (replacing an existing one
    /// always succeeds — it doesn't grow the list), so an edit can never build
    /// an element the parser would later reject for having too many
    /// attributes (parse/edit budget symmetry).
    pub(crate) fn set_attr(&mut self, id: NodeId, key: &[u8], val: &[u8]) -> Result<()> {
        if let Node::Element { attrs, .. } = &mut self.nodes[id.0 as usize].node {
            match attrs.iter_mut().find(|(k, _)| k.as_slice() == key) {
                Some((_, v)) => *v = val.to_vec(),
                None => {
                    if attrs.len() >= max_attrs() {
                        return Err(Error::Xml("element has too many attributes to add another"));
                    }
                    attrs.push((key.to_vec(), val.to_vec()));
                }
            }
        }
        Ok(())
    }

    /// Whether [`Self::set_attr`]`(id, key, _)` would succeed: `true` if `id`
    /// already has `key` (replace, no growth) or has room under the attribute
    /// cap. Lets an edit preflight-reject — before any mutation — a change that
    /// would overflow the cap on a new attribute.
    pub(crate) fn can_set_attr(&self, id: NodeId, key: &[u8]) -> bool {
        match &self.nodes[id.0 as usize].node {
            Node::Element { attrs, .. } => {
                attrs.iter().any(|(k, _)| k.as_slice() == key) || attrs.len() < max_attrs()
            }
            _ => true,
        }
    }

    /// Remove an attribute if present (no-op otherwise / on non-elements).
    pub(crate) fn remove_attr(&mut self, id: NodeId, key: &[u8]) {
        if let Node::Element { attrs, .. } = &mut self.nodes[id.0 as usize].node {
            attrs.retain(|(k, _)| k.as_slice() != key);
        }
    }

    /// Remove `id` from `parent`'s direct children. **Errors** if `id` is not
    /// currently a direct child of `parent` — needed for structural edits such
    /// as dropping a stale `<f>` (formula) child when a cell's value type
    /// changes.
    pub(crate) fn remove_child(&mut self, parent: NodeId, id: NodeId) -> Result<()> {
        let children = &mut self.nodes[parent.0 as usize].children;
        let pos = children
            .iter()
            .position(|&c| c == id)
            .ok_or(Error::Xml("child not found under parent"))?;
        children.remove(pos);
        Ok(())
    }

    /// Parse `xml` as a throwaway fragment and graft its root node(s) as new
    /// children of `parent`, inserted starting at `index` (clamped to
    /// `parent`'s current child count). Returns the [`NodeId`] of the first
    /// grafted node.
    ///
    /// Two budgets are preflighted against the *whole* fragment before any
    /// node of it is committed to `self`, so an edit can't silently grow a
    /// tree past what a re-parse would accept and can't silently mutate the
    /// tree on a rejection:
    /// - node count, re-checked against `self.nodes.len() + frag.nodes.len()`;
    /// - nesting depth, re-checked against `parent`'s current depth plus the
    ///   fragment's own deepest node (see [`MAX_DEPTH`]) — otherwise chaining
    ///   many small inserts (each individually far under the node budget)
    ///   could build a tree deep enough to stack-overflow `write_node`/
    ///   `collect_text`, which recurse over element children with no depth
    ///   check of their own.
    ///
    /// Because both budgets are checked up front, [`Self::graft`] itself
    /// should only ever fail on a genuine allocator OOM once grafting
    /// starts; that residual case is still handled without leaving a
    /// partial mutation behind — see the rollback below.
    pub(crate) fn insert_fragment_at(
        &mut self,
        parent: NodeId,
        index: usize,
        xml: &[u8],
    ) -> Result<NodeId> {
        // Test seam: simulate a commit-time allocation failure (see
        // `commit_should_fail`).
        #[cfg(test)]
        if commit_should_fail() {
            return Err(Error::Xml(
                "simulated commit-time allocation failure (test seam)",
            ));
        }
        let pos = index.min(self.nodes[parent.0 as usize].children.len());
        let frag = XmlTree::parse(xml)?;
        // Keep the arena bounded across edits, not just at the initial parse.
        if self.nodes.len().saturating_add(frag.nodes.len()) > node_budget() {
            return Err(Error::Xml("edit would exceed the node budget"));
        }
        // Keep nesting bounded across edits too (see the doc comment above).
        // `frag`'s own nodes carry `depth` values computed relative to
        // `frag`'s own roots (root = 1), so the deepest node any fragment
        // root would reach once grafted under `parent` is `parent`'s depth
        // plus that value.
        let parent_depth = self.nodes[parent.0 as usize].depth;
        let frag_max_depth = frag.nodes.iter().map(|n| n.depth).max().unwrap_or(0);
        if parent_depth.saturating_add(frag_max_depth) > MAX_DEPTH {
            return Err(Error::Xml(
                "xml fragment insert would exceed the maximum nesting depth",
            ));
        }
        // `self.nodes` only ever grows by appending at the tail, so a
        // snapshot of its length (and of `parent`'s own child-list length)
        // taken here is exactly what a failed attempt must roll back to:
        // every node/link created by this call — including any earlier
        // fragment root that grafted successfully before a later one
        // failed — lives at-or-after this point.
        let pre_nodes_len = self.nodes.len();
        let pre_children_len = self.nodes[parent.0 as usize].children.len();
        let mut added: Vec<NodeId> = Vec::new();
        for &r in &frag.roots {
            match self.graft(&frag, r, parent) {
                Ok(id) => added.push(id),
                Err(e) => {
                    self.nodes.truncate(pre_nodes_len);
                    self.nodes[parent.0 as usize]
                        .children
                        .truncate(pre_children_len);
                    return Err(e);
                }
            }
        }
        let Some(&first) = added.first() else {
            return Err(Error::Xml("xml fragment has no root nodes to insert"));
        };
        let n = added.len();
        let pi = parent.0 as usize;
        let head_len = self.nodes[pi].children.len() - n;
        let ch = &mut self.nodes[pi].children;
        let tail: Vec<NodeId> = ch.split_off(head_len);
        for (k, id) in tail.into_iter().enumerate() {
            ch.insert(pos + k, id);
        }
        Ok(first)
    }

    /// Recursively copy a node (and its descendants) from `src` into `self` as
    /// a new child of `parent`. Recursion is bounded: `src` was parsed under
    /// `MAX_DEPTH`, so the call depth here cannot exceed it — no stack-overflow
    /// path on grafted fragments. Callers (`insert_fragment_at`) preflight
    /// both the node-count and nesting-depth budgets before calling this, so
    /// in production the only way `push` below can fail is a genuine
    /// allocator OOM; the test-only seam lets that rare path be exercised
    /// deterministically.
    fn graft(&mut self, src: &XmlTree, src_id: NodeId, parent: NodeId) -> Result<NodeId> {
        // Test seam: simulate a commit-time allocation failure (see
        // `commit_should_fail`).
        #[cfg(test)]
        if commit_should_fail() {
            return Err(Error::Xml(
                "simulated commit-time allocation failure (test seam)",
            ));
        }
        let node = src.nodes[src_id.0 as usize].node.clone();
        let new_id = self.push(node, Some(parent))?;
        for &c in &src.nodes[src_id.0 as usize].children {
            self.graft(src, c, new_id)?;
        }
        Ok(new_id)
    }

    fn push(&mut self, node: Node, parent: Option<NodeId>) -> Result<NodeId> {
        // Fallible allocation is the real OOM guard (see `MAX_NODES`): reserve
        // every slot we are about to fill *before* mutating, so a hostile part
        // that would exhaust memory returns an `Error` here instead of
        // aborting the process — and the arena stays consistent on failure
        // (nothing is pushed if either reservation fails).
        self.nodes
            .try_reserve(1)
            .map_err(|_| Error::Xml("xml: out of memory growing node arena"))?;
        match parent {
            Some(p) => self.nodes[p.0 as usize]
                .children
                .try_reserve(1)
                .map_err(|_| Error::Xml("xml: out of memory growing child list"))?,
            None => self
                .roots
                .try_reserve(1)
                .map_err(|_| Error::Xml("xml: out of memory growing root list"))?,
        }
        let depth = match parent {
            Some(p) => self.nodes[p.0 as usize].depth + 1,
            None => 1,
        };
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(NodeData {
            node,
            children: Vec::new(),
            depth,
        });
        match parent {
            Some(p) => self.nodes[p.0 as usize].children.push(id),
            None => self.roots.push(id),
        }
        Ok(id)
    }

    fn push_text_fragment(&mut self, parent: NodeId, fragment: &[u8]) -> Result<()> {
        if fragment.is_empty() {
            return Ok(());
        }
        let last = self.nodes[parent.0 as usize].children.last().copied();
        if let Some(last) = last {
            if let Node::Text(text) = &mut self.nodes[last.0 as usize].node {
                text.try_reserve(fragment.len())
                    .map_err(|_| Error::Xml("xml: out of memory growing text node"))?;
                text.extend_from_slice(fragment);
                return Ok(());
            }
        }

        let mut text = Vec::new();
        text.try_reserve(fragment.len())
            .map_err(|_| Error::Xml("xml: out of memory creating text node"))?;
        text.extend_from_slice(fragment);
        self.push(Node::Text(text), Some(parent))?;
        Ok(())
    }
}

/// Maximum attributes accepted on a single element — a size-capped part could
/// otherwise pack one element with millions of attributes, amplifying into
/// large heap use.
const MAX_ATTRS_PER_ELEMENT: usize = 65_536;

#[cfg(test)]
thread_local! {
    static TEST_MAX_GENERAL_REFS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(crate::MAX_XML_GENERAL_REFS) };
}

#[cfg(test)]
fn set_test_max_general_refs(n: usize) {
    TEST_MAX_GENERAL_REFS.with(|cap| cap.set(n));
}

fn max_general_refs() -> usize {
    #[cfg(test)]
    {
        TEST_MAX_GENERAL_REFS.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        crate::MAX_XML_GENERAL_REFS
    }
}

// Test-lowerable copy of the attribute cap, so the over-cap path can be
// exercised on a tiny fixture instead of a 65k-attribute one. Production
// always uses the const.
#[cfg(test)]
thread_local! {
    static TEST_MAX_ATTRS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(MAX_ATTRS_PER_ELEMENT) };
}

/// Set the per-element attribute cap for the current test thread.
#[cfg(test)]
pub(crate) fn set_test_max_attrs(n: usize) {
    TEST_MAX_ATTRS.with(|c| c.set(n));
}

fn max_attrs() -> usize {
    #[cfg(test)]
    {
        TEST_MAX_ATTRS.with(|c| c.get())
    }
    #[cfg(not(test))]
    {
        MAX_ATTRS_PER_ELEMENT
    }
}

/// Build a [`Node::Element`] from a quick-xml start/empty tag, capturing the
/// raw qualified name and attributes (unescaped values) in source order.
fn element_node(e: &quick_xml::events::BytesStart<'_>, self_closing: bool) -> Result<Node> {
    let name = e.name().as_ref().to_vec();
    let mut attrs = Vec::new();
    let cap = max_attrs();
    for a in e.attributes() {
        if attrs.len() >= cap {
            return Err(Error::Xml("element has too many attributes"));
        }
        let a = a.map_err(|_| Error::Xml("malformed xml attribute"))?;
        let key = a.key.as_ref().to_vec();
        // Propagate (not swallow) a malformed entity reference: otherwise the
        // raw `&` survives, then re-serialization canonicalizes it to `&amp;`
        // — silently rewriting malformed XML the edit never targeted.
        let val = a
            .decoded_and_normalized_value_with(
                XmlVersion::Implicit1_0,
                e.decoder(),
                1,
                quick_xml::escape::resolve_xml_entity,
            )
            .map_err(|_| Error::Xml("malformed xml attribute value"))?;
        // Reject at the decode site rather than silently dropping at
        // serialize time (see `unescape_bytes`'s doc comment for why this
        // matters): a decoded numeric character reference such as `&#x1;`
        // can name a codepoint XML 1.0 forbids outright.
        if has_xml_illegal_char(&val) {
            return Err(Error::Xml(
                "xml attribute value contains an xml 1.0 illegal character",
            ));
        }
        let val = val.into_owned().into_bytes();
        attrs.push((key, val));
    }
    Ok(Node::Element {
        name,
        attrs,
        self_closing,
    })
}

/// Unescape XML entities in text bytes (UTF-8). **Errors** on non-UTF-8, a
/// malformed entity reference, or a decoded XML-1.0-illegal character (e.g.
/// `&#x1;`) rather than accepting it into the tree — accepting it would
/// silently drop it later at serialize time instead (see
/// `is_xml_legal_char`), and because *any* edit to *any* part of a document
/// re-serializes the whole tree fresh (`Part::bytes()` has no per-node dirty
/// tracking), that drop could corrupt a completely untouched sibling element
/// the next time an unrelated edit forces a re-serialize. Rejecting here, at
/// the source, means a part containing such a character fails to parse as an
/// `XmlTree` at all rather than becoming silently corruptible once promoted.
fn unescape_bytes(raw: &[u8]) -> Result<Vec<u8>> {
    let s = std::str::from_utf8(raw).map_err(|_| Error::Xml("xml text is not valid utf-8"))?;
    let c =
        quick_xml::escape::unescape(s).map_err(|_| Error::Xml("malformed xml entity reference"))?;
    if has_xml_illegal_char(&c) {
        return Err(Error::Xml("xml text contains an xml 1.0 illegal character"));
    }
    Ok(c.into_owned().into_bytes())
}

fn push_resolved_general_ref(
    tree: &mut XmlTree,
    parent: NodeId,
    reference: &BytesRef<'_>,
) -> Result<()> {
    match reference.resolve_char_ref() {
        Ok(Some(ch)) => {
            if !is_xml_legal_char(ch) {
                return Err(Error::Xml("xml text contains an xml 1.0 illegal character"));
            }
            let mut encoded = [0u8; 4];
            tree.push_text_fragment(parent, ch.encode_utf8(&mut encoded).as_bytes())?;
        }
        Ok(None) => {
            let name = reference
                .decode()
                .map_err(|_| Error::Xml("xml entity reference is not valid utf-8"))?;
            let resolved = quick_xml::escape::resolve_xml_entity(&name)
                .ok_or(Error::Xml("malformed xml entity reference"))?;
            tree.push_text_fragment(parent, resolved.as_bytes())?;
        }
        Err(_) => return Err(Error::Xml("malformed xml entity reference")),
    }
    Ok(())
}

/// XML 1.0 character validity. Edited strings can contain Unicode scalar
/// values (`U+FFFE`/`U+FFFF`) that are valid Rust `char`s but forbidden in XML,
/// so filter them alongside illegal C0 controls before serializing.
fn is_xml_legal_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r')
        || matches!(
            c as u32,
            0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF
        )
}

/// Whether `s` contains any character `is_xml_legal_char` forbids. Shared by
/// the parse-time decode sites (`unescape_bytes` for text, `element_node` for
/// attribute values) that must reject such input rather than let it reach
/// the serializer, which would otherwise silently drop it (see
/// `unescape_bytes`'s doc comment).
fn has_xml_illegal_char(s: &str) -> bool {
    s.chars().any(|c| !is_xml_legal_char(c))
}

fn push_char_utf8(c: char, out: &mut Vec<u8>) {
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

/// Canonical escaping for element text content. Distinct from
/// [`esc_attr_into`] because XML normalization rules differ between text
/// content and attribute values — see that function's doc comment.
fn esc_text_into(s: &[u8], out: &mut Vec<u8>) {
    for c in String::from_utf8_lossy(s).chars() {
        match c {
            '&' => out.extend_from_slice(b"&amp;"),
            '<' => out.extend_from_slice(b"&lt;"),
            '>' => out.extend_from_slice(b"&gt;"),
            // A literal CR would be folded to LF by XML end-of-line
            // normalization on the next read; emit it as a character
            // reference so the byte survives.
            '\r' => out.extend_from_slice(b"&#13;"),
            _ if !is_xml_legal_char(c) => {}
            _ => push_char_utf8(c, out),
        }
    }
}

/// Canonical escaping for attribute values. Additionally escapes `"` (values
/// are always double-quoted on write) and character-references
/// `\t`/`\n`/`\r`, because attribute-value normalization would otherwise
/// collapse them to a plain space on re-parse, silently mangling edited
/// content — text content doesn't need this extra set, hence the two
/// functions are kept distinct rather than merged.
fn esc_attr_into(s: &[u8], out: &mut Vec<u8>) {
    for c in String::from_utf8_lossy(s).chars() {
        match c {
            '&' => out.extend_from_slice(b"&amp;"),
            '<' => out.extend_from_slice(b"&lt;"),
            '>' => out.extend_from_slice(b"&gt;"),
            '"' => out.extend_from_slice(b"&quot;"),
            '\t' => out.extend_from_slice(b"&#9;"),
            '\n' => out.extend_from_slice(b"&#10;"),
            '\r' => out.extend_from_slice(b"&#13;"),
            _ if !is_xml_legal_char(c) => {}
            _ => push_char_utf8(c, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(tree: &XmlTree) -> String {
        String::from_utf8(tree.serialize()).unwrap()
    }

    // --- Parse -> serialize structural round-trip ---

    #[test]
    fn round_trip_preserves_attribute_order_and_namespace_decls() {
        let xml = br#"<worksheet xmlns:r="urn:r" c="3" a="1" b="2"><sheetData/></worksheet>"#;
        let out = s(&XmlTree::parse(xml).unwrap());
        assert_eq!(
            out,
            r#"<worksheet xmlns:r="urn:r" c="3" a="1" b="2"><sheetData/></worksheet>"#
        );
    }

    #[test]
    fn self_closing_vs_explicit_close_preserved() {
        let xml = br#"<a><b/><c></c></a>"#;
        assert_eq!(s(&XmlTree::parse(xml).unwrap()), r#"<a><b/><c></c></a>"#);
    }

    #[test]
    fn serialize_is_idempotent() {
        let xml = br#"<a><b x="1"><c/>txt</b><!-- note --><d>x &amp; y</d></a>"#;
        let once = XmlTree::parse(xml).unwrap().serialize();
        let twice = XmlTree::parse(&once).unwrap().serialize();
        assert_eq!(once, twice, "second round-trip changed bytes");
    }

    #[test]
    fn prolog_and_unknown_markup_round_trips_verbatim() {
        // A decl, a comment, and a PI outside the root must all survive, along
        // with an unmodeled child element inside a `<c>` cell (extLst-shaped).
        let xml = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><!--top--><?pi data?><worksheet><sheetData><row><c r=\"A1\"><v>1</v><extLst><ext uri=\"{x}\"/></extLst></c></row></sheetData></worksheet>";
        let out = s(&XmlTree::parse(xml).unwrap());
        for needle in [
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
            "<!--top-->",
            "<?pi data?>",
            "<extLst><ext uri=\"{x}\"/></extLst>",
        ] {
            assert!(out.contains(needle), "lost {needle:?} in:\n{out}");
        }
    }

    // --- Escaping round-trips ---

    #[test]
    fn text_entities_round_trip_and_canonicalize_spelling() {
        // A numeric entity spelling `&#38;` for `&` must be stored decoded and
        // re-emitted with the canonical `&amp;` spelling.
        let xml = br#"<t>a &#38; b &lt;c&gt; "q"</t>"#;
        let tree = XmlTree::parse(xml).unwrap();
        let root = tree.root_element().unwrap();
        assert_eq!(tree.text_of(root), r#"a & b <c> "q""#);
        assert_eq!(
            tree.node_count(),
            2,
            "entity fragments stay in one text node"
        );
        assert_eq!(s(&tree), r#"<t>a &amp; b &lt;c&gt; "q"</t>"#);
    }

    #[test]
    fn carriage_return_escaped_to_survive_normalization() {
        // Edited text with a CR must serialize as `&#13;`; a literal CR would
        // be folded to LF by XML end-of-line normalization on the next read.
        let mut t = XmlTree::parse(b"<t>x</t>").unwrap();
        let id = t.root_element().unwrap();
        t.set_element_text(id, "a\rb").unwrap();
        assert_eq!(s(&t), "<t>a&#13;b</t>");
        // And it round-trips back to the same content on re-parse.
        let re = XmlTree::parse(s(&t).as_bytes()).unwrap();
        let rid = re.root_element().unwrap();
        assert_eq!(re.text_of(rid), "a\rb");
    }

    #[test]
    fn attribute_value_escapes_quote_and_whitespace_as_char_refs() {
        let mut t = XmlTree::parse(b"<c/>").unwrap();
        let id = t.root_element().unwrap();
        t.set_attr(id, b"t", b"a\"b\tc\nd\re").unwrap();
        assert_eq!(s(&t), "<c t=\"a&quot;b&#9;c&#10;d&#13;e\"/>");
        // Round-trips back to the exact original attribute value.
        let re = XmlTree::parse(s(&t).as_bytes()).unwrap();
        let rid = re.root_element().unwrap();
        assert_eq!(re.attr_value(rid, b"t"), Some(&b"a\"b\tc\nd\re"[..]));
    }

    #[test]
    fn edited_values_drop_xml_forbidden_scalars() {
        let mut t = XmlTree::parse(b"<t>x</t>").unwrap();
        let id = t.root_element().unwrap();
        t.set_element_text(id, "a\u{FFFE}b\u{FFFF}c").unwrap();
        assert_eq!(s(&t), "<t>abc</t>");

        let mut attr = XmlTree::parse(b"<t/>").unwrap();
        let id = attr.root_element().unwrap();
        attr.set_attr(id, b"data-x", "a\u{FFFF}b".as_bytes())
            .unwrap();
        assert_eq!(s(&attr), r#"<t data-x="ab"/>"#);
    }

    // --- Malformed input is rejected, not repaired ---

    #[test]
    fn mismatched_end_tag_is_rejected() {
        assert!(XmlTree::parse(b"<a></b>").is_err());
        assert!(XmlTree::parse(b"<a></a>").is_ok());
    }

    #[test]
    fn truncated_xml_with_open_elements_is_rejected() {
        assert!(XmlTree::parse(b"<a><b>").is_err());
        assert!(XmlTree::parse(b"<c><f><v>1").is_err());
        assert!(XmlTree::parse(b"<a></a>").is_ok()); // well-formed control
    }

    #[test]
    fn invalid_utf8_in_text_is_rejected() {
        let mut xml = b"<t>".to_vec();
        xml.extend_from_slice(&[0xff, 0xfe]);
        xml.extend_from_slice(b"</t>");
        assert!(XmlTree::parse(&xml).is_err());
    }

    #[test]
    fn malformed_entity_refs_are_rejected_not_canonicalized() {
        // A bad entity reference in text OR an attribute must error, not
        // survive raw and get canonicalized (to `&amp;…`) on re-serialization.
        assert!(XmlTree::parse(b"<t>x&bogus;y</t>").is_err());
        assert!(XmlTree::parse(br#"<c x="a&bogus;b"/>"#).is_err());
        // The well-formed controls (proper entities) still parse.
        assert!(XmlTree::parse(b"<t>x&amp;y</t>").is_ok());
        assert!(XmlTree::parse(br#"<c x="a&amp;b"/>"#).is_ok());
    }

    #[test]
    fn declared_custom_entities_are_not_expanded() {
        let xml = br#"<!DOCTYPE t [<!ENTITY local "secret">]><t>&local;</t>"#;
        assert!(XmlTree::parse(xml).is_err());
    }

    #[test]
    fn parsed_attributes_follow_xml_10_whitespace_normalization() {
        let tree = XmlTree::parse(b"<t value=\"a\tb\nc&#9;d\"/>").unwrap();
        let root = tree.root_element().unwrap();
        assert_eq!(tree.attr_value(root, b"value"), Some(&b"a b c\td"[..]));
        assert_eq!(s(&tree), "<t value=\"a b c&#9;d\"/>");
    }

    #[test]
    fn second_xml_declaration_is_rejected() {
        assert!(XmlTree::parse(b"<?xml version=\"1.0\"?><a/><?xml version=\"1.0\"?>").is_err());
        assert!(XmlTree::parse(b"<a><?xml version=\"1.0\"?></a>").is_err());
        assert!(XmlTree::parse(b"<?xml version=\"1.0\"?><a/>").is_ok());
    }

    #[test]
    fn garbage_and_deep_nesting_never_panic() {
        let _ = XmlTree::parse(&[0xff, 0xfe, 0x00, 0x3c]);
        let _ = XmlTree::parse(b"<a><b><c");
        let _ = XmlTree::parse(b"plain text no tags");
        let deep = "<a>".repeat(5000);
        assert!(XmlTree::parse(deep.as_bytes()).is_err());
    }

    // --- Budget rejection, exact boundaries ---

    #[test]
    fn node_budget_boundary_is_exact() {
        // A tree with exactly `node_budget()` nodes parses (edit preflights
        // allow up to exactly the budget, so the parser must accept what an
        // edit can produce); one more must fail.
        set_test_node_budget(3);
        let ok = XmlTree::parse(b"<a><b/><c/></a>"); // 3 nodes == budget
        let err = XmlTree::parse(b"<a><b/><c/><d/></a>"); // 4 > budget
        reset_test_node_budget();
        assert!(ok.is_ok());
        assert!(err.is_err());
    }

    #[test]
    fn general_reference_budget_is_exact() {
        set_test_max_general_refs(2);
        let at_limit = XmlTree::parse(b"<t>&amp;&lt;</t>");
        let over_limit = XmlTree::parse(b"<t>&amp;&lt;&gt;</t>");
        set_test_max_general_refs(crate::MAX_XML_GENERAL_REFS);

        assert!(at_limit.is_ok());
        assert!(over_limit.is_err());
    }

    #[test]
    fn empty_element_respects_depth_cap() {
        // A self-closing element occupies a nesting level too: MAX_DEPTH open
        // elements plus one more `<b/>` must be rejected, but MAX_DEPTH - 1
        // opens plus a `<b/>` still fits.
        let nest = |opens: usize| {
            let mut out = String::new();
            for _ in 0..opens {
                out.push_str("<a>");
            }
            out.push_str("<b/>");
            for _ in 0..opens {
                out.push_str("</a>");
            }
            out
        };
        assert!(XmlTree::parse(nest(MAX_DEPTH).as_bytes()).is_err());
        assert!(XmlTree::parse(nest(MAX_DEPTH - 1).as_bytes()).is_ok());
    }

    #[test]
    fn too_many_attributes_on_parse_is_rejected() {
        set_test_max_attrs(4);
        let over = XmlTree::parse(br#"<c a0="" a1="" a2="" a3="" a4=""/>"#);
        let under = XmlTree::parse(br#"<c a0="" a1=""/>"#);
        set_test_max_attrs(MAX_ATTRS_PER_ELEMENT);
        assert!(over.is_err());
        assert!(under.is_ok());
    }

    #[test]
    fn fallible_push_does_not_panic_under_node_budget_pressure() {
        // Not a genuine OOM, but exercises the same `Err`-not-panic contract
        // the `try_reserve`-based push relies on: pushing past a tiny budget
        // during a fragment insert must return `Err`, never abort.
        set_test_node_budget(2);
        let mut t = XmlTree::parse(b"<a/>").unwrap();
        let id = t.root_element().unwrap();
        let r = t.insert_fragment_at(id, 0, b"<b/><c/>");
        reset_test_node_budget();
        assert!(r.is_err());
    }

    // --- set_attr / can_set_attr / remove_attr ---

    #[test]
    fn set_attr_replaces_in_place_and_appends_preserving_order() {
        let mut t = XmlTree::parse(br#"<c a="1" b="2"/>"#).unwrap();
        let id = t.root_element().unwrap();
        t.set_attr(id, b"a", b"9").unwrap(); // replace, no growth
        assert_eq!(s(&t), r#"<c a="9" b="2"/>"#);
        t.set_attr(id, b"z", b"3").unwrap(); // append at end
        assert_eq!(s(&t), r#"<c a="9" b="2" z="3"/>"#);
    }

    #[test]
    fn set_attr_append_is_budget_gated_replace_is_not() {
        set_test_max_attrs(2);
        let mut t = XmlTree::parse(br#"<c a="1" b="2"/>"#).unwrap();
        let id = t.root_element().unwrap();
        // Replacing an existing key never grows the list, so it's exempt.
        assert!(t.can_set_attr(id, b"a"));
        t.set_attr(id, b"a", b"9").unwrap();
        // Adding a brand-new key at the cap is refused.
        assert!(!t.can_set_attr(id, b"new"));
        assert!(t.set_attr(id, b"new", b"1").is_err());
        set_test_max_attrs(MAX_ATTRS_PER_ELEMENT);
    }

    #[test]
    fn remove_attr_is_noop_if_absent() {
        let mut t = XmlTree::parse(br#"<c a="1"/>"#).unwrap();
        let id = t.root_element().unwrap();
        t.remove_attr(id, b"missing");
        assert_eq!(s(&t), r#"<c a="1"/>"#);
        t.remove_attr(id, b"a");
        assert_eq!(s(&t), r#"<c/>"#);
    }

    // --- text_of / set_element_text ---

    #[test]
    fn text_of_reads_plain_qualified_attrs_and_nested_text() {
        let xml = br#"<c r="A1" s="3" t="s"><v>5</v></c>"#;
        let t = XmlTree::parse(xml).unwrap();
        let id = t.root_element().unwrap();
        assert_eq!(t.attr_value(id, b"r"), Some(&b"A1"[..]));
        assert_eq!(t.attr_value(id, b"s"), Some(&b"3"[..]));
        assert_eq!(t.attr_value(id, b"t"), Some(&b"s"[..]));
        assert_eq!(t.attr_value(id, b"missing"), None);
        assert_eq!(t.text_of(id), "5");
    }

    #[test]
    fn set_element_text_reuses_text_carrier_without_growing_arena() {
        let mut t = XmlTree::parse(b"<t>OLD</t>").unwrap();
        let before = t.node_count();
        let id = t.root_element().unwrap();
        t.set_element_text(id, "NEW").unwrap();
        assert_eq!(t.node_count(), before, "reused-carrier edit grew the arena");
        assert_eq!(t.text_of(id), "NEW");
    }

    #[test]
    fn set_element_text_reuses_cdata_carrier_without_growing_arena() {
        let mut t = XmlTree::parse(b"<t><![CDATA[OLD]]></t>").unwrap();
        let before = t.node_count();
        let id = t.root_element().unwrap();
        t.set_element_text(id, "NEW").unwrap();
        assert_eq!(t.node_count(), before, "CDATA edit grew the arena");
        assert_eq!(t.text_of(id), "NEW");
        assert_eq!(s(&t), "<t>NEW</t>");
    }

    #[test]
    fn set_element_text_allocates_when_no_carrier_present() {
        let mut t = XmlTree::parse(b"<t/>").unwrap();
        let before = t.node_count();
        let id = t.root_element().unwrap();
        t.set_element_text(id, "NEW").unwrap();
        assert!(t.node_count() > before, "no-carrier edit should allocate");
        assert_eq!(s(&t), "<t>NEW</t>");
    }

    #[test]
    fn set_element_text_clears_other_children_besides_the_reused_carrier() {
        let mut t = XmlTree::parse(b"<c><extra/>OLD<more/></c>").unwrap();
        let id = t.root_element().unwrap();
        t.set_element_text(id, "NEW").unwrap();
        assert_eq!(s(&t), "<c>NEW</c>");
    }

    // --- insert_fragment_at / graft ---

    #[test]
    fn insert_fragment_at_grafts_at_index_and_returns_first_new_id() {
        let mut t = XmlTree::parse(b"<row><c r=\"A1\"/><c r=\"C1\"/></row>").unwrap();
        let row = t.root_element().unwrap();
        let new_id = t.insert_fragment_at(row, 1, br#"<c r="B1"/>"#).unwrap();
        assert_eq!(s(&t), r#"<row><c r="A1"/><c r="B1"/><c r="C1"/></row>"#);
        assert_eq!(t.attr_value(new_id, b"r"), Some(&b"B1"[..]));
    }

    #[test]
    fn insert_fragment_at_grafts_multiple_roots_and_nested_children() {
        let mut t = XmlTree::parse(b"<row/>").unwrap();
        let row = t.root_element().unwrap();
        t.insert_fragment_at(row, 0, b"<c><v>1</v></c><c><v>2</v></c>")
            .unwrap();
        assert_eq!(s(&t), "<row><c><v>1</v></c><c><v>2</v></c></row>");
    }

    #[test]
    fn insert_fragment_at_index_is_clamped_to_child_count() {
        let mut t = XmlTree::parse(b"<row><a/></row>").unwrap();
        let row = t.root_element().unwrap();
        t.insert_fragment_at(row, 999, b"<b/>").unwrap();
        assert_eq!(s(&t), "<row><a/><b/></row>");
    }

    #[test]
    fn insert_fragment_at_rejects_over_node_budget_before_committing() {
        set_test_node_budget(2);
        let mut t = XmlTree::parse(b"<row/>").unwrap();
        let before = t.node_count();
        let row = t.root_element().unwrap();
        let r = t.insert_fragment_at(row, 0, b"<a/><b/>");
        reset_test_node_budget();
        assert!(r.is_err());
        assert_eq!(
            t.node_count(),
            before,
            "rejected insert must not partially commit"
        );
    }

    #[test]
    fn self_closing_element_gains_explicit_close_after_graft() {
        let mut t = XmlTree::parse(b"<c/>").unwrap();
        let id = t.root_element().unwrap();
        assert_eq!(s(&t), "<c/>");
        t.insert_fragment_at(id, 0, b"<v>1</v>").unwrap();
        // Now that the element has children, it must not serialize
        // self-closing (which would silently drop the grafted content).
        assert_eq!(s(&t), "<c><v>1</v></c>");
    }

    // --- Navigation primitives ---

    #[test]
    fn children_of_and_child_by_name() {
        let t = XmlTree::parse(b"<row><c r=\"A1\"/><f>SUM</f><c r=\"B1\"/></row>").unwrap();
        let row = t.root_element().unwrap();
        assert_eq!(t.children_of(row).len(), 3);
        let f = t.child_by_name(row, b"f").unwrap();
        assert_eq!(t.text_of(f), "SUM");
        assert!(t.child_by_name(row, b"missing").is_none());
        // Returns the FIRST matching child when more than one exists.
        let first_c = t.child_by_name(row, b"c").unwrap();
        assert_eq!(t.attr_value(first_c, b"r"), Some(&b"A1"[..]));
    }

    #[test]
    fn element_name_distinguishes_elements_from_text_and_none_for_non_elements() {
        let t = XmlTree::parse(b"<sheets>\n  <sheet/>\n  <sheet/>\n</sheets>").unwrap();
        let root = t.root_element().unwrap();
        let kinds: Vec<Option<&[u8]>> = t
            .children_of(root)
            .iter()
            .map(|&c| t.element_name(c))
            .collect();
        // Whitespace `Text` nodes between the two `<sheet>` elements must
        // report `None`, not be mistaken for elements.
        assert_eq!(
            kinds,
            vec![None, Some(&b"sheet"[..]), None, Some(&b"sheet"[..]), None]
        );
    }

    #[test]
    fn remove_child_removes_and_errors_if_not_a_direct_child() {
        let mut t = XmlTree::parse(b"<row><c/><f/></row>").unwrap();
        let row = t.root_element().unwrap();
        let f = t.child_by_name(row, b"f").unwrap();
        t.remove_child(row, f).unwrap();
        assert_eq!(s(&t), "<row><c/></row>");
        // Removing again (no longer a child) errors rather than no-op.
        assert!(t.remove_child(row, f).is_err());
        // A node that is not a child of `row` at all also errors.
        let c = t.child_by_name(row, b"c").unwrap();
        assert!(t.remove_child(c, row).is_err());
    }

    #[test]
    fn root_element_skips_raw_prolog_nodes() {
        let xml = b"<?xml version=\"1.0\"?><!--c--><worksheet foo=\"1\"/>";
        let t = XmlTree::parse(xml).unwrap();
        // Full round-trip is unaffected by the prolog (sanity check the fixture).
        assert_eq!(s(&t), std::str::from_utf8(xml).unwrap());
        // `root_element` must skip past the Decl/Comment `Raw` roots and land on
        // the actual `<worksheet>` element, not one of the `Raw` siblings (a
        // `Raw` id would fail `attr_value`, which only resolves on elements).
        let id = t.root_element().unwrap();
        assert_eq!(t.attr_value(id, b"foo"), Some(&b"1"[..]));
        assert!(t.children_of(id).is_empty());
    }

    // --- Commit-failure test seam ---

    #[test]
    fn commit_should_fail_seam_fires_once_then_disarms() {
        let mut t = XmlTree::parse(b"<t>x</t>").unwrap();
        let id = t.root_element().unwrap();

        set_test_fail_commit_after(1);
        assert!(
            t.set_element_text(id, "a").is_ok(),
            "first commit should succeed"
        );
        assert!(
            t.set_element_text(id, "b").is_err(),
            "armed commit should fail exactly once"
        );
        assert!(
            t.set_element_text(id, "c").is_ok(),
            "seam must self-disarm after firing"
        );
        reset_test_fail_commit();

        set_test_fail_commit_after(0);
        let row_xml = b"<row/>";
        let mut row_tree = XmlTree::parse(row_xml).unwrap();
        let row = row_tree.root_element().unwrap();
        assert!(
            row_tree.insert_fragment_at(row, 0, b"<c/>").is_err(),
            "insert_fragment_at must also honor the commit-failure seam"
        );
        reset_test_fail_commit();
        assert!(row_tree.insert_fragment_at(row, 0, b"<c/>").is_ok());
    }

    // --- Bug fix regression tests ---

    // Bug 1: insert_fragment_at/graft only checked node COUNT budget, never
    // nesting DEPTH, so chained small inserts could build a tree deep enough
    // to stack-overflow write_node/collect_text (unbounded recursion).
    #[test]
    fn insert_fragment_at_rejects_chained_inserts_before_exceeding_max_depth() {
        // Each individual insert is trivially small (one `<a/>` node, far
        // under the node budget), but chaining many of them — each grafted
        // under the previous insert's own returned id — can build a tree far
        // deeper than any single `parse()` call would ever accept. Before
        // the depth preflight, this kept succeeding indefinitely; a tree
        // built this way would later stack-overflow `serialize()`/
        // `text_of()` (plain recursion with no depth check of their own).
        // It must instead be rejected here, long before that, with a typed
        // `Err` rather than crashing.
        let mut t = XmlTree::parse(b"<a/>").unwrap();
        let mut cur = t.root_element().unwrap();
        let mut successes = 0usize;
        let mut node_count_before_last_attempt = t.node_count();
        let mut failed = false;
        for _ in 0..(MAX_DEPTH + 16) {
            node_count_before_last_attempt = t.node_count();
            match t.insert_fragment_at(cur, 0, b"<a/>") {
                Ok(id) => {
                    cur = id;
                    successes += 1;
                }
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
        assert!(
            failed,
            "chained inserts must eventually be rejected for depth, not keep \
             succeeding indefinitely"
        );
        // The rejection must come nowhere near needing "a few hundred
        // thousand" inserts to trigger, and must land exactly at the
        // MAX_DEPTH boundary — mirrors `node_budget_boundary_is_exact`'s
        // exact-boundary style, just for the depth budget instead.
        assert_eq!(
            successes,
            MAX_DEPTH - 1,
            "depth budget boundary is off (root itself already occupies depth 1)"
        );
        assert_eq!(
            t.node_count(),
            node_count_before_last_attempt,
            "the rejected insert must not have partially committed"
        );
        // The tree remains fully usable afterward: no crash, despite now
        // being MAX_DEPTH levels deep.
        let bytes = t.serialize();
        assert!(!bytes.is_empty());
        let _ = t.text_of(t.root_element().unwrap());
    }

    // Bug 2 (set_element_text): the no-carrier branch cleared the element's
    // existing children (destructive, infallible) BEFORE the fallible
    // `push`, so a push failure left content destroyed despite returning
    // `Err`.
    #[test]
    fn set_element_text_no_carrier_path_leaves_tree_unchanged_on_simulated_failure() {
        let mut t = XmlTree::parse(b"<c><extra/><more/></c>").unwrap(); // no text/CDATA carrier
        let id = t.root_element().unwrap();
        let before_bytes = s(&t);
        let before_count = t.node_count();
        // Tick 1: the unrelated top-of-function check (must pass through).
        // Tick 2: the no-carrier branch's own preflight, positioned right
        // before the destructive `children.clear()` — this is the one that
        // must fire for this test.
        set_test_fail_commit_after(1);
        let r = t.set_element_text(id, "NEW");
        reset_test_fail_commit();
        assert!(r.is_err(), "simulated failure must propagate as Err");
        assert_eq!(
            s(&t),
            before_bytes,
            "a rejected no-carrier edit must not destroy the element's prior children"
        );
        assert_eq!(
            t.node_count(),
            before_count,
            "a rejected no-carrier edit must not grow the arena"
        );
    }

    // Bug 2 (graft/insert_fragment_at): an earlier fragment root that
    // grafted successfully stayed permanently linked into the live tree even
    // if a later fragment root in the same insert failed.
    #[test]
    fn insert_fragment_at_rolls_back_earlier_roots_when_a_later_root_fails() {
        let mut t = XmlTree::parse(b"<row/>").unwrap();
        let row = t.root_element().unwrap();
        let before_bytes = s(&t);
        let before_count = t.node_count();
        // Tick 1: insert_fragment_at's own top-of-function check (pass).
        // Tick 2: graft() for the first fragment root `<a/>` (pass, commits).
        // Tick 3: graft() for the second fragment root `<b/>` (fail).
        set_test_fail_commit_after(2);
        let r = t.insert_fragment_at(row, 0, b"<a/><b/>");
        reset_test_fail_commit();
        assert!(
            r.is_err(),
            "simulated mid-graft failure must propagate as Err"
        );
        assert_eq!(
            s(&t),
            before_bytes,
            "an earlier fragment root that grafted successfully must be rolled back \
             when a later root in the same insert fails"
        );
        assert_eq!(
            t.node_count(),
            before_count,
            "insert_fragment_at must be all-or-nothing"
        );
    }

    // Bug 3: escaping silently dropped XML-1.0-illegal characters at
    // serialize time instead of rejecting them at parse time, so an
    // untouched cell containing e.g. `&#x1;` could be silently corrupted the
    // next time ANY edit forced a whole-part re-serialize.
    #[test]
    fn parse_rejects_xml_illegal_char_from_numeric_entity_in_text() {
        // `&#x1;` decodes to U+0001, which XML 1.0 forbids outright. Before
        // this fix, parse() accepted it into a `Text` node (`text_of` even
        // read it back correctly) and only silently dropped it later at
        // serialize time (`<t>ab</t>`). It must be rejected here instead.
        assert!(XmlTree::parse(b"<t>a&#x1;b</t>").is_err());
        // Legal content is unaffected.
        assert!(XmlTree::parse(b"<t>abc</t>").is_ok());
    }

    #[test]
    fn parse_rejects_xml_illegal_char_from_numeric_entity_in_attribute() {
        // Same defect, same fix, sibling call path: attribute-value entity
        // decoding must be held to the same rule as text-node decoding.
        assert!(XmlTree::parse(br#"<t x="a&#x1;b"/>"#).is_err());
        assert!(XmlTree::parse(br#"<t x="abc"/>"#).is_ok());
    }

    // Bug 4: prolog/epilog text bypassed UTF-8 validation, seemingly
    // contradicting parse()'s doc comment. Resolution: this is consistent
    // with every other `Node::Raw`-producing branch (comments/PIs/CDATA/
    // doctype/decl), none of which are UTF-8-validated either — so the doc
    // comment was the actual bug, not this behavior. Locked in here so it
    // can't silently drift.
    #[test]
    fn prolog_epilog_text_is_raw_passthrough_not_utf8_validated() {
        let mut xml = b"<a/> ".to_vec();
        xml.extend_from_slice(&[0xff, 0xfe]);
        let tree = XmlTree::parse(&xml).unwrap();
        assert_eq!(
            tree.serialize(),
            xml,
            "prolog/epilog Raw text must round-trip verbatim, invalid UTF-8 included"
        );
    }
}
