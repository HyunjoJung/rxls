# Third-Party Licenses

`rxls` is distributed under the MIT License (see [`LICENSE`](LICENSE)). It links
the following third-party crates, all under permissive licenses compatible with
MIT.

## Direct dependencies

| Crate | License | Repository |
| --- | --- | --- |
| [`cfb`](https://crates.io/crates/cfb) | MIT | mdsteele/rust-cfb |
| [`encoding_rs`](https://crates.io/crates/encoding_rs) | (MIT OR Apache-2.0) AND BSD-3-Clause | hsivonen/encoding_rs |
| [`thiserror`](https://crates.io/crates/thiserror) | MIT OR Apache-2.0 | dtolnay/thiserror |
| [`zip`](https://crates.io/crates/zip) *(optional, `xlsx`/`xlsb`/`ods`)* | MIT | zip-rs/zip2 |
| [`quick-xml`](https://crates.io/crates/quick-xml) *(optional, `xlsx`/`ods`)* | MIT | tafia/quick-xml |
| [`serde`](https://crates.io/crates/serde) *(optional)* | MIT OR Apache-2.0 | serde-rs/serde |
| [`chrono`](https://crates.io/crates/chrono) *(optional)* | MIT OR Apache-2.0 | chronotope/chrono |

No third-party source code is vendored into this crate. The tracked reader
fixture `tests/fixtures/xls/korean-cp949-biff5.xls` is a reduced data derivative
of Apache POI's Apache-2.0-licensed `15556.xls`; its immutable source revision,
source hash, transformation, and output hash are recorded in
`tests/fixtures/README.md` and `tests/fixtures/MANIFEST.json`.

The repository-only `bindings/wasm` package additionally depends directly on
[`wasm-bindgen`](https://crates.io/crates/wasm-bindgen), licensed under MIT OR
Apache-2.0. That binding package is excluded from the published `rxls` crate.

## Format reference

This crate implements the publicly documented Microsoft Excel binary formats
([MS-XLS], [MS-XLSB]), OOXML SpreadsheetML ([ECMA-376]), the OLE2 Compound File
Binary format ([MS-CFB]), and OpenDocument Spreadsheet ([ODF]). No Microsoft or
OASIS source code or proprietary material is used; only open specifications.

[MS-XLS]: https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xls/
[MS-XLSB]: https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xlsb/
[MS-CFB]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-cfb/
[ECMA-376]: https://ecma-international.org/publications-and-standards/standards/ecma-376/
[ODF]: https://docs.oasis-open.org/office/OpenDocument/v1.3/
