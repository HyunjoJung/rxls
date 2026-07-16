//! Typed, bounded provenance for successful workbook parsing.

/// Maximum number of recovery codes retained for one successful parse.
///
/// The current readers emit at most one code. The explicit cap is part of the
/// public contract so future recovery paths cannot turn diagnostics into an
/// unbounded allocation or output surface.
pub const MAX_PARSE_RECOVERY_CODES: usize = 16;

/// Container path used to produce a workbook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ContainerParseMode {
    /// The workbook was authored in memory, so no source-container parse ran.
    #[default]
    NotApplicable,
    /// The format's primary container reader succeeded without an rxls
    /// recovery path.
    ///
    /// For `.xls`, this means `cfb::CompoundFile::open` succeeded. It does not
    /// certify full [MS-CFB] conformance or workbook validity.
    Primary,
    /// The primary CFB reader rejected the `.xls`, but the bounded rxls
    /// directory scan recovered an intact `Workbook` or `Book` stream.
    TolerantCfbDirectoryWalk,
}

impl ContainerParseMode {
    /// Stable lowercase code used by machine-readable diagnostics.
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotApplicable => "not_applicable",
            Self::Primary => "primary",
            Self::TolerantCfbDirectoryWalk => "tolerant_cfb_directory_walk",
        }
    }
}

/// Stable reason that a successful parse used a bounded recovery path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecoveryCode {
    /// A linear, bounds-checked CFB directory scan recovered the workbook
    /// stream after the primary container reader rejected the directory tree.
    TolerantCfbDirectoryWalk,
}

impl RecoveryCode {
    /// Stable lowercase code used by machine-readable diagnostics.
    pub const fn code(self) -> &'static str {
        match self {
            Self::TolerantCfbDirectoryWalk => "tolerant_cfb_directory_walk",
        }
    }
}

/// Deterministic provenance retained after a successful workbook parse.
///
/// The structure never contains source bytes, paths, names, or hashes. Recovery
/// codes are emitted in stable reader-precedence order and are capped by
/// [`MAX_PARSE_RECOVERY_CODES`]. A recovered parse is an audit signal, not a
/// guarantee that the original container was valid.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ParseProvenance {
    /// Container path that produced the workbook.
    pub container: ContainerParseMode,
    /// Stable recovery codes, in deterministic reader-precedence order.
    recoveries: Vec<RecoveryCode>,
    /// Whether additional recovery codes were omitted at the public cap.
    recoveries_truncated: bool,
    /// Whether bounded extraction omitted data, currently because the global
    /// text budget was exhausted.
    pub partial: bool,
}

impl ParseProvenance {
    pub(crate) fn from_state(container: ContainerParseMode, partial: bool) -> Self {
        let mut recoveries = Vec::new();
        if matches!(container, ContainerParseMode::TolerantCfbDirectoryWalk)
            && recoveries.len() < MAX_PARSE_RECOVERY_CODES
        {
            recoveries.push(RecoveryCode::TolerantCfbDirectoryWalk);
        }
        Self {
            container,
            recoveries,
            recoveries_truncated: false,
            partial,
        }
    }

    /// `true` when at least one bounded recovery path contributed to success.
    pub fn is_recovered(&self) -> bool {
        !self.recoveries.is_empty()
    }

    /// Stable recovery codes in deterministic reader-precedence order.
    pub fn recoveries(&self) -> &[RecoveryCode] {
        &self.recoveries
    }

    /// `true` if recovery codes were omitted at
    /// [`MAX_PARSE_RECOVERY_CODES`].
    pub fn recoveries_truncated(&self) -> bool {
        self.recoveries_truncated
    }
}
