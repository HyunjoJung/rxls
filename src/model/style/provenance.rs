//! Reader style-fidelity and loss provenance.

/// Fidelity of worksheet style information retained in the public model.
///
/// This signal is intentionally sheet-scoped because some container formats can
/// mix worksheet kinds and reader capabilities. Consumers such as renderers
/// should surface a warning for [`Self::Partial`] or [`Self::Unavailable`]
/// instead of assuming that absent style data means an unformatted source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StyleFidelity {
    /// The reader did not retain source cell styles for this sheet.
    #[default]
    Unavailable,
    /// A documented, useful subset of source styles was retained.
    Partial,
    /// Every source style property represented by the public model was retained.
    Retained,
    /// The sheet was created through rxls authoring APIs, so its model is the source.
    Authored,
}

/// A bounded, typed reason why source styling could not be represented exactly.
///
/// Readers aggregate identical reasons per sheet. This keeps hostile documents
/// from creating an unbounded warning list while allowing renderers to explain
/// why [`StyleFidelity::Partial`] was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StyleLossKind {
    /// A source property has no equivalent in rxls' public style model.
    UnsupportedProperty,
    /// A referenced source style was absent or outside the retained table.
    MissingReference,
    /// A parent-style cycle was detected and cut at the bounded resolver depth.
    InheritanceCycle,
    /// A format-defined or rxls safety limit was reached.
    LimitExceeded,
    /// Color information depended on an unavailable palette or theme entry.
    UnresolvedColor,
    /// Drawing/chart metadata was retained only partially.
    DrawingMetadataPartial,
}

/// One aggregated source-style loss boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct StyleLoss {
    /// Stable typed reason.
    pub kind: StyleLossKind,
    /// Number of occurrences, saturated at [`u32::MAX`].
    pub occurrences: u32,
}
