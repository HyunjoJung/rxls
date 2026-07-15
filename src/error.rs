//! Error type for `.xls` parsing.

/// Errors produced while opening, decoding, exporting, or editing a spreadsheet.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The input is not an OLE2 / CFB compound file (`.xls` is OLE2-based).
    #[error("not an OLE2/CFB file (.xls must start with the D0CF11E0 magic)")]
    NotOle2,

    /// A raw pre-OLE2 Excel 2.0/3.0/4.0 stream (BIFF2–BIFF4) was detected.
    /// These predate the OLE2-wrapped `[MS-XLS]` format and are out of scope —
    /// only BIFF5/7 (`Book`) and BIFF8 (`Workbook`) workbooks are read.
    #[error("legacy Excel 2.0/3.0/4.0 stream (BIFF2-4) — unsupported; only BIFF5/7/8 is read")]
    LegacyBiff,

    /// The OLE2 container could not be opened.
    #[error("failed to open compound file: {0}")]
    Cfb(#[from] std::io::Error),

    /// An OLE2-looking package is too corrupt or truncated to expose a bounded
    /// workbook stream through either the strict CFB reader or tolerant fallback.
    #[error("invalid CFB package: {0}")]
    InvalidCfb(&'static str),

    /// Neither the `Workbook` (BIFF8) nor `Book` (BIFF5/7) stream was found.
    #[error("missing Workbook/Book stream")]
    MissingWorkbook,

    /// The BIFF record stream is malformed.
    #[error("malformed BIFF stream: {0}")]
    Biff(&'static str),

    /// A ZIP-based spreadsheet container could not be opened as a ZIP package.
    #[error("invalid ZIP package: {0}")]
    Zip(&'static str),

    /// A ZIP package entry uses a compression method not enabled by rxls.
    #[error("unsupported ZIP compression method {method} in part {part}")]
    UnsupportedCompression {
        /// Package part whose central-directory entry declares the method.
        part: String,
        /// ZIP compression method identifier.
        method: u16,
    },

    /// An OOXML part's XML tree (`xmltree::XmlTree`) could not be parsed or
    /// edited: malformed markup (mismatched/unclosed tags, invalid UTF-8, a
    /// malformed entity reference, a misplaced XML declaration) or a budget was
    /// exceeded (nesting depth, node count, attributes per element). Edits are
    /// rejected rather than repaired, so a damaged part is never silently
    /// rewritten into different content.
    #[error("malformed or over-budget xml: {0}")]
    Xml(&'static str),

    /// The workbook uses an unsupported `FILEPASS` encryption mode/password.
    /// Extraction is refused rather than emitting ciphertext.
    #[error("unsupported encrypted workbook (FILEPASS)")]
    Encrypted,

    /// The input is an OLE2-wrapped encrypted OOXML package (`EncryptedPackage`
    /// plus `EncryptionInfo`) rather than a readable BIFF workbook stream.
    #[error("unsupported encrypted OOXML package")]
    EncryptedPackage,

    /// The input is an encrypted OpenDocument package. The manifest advertises
    /// encrypted payload streams, but rxls does not decrypt password-protected ODF.
    #[error("unsupported encrypted OpenDocument package")]
    EncryptedOpenDocument,

    /// The workbook parsed but contained no indexable text.
    #[error("no indexable text")]
    NoText,

    /// The requested worksheet index does not exist or is not a grid worksheet.
    #[error("sheet index out of range")]
    SheetOutOfRange,
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;
