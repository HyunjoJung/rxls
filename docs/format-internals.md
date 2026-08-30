# Format internals

[Back to README](../README.md)

rxls keeps format-specific parsing behind one typed `Workbook` model. Public
library, range, export, diagnostics, CLI, formula, editing, and WASM surfaces
consume that model instead of exposing parser-specific object graphs.

![rxls architecture: untrusted bytes pass through bounded format parsers into one typed model and public surfaces](../.github/assets/rxls-architecture-en.png)

## Detection and model

`Workbook::open` inspects container signatures rather than trusting a filename
extension. OLE2/CFB input is routed to the legacy BIFF reader. ZIP packages are
classified as OOXML, binary OOXML, or OpenDocument when their enabled reader
recognizes the package. Unsupported and malformed input returns a typed
`Error`.

All readers populate sheets and coordinates with the common `Cell` variants:
text, number, date serial, boolean, error, and formula with a cached value.
`Sheet::cell`, `cells`, `dimensions`, range facades, metadata accessors, text
extraction, and exports therefore work independently of the source parser.

## XLS: CFB and BIFF

An XLS file is an OLE2 compound document containing a `Workbook` stream for
BIFF8 or a `Book` stream for BIFF5/7. The reader:

1. opens the CFB container and locates the workbook stream;
2. walks global and worksheet BIFF substreams and detects the generation from
   the first `BOF` record;
3. decodes the BIFF8 shared string table, including strings split across
   `CONTINUE` records with compression changes at a boundary;
4. decodes BIFF5/7 8-bit text through the workbook `CODEPAGE` declaration;
5. converts label, shared-string, numeric, boolean/error, and formula records
   into common typed cells.

Codepage 949 (Windows Korean/UHC) and 51949 (EUC-KR) use the
Windows-949-compatible decoder from `encoding_rs`. Japanese cp932 and other
supported declarations follow the same path. Missing or unknown codepages fall
back to Windows-1252, malformed byte sequences become U+FFFD, and
`Workbook::open_with_codepage` can override a missing or incorrect declaration.
BIFF8 strings are Unicode and do not use the fallback.

Password-protected `FILEPASS` workbooks return `Error::Encrypted` instead of
exposing ciphertext. The bounded legacy XOR Method 1 path recognizes Excel's
default `VelvetSweatshop` password.

## XLSX and XLSM

The `xlsx` feature reads OOXML cell data, shared strings, styles, number formats,
relationships, and supported workbook/worksheet metadata through `zip` and
`quick-xml`. XLSX and XLSM share the read model.

`Spreadsheet` retains an OOXML package as name-keyed raw parts plus parsed
content-type and relationship metadata. A part stays raw until an edit promotes
it. Only changed parts are serialized; untouched declared parts remain their
original bytes. See [Preservation and editing](preservation.md) for the
fail-closed mutation contract.

The authoring path creates a new XLSX package from the writer model. It is
separate from the retained-package editing path, so opening a file never turns
all reader-discovered data into an implicit rewrite template.

## XLSB and ODS

The optional `xlsb` reader decodes supported binary workbook, worksheet,
shared-string, style, relationship, and metadata records into the common model.
It is read-only. Binary table, hyperlink, comment, validation, pane, page setup,
and other supported records surface through the same public metadata APIs used
by the other readers.

The optional `ods` reader walks bounded OpenDocument XML and package parts.
ODS display paragraphs are retained when present; typed office values provide
fallback text. Named database ranges, annotations, links, validation
conditions, hidden-sheet styles, print ranges, and supported document metadata
are mapped to the common model. ODS is read-only.

## Failure and recovery behavior

Failure is typed rather than a panic. Reads and allocations are bounds-checked.
Malformed structures either follow an explicit bounded recovery path or return
an error. `Workbook::parse_provenance` distinguishes the format's primary
container path from the tolerant CFB directory walk and exposes stable typed
recovery codes.

Recovery is an audit signal, not proof that the original container was valid or
complete. It never bypasses strict edit/save capability checks. In particular,
an OOXML package that opened with incomplete or metadata-lossy parts can remain
readable while package-preserving edits are refused.

Parsing, export, editing, diagnostics, and WASM paths enforce input, allocation,
recursion, range, or output limits appropriate to their surface. The crate uses
`#![forbid(unsafe_code)]`. CI adds dependency policy, CodeQL, fuzz
smoke/scheduled runs, deterministic dependency manifests, performance ceilings,
and exact-source release checks.

For the externally reproduced corpus and oracle results, see
[Validation and reproducibility](validation.md).
