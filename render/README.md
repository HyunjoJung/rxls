# rxls-render

`rxls-render` converts worksheet ranges into backend-neutral, fixed-point scenes
and deterministic SVG. Its optional print pipeline paginates the same scene
commands and emits per-page SVG, one multi-page PDF, and bounded per-page PNG.
Rows, columns, cells, pages, text shaping, line breaking, outlined paths, scene
nodes, backend commands, dimensions, pixels, and serialized output all have
explicit resource limits.

Worksheet layout applies the typed conditional-format subset retained by
`rxls`: literal or relative/absolute A1 `CellIs` comparisons, two- and
three-color scales, top/bottom ranks (including ties and percent ranks),
above/below-average fills, duplicate/unique values, and bounded binary A1
expressions. Data bars are emitted as deterministic solid bars and carry an
explicit gradient-simplification warning. Unsupported expression syntax or
unresolved operands remain explicit deferred warnings instead of being guessed.
Numeric and date displays that cannot fit their cell become `#` runs; wrap and
shrink-to-fit continue to take precedence.
Verified-font renders convert retained column character widths through the
source workbook's resolved default-font maximum digit width. OOXML sheets keep
explicit `defaultColWidth`/`cchDefColWidth`, authored `baseColWidth` provenance,
and Calc's 8.5-character application default as distinct cases; non-OOXML
sheets with no width metadata use the renderer's 10-character default. Fontless
fallback scenes retain the configured fixed pixel width and report approximate
text metrics.

Images, charts, and sparklines share the same bounded scene as cells. PNG/JPEG
images are dimension-preflighted before decode and retain bounded crop,
rotation, alpha, alternate text, anchors, absolute size, and source stacking
order. Supported bar, line, pie, and scatter charts resolve bounded cached/A1
series, axes, legends, titles, and labels; line/column/win-loss sparklines use
the same point budget. Unsupported image encodings, chart kinds, series, and
unanchored shapes become deterministic placeholders with typed warnings rather
than silent omissions.

For reproducible typography, pass an explicitly acquired and verified OFL font
pack. The renderer verifies every declared file size and SHA-256, performs
Unicode bidirectional shaping and fallback only from the owned pack bytes, and
emits glyph outlines instead of relying on fonts installed on the host. The
checked-in lock pins regular, bold, italic, and bold-italic Carlito, Arimo,
Tinos, Cousine, and Caladea faces from their primary Google Fonts upstream
repositories. Manifest aliases substitute Calibri, Arial/Helvetica, Times New
Roman, Courier New, and Cambria without mislabeling the actual selected family;
LibreOffice's Liberation Sans/Serif/Mono requests resolve to the corresponding
metric-compatible faces. Noto CJK, Arabic, and Hebrew faces remain the ordered
script fallback. The
generated `fonts.conf`, alias table, license files, and every selected face are
part of the path-independent pack identity and fail offline verification if
changed.

API callers may set `RenderOptions::font_pack` to a verified caller pack and
then call `RenderOptions::with_fallback_font_pack` with the pinned OFL pack.
Exact family matches are resolved caller-first across the stack before aliases,
and an unknown family uses the final fallback pack's default face. Filesystem
and in-memory packs use the same resolution rules and never discover host fonts.
Render report schema 2 records the effective pack or stack SHA-256 plus the
source-pack and face SHA-256, actual family, weight, italic state, and
substitution status of every selected face. `FontPack::load_caller_manifest` and
`FontPack::load_caller_memory` retain all verification and isolation checks for
fonts the caller is independently authorized to use; the existing loaders
remain strictly OFL-only.

The same shaping pass retains rich-text run family, size, color, bold, italic,
underline, strike-through, and superscript/subscript properties with cell-font
fallback. Unicode line breaking and wrapping operate across run boundaries;
indentation, shrink-to-fit, horizontal and vertical alignment, rotation,
clipping, and legal empty-cell overflow remain cell-level policies. Each
outlined node records bounded UTF-8 source-cluster to path-command ranges and
contiguous paint spans, including ligatures, combining sequences, fallback
faces, CJK, and bidirectional visual order. SVG, PDF, and PNG replay those same
scene commands and colors. PDF additionally uses Unicode `ActualText` plus a
deterministic embedded Type3 subset whose bounded source-cluster glyph programs,
non-zero widths, color spans, and `ToUnicode` maps come directly from those
retained outlines, without consulting host fonts. The bundle manifest records
the exact path-independent font-pack SHA-256.
Sandboxed and WASM callers can supply the same manifest and owned virtual files
through `FontPack::load_memory`; it applies identical bounds and hashes, rejects
missing/extra/unsafe members, and exposes verified per-face SHA-256 identities
without touching a filesystem.

```sh
cargo run --manifest-path render/Cargo.toml -- \
  bundle tests/fixtures/xls/reader-basic.xls \
  --output-dir /tmp/rxls-render \
  --font-pack-manifest local/render-fonts/pack/manifest.json
```

The pinned pack can be acquired with
`python3 scripts/fetch-render-fonts.py --acquire`; payloads remain under the
ignored `local/render-fonts` directory. If no pack is supplied, SVG text remains
available through the bounded approximate-metrics fallback and the render report
records that approximation. Rich runs, wrapping, shrink-to-fit, and font scripts
that cannot be represented faithfully by this fallback retain explicit typed
warnings instead of being reported as exact.

The bundle contains ordered `sheet-0000.svg` files and a deterministic
`render-manifest.json` with source, scene, output, and optional font-pack hashes.
Print artifacts are opt-in, so the original SVG bundle contract and filenames
remain unchanged:

```sh
cargo run --manifest-path render/Cargo.toml -- \
  bundle tests/fixtures/xls/reader-basic.xls \
  --output-dir /tmp/rxls-print \
  --font-pack-manifest local/render-fonts/pack/manifest.json \
  --print-backends svg,pdf,png \
  --png-dpi 144
```

For LibreOffice `SinglePageSheets` differential runs, add
`--single-page-sheets`. This opt-in override emits the selected visible sheet
scene at 100% on one content-sized page. Like LibreOffice, it ignores authored
paper, orientation, margins, scale, print headings, repeated titles, and
headers/footers. The `single_page_sheets` override is recorded in both the page
report and bundle manifest. Default authored pagination is unchanged.

Print layout honors the retained print area, paper/orientation, margins,
percentage or fit-to-page scaling, repeated title rows and columns, horizontal
and vertical centering, print gridlines/headings, and headers/footers. Page maps
and typed approximations are written to `sheet-0000-pages.json`. PDF files have
fixed metadata and IDs, path-safe link annotations, and Unicode `ActualText`;
verified font glyphs are embedded as deterministic Type3 source-cluster subsets
and remain the exact outlines and colors used by SVG and PNG. PNG output requires
a verified font pack whenever a scene contains text, and each page is preflighted
and rasterized independently at the requested DPI.

The nested crate enables the `rxls` XLSX, XLSB, and ODS readers without the core
CLI, so the renderer accepts every spreadsheet format supported by `rxls` while
remaining isolated from the main crate's default dependency surface.
