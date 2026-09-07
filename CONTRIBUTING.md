# Contributing to rxls

Thanks for your interest in improving `rxls` — a native Rust spreadsheet
library that reads `.xls` (BIFF8/5/7), `.xlsx`, `.xlsb`, and `.ods`, writes
and package-preservingly edits `.xlsx`/`.xlsm`, evaluates a deterministic
formula subset, and exports CSV/HTML/Markdown.

## Your first contribution

Start with a [good first issue] or browse [help wanted]. Each task describes
the relevant files, expected result, and a focused check. Documentation,
usage examples, small reproductions, and tests are welcome contributions;
you do not need to understand every spreadsheet format to help.

1. Read the issue and its comments, then leave a short comment if you plan to
   work on it so others can coordinate. Ask for pointers when the scope is
   unclear. Small documentation corrections can go straight to a PR.
2. Fork the repository and create a branch from `main`. Keep the change about
   one problem. For a bug fix, add a small regression test that fails before
   the fix and passes afterward.
3. Open a PR against `main` with the problem, the change, and the exact checks
   you ran. A draft PR with a reproduction or a specific question is useful
   even when the fix is unfinished.

Run the checks for your changed area below before opening a PR. You do not
need to reproduce the full release matrix to request review. State any check
you could not run and why; passing the applicable CI and additional
maintainer-run gates is still required before merge or publication.

If none of the listed tasks fits, open a [contribution question] with the area
you want to work on. For larger features, discuss the user need and scope in
an issue before implementation. Please follow the [Code of Conduct].

## Set up only what you need

For core Rust changes, install [Rust through rustup], then run from the
repository root:

```sh
rustup toolchain install 1.85.0 --profile minimal --component rustfmt --component clippy
cargo +1.85.0 test --lib --all-features --locked
```

This uses the committed lockfile and does not need Excel, Java, LibreOffice,
Node.js, or an external workbook corpus. The first build downloads Rust
dependencies. On macOS, put rustup's proxies before any Homebrew Rust tools
with `export PATH="$HOME/.cargo/bin:$PATH"`.

For documentation-only edits, start with your editor and `git diff --check`.
Check links and run any example you change. Python repository checks require
Python 3.11 or newer.

For viewer or VS Code work, use Node.js 24.18.0 with npm 11.16.0, matching the
hosted tooling. Install only the package you are changing with
`npm ci --prefix viewer --ignore-scripts` or
`npm ci --prefix extensions/vscode --ignore-scripts`.

The core and renderer support Rust 1.85. For the separate MCP adapter, install
its Rust 1.88 toolchain and use `cargo +1.88.0` for that package:

```sh
rustup toolchain install 1.88.0 --profile minimal --component rustfmt --component clippy
```

Browser and installed-extension tests have extra prerequisites documented in
their package guides.

## Choose a focused check

All commands below run from the repository root. For Rust changes, also run
`cargo +1.85.0 fmt --all -- --check` (or the changed package's `cargo fmt`).

| Changed area | Start here | More context |
| --- | --- | --- |
| Prose and links | `git diff --check`; check links and any changed example | [README](README.md), [public docs](docs/compatibility.md) |
| Core readers, writer, editor, or formulas | `cargo +1.85.0 test --lib --all-features --locked` | `src/`, [format internals](docs/format-internals.md) |
| CLI | `cargo +1.85.0 test --test cli --all-features --locked` | `src/main.rs`, `tests/cli.rs` |
| Public API or fixture regressions | `cargo +1.85.0 test --test integration --test api_contract --all-features --locked` | `tests/`, [fixture provenance](tests/fixtures/README.md) |
| Renderer | `cargo +1.85.0 test --manifest-path render/Cargo.toml --all-targets --locked` | [Renderer guide](render/README.md) |
| Viewer application logic | `npm --prefix viewer test` | [Viewer setup and browser tests](viewer/README.md) |
| Render worker JavaScript | `npm --prefix bindings/render-wasm test` | [Worker setup and WASM tests](bindings/render-wasm/README.md) |
| MCP adapter | `cargo +1.88.0 test --manifest-path bindings/mcp/Cargo.toml --locked` | [MCP guide](bindings/mcp/README.md) |
| VS Code extension | `npm --prefix extensions/vscode test` | [Extension setup and E2E tests](extensions/vscode/README.md) |

You can append a test-name filter to a Cargo test command while iterating.
Check that the output reports the intended test ran; a result with zero tests
does not verify the change. Run the containing suite before requesting review.
User-visible browser or extension changes also need their package's browser
or E2E journey; unit tests alone do not check the rendered interface.

Changes to readers, XML/ZIP handling, dependencies, or resource limits may
need corpus, fuzz, or rendering-oracle checks. Describe the affected formats
in the PR so maintainers can select the appropriate hosted runs. Contributors
do not need registry credentials or access to maintainer release environments.

## Reproductions and workbook fixtures

A small generated workbook or a test built from in-memory bytes is often the
best reproduction. Include the command or API call, enabled features, and
expected versus actual result. An independent reader comparison helps when
available, but is not required to report an ordinary bug.

For contributed files, record the source, license, producing application when
known, and expected values. Follow the [fixture guide](tests/fixtures/README.md)
for committed generated fixtures and their manifest. Include only files you
may redistribute. Remove confidential data, including hidden sheets,
properties, comments, and macros, before sharing a workbook; recreating the
problem with synthetic data is preferable.

Report exploitable crashes, unbounded resource use, and other vulnerabilities
through the [private security reporting channel], following the
[Security Policy](.github/SECURITY.md).

## Ground rules

- **No `unsafe`.** The crate is `#![forbid(unsafe_code)]`. Parsing untrusted
  files must never crash a host process — every byte access is bounds-checked
  and malformed input must never panic. Recovery must be bounded and preserve
  source meaning; otherwise the input must surface as an
  [`Error`](https://docs.rs/rxls/latest/rxls/enum.Error.html).
- **Document every public item.** The crate denies `missing_docs`.
- **Keep dependencies minimal.** The default build depends only on `cfb`,
  `encoding_rs`, `thiserror`, `zip`, and `quick-xml` (the latter two behind
  the default-on `xlsx` feature); `--no-default-features` drops to the
  `.xls`-only trio. New dependencies need a strong justification.
- **Follow the spec.** Behaviour should be traceable to [MS-XLS] / [MS-XLSB] /
  [MS-CFB] / ECMA-376 (SpreadsheetML) / ODF 1.2. Cite the relevant section in
  comments when implementing record or element details.
- **Bounded everything.** Adversarial input is a first-class concern: depth,
  node-count, part-size, and total-allocation budgets must hold on both the
  read and write/edit paths, and edits must be preflighted so a failed edit
  never leaves a half-mutated package.

## Full integration and release validation

Maintainers and contributors working across multiple surfaces use the full
matrix below before integration or release, together with the applicable
hosted checks. It is a reference for broader validation, not a prerequisite
for opening a focused or draft PR.

Install the pinned registry-compatibility checker with
`cargo install cargo-semver-checks --version 0.49.0 --locked`, ensure the
`wasm32-unknown-unknown` Rust target is installed, and install the oracle
dependencies described in [Validation and reproducibility](docs/validation.md).
Run the full local gate; all applicable checks must pass clean:

On macOS, put rustup's cargo proxy before any Homebrew Rust installation so
`cargo +1.85.0`, `cargo +nightly`, and the API checker use the pinned toolchain:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo +1.85.0 -V
```

```sh
python3 scripts/public_hygiene_audit.py
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo clippy --all-targets --no-default-features --features cli --locked -- -D warnings
cargo clippy --all-targets --no-default-features --features cli,xlsb --locked -- -D warnings
cargo clippy --all-targets --no-default-features --features cli,ods --locked -- -D warnings
RXLS_REQUIRE_OPENPYXL=1 cargo test --all-targets --all-features --locked
cargo test --no-default-features --all-targets --locked
cargo test --doc --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features --locked
python3 scripts/check_public_api.py
cargo semver-checks check-release --manifest-path Cargo.toml \
  --baseline-version 0.1.2 --release-type patch --all-features
cargo semver-checks check-release --manifest-path Cargo.toml \
  --baseline-version 0.1.2 --release-type patch --default-features
cargo semver-checks check-release --manifest-path Cargo.toml \
  --baseline-version 0.1.2 --release-type patch --only-explicit-features
cargo fmt --manifest-path render/Cargo.toml -- --check
cargo clippy --manifest-path render/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path render/Cargo.toml --all-targets --locked
cargo fmt --manifest-path bindings/render-wasm/Cargo.toml -- --check
cargo clippy --manifest-path bindings/render-wasm/Cargo.toml --all-targets --locked -- -D warnings
cargo clippy --manifest-path bindings/wasm/Cargo.toml --all-targets \
  --target wasm32-unknown-unknown --locked -- -D warnings
cargo clippy --manifest-path bindings/render-wasm/Cargo.toml --all-targets \
  --target wasm32-unknown-unknown --locked -- -D warnings
cargo test --manifest-path bindings/render-wasm/Cargo.toml --locked
npm --prefix bindings/render-wasm test
python3 -m unittest discover -s scripts -p "test_*.py"
python3 scripts/libreoffice-render-parity.py --corpus tests/fixtures \
  --dry-run --max-files 8 \
  --report target/libreoffice-render-parity-dry-run.json
cargo package --locked
python3 scripts/check_core_package.py target/package/rxls-0.1.3.crate
cargo publish --dry-run --locked
```

## Tests

- Unit tests build minimal in-memory structures (BIFF records, OOXML zip
  packages, XML trees) so parsers and editors are exercised without large
  binary fixtures; small committed fixtures under `tests/fixtures/` cover
  each container format end to end.
- The trickiest reader area is the SST: shared strings that span `CONTINUE`
  records and re-specify their compression flag at the boundary. Any change
  there must keep the split-string test green.
- The trickiest edit-path invariant is byte preservation: a no-op
  open → save must reproduce every untouched part byte-for-byte, and an edit
  may only rewrite the parts it actually touched.
- Fuzz targets (`fuzz/`) cover parsing, authoring, package-preserving edits,
  and formula decompilation/evaluation. Run all four locally with:

  ```sh
  for target in parse author edit formula; do
    cargo +nightly fuzz run "$target" -- -max_total_time=20
  done
  ```

  Pull requests run a bounded smoke; scheduled, manual, and release-candidate
  campaigns retain per-target diagnostics.
- The standalone renderer lives under `render/`. Its tests cover one
  backend-neutral fixed-point scene, deterministic SVG/PDF/PNG replay,
  authored pagination, typography and style resolution, drawings/charts,
  bounded failures, and atomic bundles. The separate browser package under
  `bindings/render-wasm/` exercises the same engine through a CSP-safe worker.
  Primary CI runs the LibreOffice parity harness in dependency-free preflight
  mode. The render-oracle workflow builds and exercises the pinned,
  network-isolated LibreOffice container, exact OFL font pack, and locked
  Poppler tools. Full campaigns compare visual, semantic, text-box, edge, page,
  and authored-print geometry metrics without retaining workbook text or raw
  rendered pages in uploaded evidence.

## Release candidates

Release publication is maintainer-controlled. Do not create a release tag or
publish a package from a contributor branch. Maintainers run the hosted
reproducibility, exact-commit CI and CodeQL, artifact-integrity, and installed
consumer gates before publishing.

## Scope

Reading targets faithful typed-cell extraction with display-formatted text;
formula evaluation is limited to the deterministic subset exposed by
`Workbook::evaluate_cell` (everything else falls back to the cached value
with a typed reason). Editing is `.xlsx`/`.xlsm`-only and package-preserving.
Excel custom number-format sections are rendered within explicit locale and
output bounds. Complete cross-format styling semantics, macro execution, and
pivot-table semantics are not promised; unmodeled parts are preserved rather
than interpreted. Larger features are welcome — open an issue first.

[MS-XLS]: https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xls/
[MS-XLSB]: https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xlsb/
[MS-CFB]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-cfb/
[good first issue]: https://github.com/HyunjoJung/rxls/issues?q=is%3Aissue%20is%3Aopen%20label%3A%22good%20first%20issue%22
[help wanted]: https://github.com/HyunjoJung/rxls/issues?q=is%3Aissue%20is%3Aopen%20label%3A%22help%20wanted%22
[contribution question]: https://github.com/HyunjoJung/rxls/issues/new?template=contribution_question.md
[Code of Conduct]: .github/CODE_OF_CONDUCT.md
[Rust through rustup]: https://rustup.rs/
[private security reporting channel]: https://github.com/HyunjoJung/rxls/security/advisories/new
