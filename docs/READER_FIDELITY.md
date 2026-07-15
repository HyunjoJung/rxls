# Reader fidelity contract

This matrix defines the metadata that rxls 0.1.2 retains and the places where a
format cannot yet be represented losslessly by the common model. “Partial” is a
deliberate, documented subset; it must not be interpreted as silent full
support.

| Surface | XLS (BIFF5/8) | XLSX/XLSM | XLSB | ODS |
| --- | --- | --- | --- | --- |
| Typed cells and display text | Yes | Yes | Yes | Yes |
| Formula source | Common BIFF tokens, shared/array definitions | Yes | Common BIFF12 tokens, shared/array definitions | ODF formula text |
| Global/local defined names | Yes | Yes | Yes | Global named ranges |
| Rich-text runs | BIFF8 `RSTRING` boundaries; font identity is inherited | Run boundaries and common font properties | Run boundaries; font identity is inherited | `text:span` boundaries; style identity is inherited |
| Number formats | Rendered through XF/FORMAT | Common built-ins and custom codes retained | Rendered through BrtXF/BrtFmt | Typed ODF values and visible text; style code not retained |
| Font/fill/border/alignment | Not retained as `CellStyle` | Common font, fill, border, alignment, protection subset | Not retained as `CellStyle` | Not retained as `CellStyle` |
| Merges and sheet visibility | Yes | Yes | Yes | Yes |
| Active sheet and tab color | Yes | Yes | Yes | Active sheet from settings; tab color when expressed by table style |
| Row/column size and hidden state | Yes for ROW/COLINFO | Yes | Yes for BrtRowHdr/BrtColInfo | Group outline/visibility only; physical sizes are not converted to character widths |
| Freeze panes and views | Common view fields | Yes | Common view fields | Active table only |
| Print area/setup | Common BIFF records and built-in names | Yes | Common records and built-in names | Common table/page-layout fields |
| Comments/notes and hyperlinks | Yes | Yes | Yes | Yes |
| Tables and validations | Validations; no BIFF ListObject model | Yes | Yes | Database ranges and validations |
| Non-worksheet sheet types | Classified and exposed as metadata | Classified and exposed | Classified and exposed | Tables are worksheets |

BIFF and XLSB external `NameX` references retain the original external-name
spelling when the workbook provides the corresponding name table. Formula text
keeps an explicit external-workbook marker (`[ixti:N]!Name`) so it cannot be
mistaken for a local defined name, and deterministic evaluation returns the
typed external-reference fallback. Missing or malformed external-name tables
remain explicit diagnostics rather than silently becoming local names.

## Text and codepage policy

BIFF5/7 declarations 949 and 51949 use the Windows-949-compatible decoder;
CP932/Shift-JIS, Windows-1252, and other mapped BIFF codepages use their named
decoder. Missing or unknown declarations fall back to Windows-1252. Malformed
byte sequences become U+FFFD. `Workbook::open_with_codepage` is the explicit
override for missing or incorrect declarations. BIFF8 strings are Unicode and
do not use this fallback.

The tracked `tests/fixtures/xls/korean-cp949-biff5.xls` fixture is a reduced,
Apache-2.0-licensed derivative of Apache POI `15556.xls`. It fixes the legacy
`Book` stream, `CODEPAGE=949`, decoded sheet name, and exact Korean cell text in
the committed oracle.

## Failure policy

Encrypted BIFF, OOXML, and ODF packages return format-specific encryption
errors. Invalid containers, missing workbook parts, malformed BIFF headers,
over-budget XML/entity input, and unsupported package data return an error or a
bounded documented fallback; they must never panic. Formula evaluation limits
each range traversal to 10,000 cells, expression recursion to 128 levels,
formula/defined-name dependency chains to 64 bodies, and each top-level
evaluation to 10,000 semantic operations shared across referenced formulas.
Budget exhaustion preserves the cached value with the typed
`dependency_depth_exceeded` or `operation_limit_exceeded` fallback reason. Text,
image, repeat, and package-part limits likewise prevent hostile compact inputs
from causing unbounded allocation. The negative and fuzz suites exercise these
contracts for every reader feature.

Unknown records or style properties are skipped only when record framing is
valid and the retained public value remains unambiguous. The table above is the
loss contract for those cases.
