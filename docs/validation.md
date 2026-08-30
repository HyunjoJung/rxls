# Validation and reproducibility

[Back to README](../README.md)

The published claims are based on public fixtures, pinned upstream corpora, and
release artifacts bound to one source revision. Other spreadsheet readers are
used as visible-value oracles; they are not presented here as marketing
benchmarks.

## Public corpus baseline

<!-- public-corpus-baseline:start -->
**Current public-corpus gate (2026-08-22).** The pinned fetch recipe selects 916
files from Apache POI and calamine at immutable upstream commits: 448 `.xls`,
413 `.xlsx`, 18 `.xlsm`, 21 `.xlsb`, and 16 `.ods`. `rxls corpus-report` opens
868; the remaining 48 are explicit expected rejections for encrypted input,
unsupported legacy BIFF, malformed containers, structurally invalid BIFF streams, or malformed OOXML package relationships.
The report records 0 unexpected failures and 0 unexpected accepts. Public visible-value checks report:

| Format | Comparable files | Result |
|---|---:|---:|
| `.xls` vs `xlrd` | 414 | 100.000% mean parity; 414/414 at least 99% |
| `.xlsx`/`.xlsm` vs `openpyxl` | 387 | 100.000% mean parity; 387/387 at least 99% |
| `.xlsb` vs `pyxlsb` plus committed residual oracles | 18 | 100.000% mean parity |
| `.ods` vs bounded ODF XML visible-text oracle | 14 | 100.000% mean recall |
<!-- public-corpus-baseline:end -->

The corpus manifest records 916 eligible files: 448 XLS, 413 XLSX, 18 XLSM, 21
XLSB, and 16 ODS. Of the 48 expected rejections, each exact path and rejection
class is checked in. The gate fails on an unexpected failure, an unexpected
accept, a changed count, or parity below the recorded baseline.

Each parity report records the oracle reader and installed version plus the
SHA-256 of the exact input-manifest bytes. Directory-only development runs
explicitly report `input_manifest_sha256=none`; release evidence requires the
pinned manifest and a valid SHA-256.

## Release contract

Version `0.1.3` is accepted only when the crate, tagged source, GitHub Release
bundle, SBOM, checksums, and provenance are bound by the release manifest to one
revision. The immutable `v0.1.3` tag identifies source commit
`e1390e5aa349fbf933c39bccda400a4a2ee1d814`. Later `main` revisions may contain
documentation and unreleased work.

The [52-asset v0.1.3 release bundle](https://github.com/HyunjoJung/rxls/releases/tag/v0.1.3)
contains the crate, WASM package, checksums, SBOM, public-corpus reports,
performance evidence, fuzz evidence, LibreOffice edit smoke results,
publication dry-run receipt, candidate comparison, attestation, and final
release manifest. The manifest covers every asset and stores its SHA-256.
Post-publication verification downloads the bundle, verifies complete manifest
coverage and checksums, installs the exact crate, and runs Node/browser WASM
consumers.

The release workflow creates two clean candidates from the same commit. A
fail-closed comparator requires deterministic artifacts to match and permits
only documented test-duration, successful fuzz-log, and bounded performance
noise. Tag publication requires the exact-SHA CI and security gates, the
candidate attestation, a clean tag-to-candidate comparison, and a hosted
`cargo publish --dry-run` receipt.

## Local release gates

The core validation sequence runs from a clean checkout and uses no private
data:

```bash
python3 -m pip install \
  "CairoSVG==2.9.0" "numpy==2.4.4" "openpyxl==3.1.5" \
  "Pillow==12.3.0" "pyxlsb==1.0.10" "xlrd==2.0.2" "odfpy==1.4.1"
python3 scripts/public_hygiene_audit.py
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
RXLS_REQUIRE_OPENPYXL=1 cargo test --all-targets --all-features --locked
cargo test --no-default-features --all-targets --locked
cargo test --doc --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features --locked
python3 -m unittest discover -s scripts -p "test_*.py"
cargo package --locked
python3 scripts/check_core_package.py target/package/rxls-0.1.3.crate
cargo publish --dry-run --locked
```

Test the exact packaged crate as an external dependency and installed CLI:

```bash
python3 scripts/smoke_crate_distribution.py \
  --crate target/package/rxls-0.1.3.crate \
  --fixture tests/fixtures/xlsx/reader-structural.xlsx
```

## Full public-corpus run

```bash
python3 scripts/fetch-public-corpus.py --dry-run
python3 scripts/fetch-public-corpus.py
cargo build --all-features --example extract --locked
cargo run --bin rxls --all-features --locked -- corpus-report local/public-corpus/manifest.json | tee target/release-corpus-report.txt
python3 scripts/xls-xlrd-parity.py --manifest local/public-corpus/manifest.json --bin target/debug/examples/extract --corpus-report target/release-corpus-report.txt --min 0.99 --show-worst 20 --show-skips 200 | tee target/release-xls-parity-full.txt
python3 scripts/xlsx-openpyxl-parity.py --manifest local/public-corpus/manifest.json --bin target/debug/examples/extract --corpus-report target/release-corpus-report.txt --min 0.99 --show-worst 20 --show-skips 200 | tee target/release-ooxml-parity-full.txt
python3 scripts/xlsb-pyxlsb-parity.py --manifest local/public-corpus/manifest.json --bin target/debug/examples/extract --expected-values tests/oracles/xlsb-visible-values.json --corpus-report target/release-corpus-report.txt --min 0.99 --show-skips 200 | tee target/release-xlsb-parity-full.txt
python3 scripts/ods-odfpy-parity.py --manifest local/public-corpus/manifest.json --bin target/debug/examples/extract --corpus-report target/release-corpus-report.txt --min 0.99 --show-skips 200 | tee target/release-ods-parity-full.txt
python3 scripts/verify_public_baseline.py \
  --corpus-report target/release-corpus-report.txt \
  --xls target/release-xls-parity-full.txt \
  --ooxml target/release-ooxml-parity-full.txt \
  --xlsb target/release-xlsb-parity-full.txt \
  --ods target/release-ods-parity-full.txt \
  --readme README.md \
  --readme-ko README.ko.md \
  --validation-doc docs/validation.md
```

The dry run must report 916 files. Downloads go to the gitignored
`local/public-corpus` directory; the repository commits the pinned fetch recipe,
manifest expectations, and documentation rather than the corpus payloads.

## Demo evidence

The English and Korean demos run the `v0.1.3` CLI from the exact release source,
open all four read formats through the common model, create a styled six-row
XLSX report, inspect its table, filter, cached `SUM` formula, data validation,
and chart in Excel 16, and reopen it with `openpyxl 3.1.5`.

[English captions](https://github.com/HyunjoJung/rxls/releases/download/oss-contest-2026-demo/rxls-2026-oss-contest-demo.en-US.srt) ·
[Korean captions](https://github.com/HyunjoJung/rxls/releases/download/oss-contest-2026-demo/rxls-2026-oss-contest-demo.ko.srt) ·
[English build receipt](https://github.com/HyunjoJung/rxls/releases/download/oss-contest-2026-demo/video-verification.en-US.json) ·
[Korean build receipt](https://github.com/HyunjoJung/rxls/releases/download/oss-contest-2026-demo/video-verification.json) ·
[English QA](https://github.com/HyunjoJung/rxls/releases/download/oss-contest-2026-demo/video-qa.en-US.json) ·
[Korean QA](https://github.com/HyunjoJung/rxls/releases/download/oss-contest-2026-demo/video-qa.json) ·
[media release](https://github.com/HyunjoJung/rxls/releases/tag/oss-contest-2026-demo)

This media release is intentionally separate from the immutable `v0.1.3` core
release-evidence bundle.

## Rendering evidence

Rendering is a separate source-only track and is not part of the published
`rxls 0.1.3` reader, writer, editor, CLI, or core WASM contract. The renderer
uses locked LibreOffice and font inputs, deterministic SVG/PDF/PNG outputs,
bounded campaigns, and a reviewed full-campaign parity baseline. Its workflow
and package have independent supply-chain, browser, fidelity, performance, and
release gates.

See the [renderer guide](https://github.com/HyunjoJung/rxls/blob/main/render/README.md),
[worker guide](https://github.com/HyunjoJung/rxls/blob/main/bindings/render-wasm/README.md),
and [full-campaign baseline](https://github.com/HyunjoJung/rxls/blob/main/scripts/render-parity-baseline-full.json)
for the current source contract.
