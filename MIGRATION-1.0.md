# Migration from 0.1.2 to 1.0.0

The target migration cost is **zero required source changes**. Version 0.1.2 is
the compatibility candidate for 1.0.0: its public paths, zero-based coordinate
model, inclusive ranges, feature behavior, MSRV, error policy, output contracts,
and supported format boundaries are intended to carry forward unchanged.

Before upgrading an application, verify these existing 0.1.2 contracts:

- Keep wildcard arms when matching `#[non_exhaustive]` errors, fallback reasons,
  or option enums so additive 1.x variants remain compatible.
- Use `Workbook::to_xlsx_checked` when invalid or over-budget authoring input
  must be rejected rather than sanitized by the legacy convenience path.
- Treat diagnose JSON as schema v1 and tolerate new warning strings; a shape or
  type change requires a new `schema_version`.
- Numeric worksheet coordinates are zero-based and range endpoints are
  inclusive. A1 references are file-format text, not numeric API coordinates.
- Check `Spreadsheet::edit_capability()` before package-preserving edits.
  Legacy XLS, XLSB, ODS, and metadata-lossy OOXML packages remain read-only.
- `Spreadsheet::workbook()` is the parsed-at-open view. Reopen bytes returned by
  `save()` or the file written by `save_to_path()` to inspect committed edits.
- Row/column insertion and deletion are intentionally not part of the 0.1.2/1.0
  editing contract; use cell/range edits or rebuild a workbook when structural
  dependency rewriting is required.
- The synchronous WASM binding rejects inputs larger than 32 MiB. Use a worker
  and the documented native/streaming alternatives for larger workloads. The
  packaged JavaScript runtime requires Node.js 20 or newer.

No public item is currently scheduled for removal, rename, or signature change
in 1.0.0. If observation of 0.1.2 exposes a required breaking correction, it
must be recorded here and in `CHANGELOG.md` before the 1.0.0 release decision;
otherwise this document will remain a no-op migration note.

See [`API-COMPATIBILITY.md`](API-COMPATIBILITY.md),
[`EDITING-CONTRACT.md`](EDITING-CONTRACT.md),
[`OUTPUT-CONTRACTS.md`](OUTPUT-CONTRACTS.md),
[`PERFORMANCE.md`](PERFORMANCE.md), and
[`docs/READER_FIDELITY.md`](docs/READER_FIDELITY.md) for the complete
compatibility boundary.
