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
| [`zip`](https://crates.io/crates/zip) *(optional, `xlsx`)* | MIT | zip-rs/zip2 |
| [`quick-xml`](https://crates.io/crates/quick-xml) *(optional, `xlsx`)* | MIT | tafia/quick-xml |

No third-party source is vendored into this crate.

## Format reference

This crate implements the publicly documented Microsoft Excel binary format
([MS-XLS]) and the OLE2 Compound File Binary format ([MS-CFB]). No Microsoft
source code or proprietary material is used; only the open specifications.

[MS-XLS]: https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xls/
[MS-CFB]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-cfb/
