# rxls roadmap

This document describes direction and a small set of priorities, not release
dates or a list of everything that is missing. Follow the linked issues for
current status and ownership.

## Direction

rxls is a native Rust toolkit for reading spreadsheets, creating XLSX files,
and applying supported edits to existing XLSX/XLSM packages while preserving
untouched parts. Optional rendering and local interfaces use the same foundation.

The next focus is making existing capabilities easier to integrate and their
results and limits easier to understand, rather than building another office
suite. Existing readers, authoring, preservation editing, and rendering remain
part of the project; this is not a feature-removal plan.

Use [Compatibility](docs/compatibility.md),
[Preservation and editing](docs/preservation.md), and
[Formula support](docs/formulas.md) for current contracts. Reading, generating a
new workbook, preserving an existing package, and rendering are separate
capabilities. Source implementation does not establish registry availability.

## Priorities

### 1. Make existing APIs usable end to end

Prefer runnable examples and short integration paths over new tools or layers.
These tasks are already tracked publicly; do not create a second backlog.

| Task | Issue | Acceptance focus |
|---|---|---|
| Typed-row example using Serde | [#79](https://github.com/HyunjoJung/rxls/issues/79) | Explicit features, fallible handling, and a runnable example without external input files. |
| Matching CLI and Rust CSV recipes | [#80](https://github.com/HyunjoJung/rxls/issues/80) | Reproducible quoting, newline, formula-like text, and output-limit behavior. |
| MCP open/read/edit/save-copy walkthrough | [#81](https://github.com/HyunjoJung/rxls/issues/81) | Actual request schemas, read-back of edits, and a separate output file. |

Larger integration proposals should name a concrete gap exposed by a real
workflow. MCP is an interface to the existing engine, not a reason to introduce
a second calculation or persistence engine.

### 2. Make previews and saved results trustworthy

Prioritize keyboard access, correct edit targets, visible limitations, and
save verification over interface decoration or feature counts. Starting points
are [mobile-panel Escape and focus handling #83](https://github.com/HyunjoJung/rxls/issues/83)
and [corrupt-input tests for the ZIP verification helper #84](https://github.com/HyunjoJung/rxls/issues/84).
The latter strengthens a test helper, not a replacement production parser.

Editing changes must distinguish computed results from retained caches, expose
read-only reasons, preserve state on failure, and respect save-copy boundaries.
Reuse existing diagnostics; do not present an unverified cached value as a
freshly calculated result. Tracking changed parts is not itself proof of a
byte-comparison check.

Keep the shared render scene and existing SVG/PDF/PNG paths. Fix visual defects
using representative inputs and reproducible expectations. Report unsupported
features and approximations instead of silently omitting them or claiming exact
reproduction.

### 3. Keep support and distribution claims aligned

Treat core, general WASM, render worker, MCP, and editor-extension versions and
verification separately. Installation guides should identify the artifact and
compatible component combination actually checked, rather than infer shipment
from a source change or another package's release.

Use [dependency follow-ups #110](https://github.com/HyunjoJung/rxls/issues/110)
for scoped migrations. Update affected locks, tool versions, license notices,
and compatibility evidence together. Do not weaken resource limits or
preservation checks to make an upgrade or feature expansion pass.

## Later proposals and boundaries

Consider additional formulas, metadata, and rendering features when a proposal
includes a user workflow, a shareable minimal example, and a validation method.
Discuss scope and acceptance in a public issue before substantial implementation.
Absence from this page is not an automatic rejection.

This roadmap does not promise a complete Excel calculation engine, macro
execution, editing every format, identical reproduction of every visual feature,
or guessed dependency repair after structural changes. Existing contracts and
release gates remain in force. Regressions and security issues take priority.

## Contributing and acceptance

Public issues should state the problem, included and excluded scope, completion
conditions, and focused checks. Use fixtures with clear rights and provenance;
prefer minimal examples without sensitive data. Report vulnerabilities through
the private reporting route in the [Security Policy](.github/SECURITY.md).

Follow [CONTRIBUTING](CONTRIBUTING.md), run the issue's focused checks, and report
what could not be run. Maintainers select the additional checks needed before
merge or release. Contributing must not require access to personal workspaces
or unpublished operational records. Non-sensitive design decisions, limitations,
and acceptance criteria belong in public issues or documentation.

Reading correctness, preservation, calculation, visual fidelity, and distribution
are separate verification targets. Support claims with the evidence described in
[Validation and reproducibility](docs/validation.md); passing one test corpus
does not establish complete compatibility with every document.

## Keeping this roadmap small

Keep direction and a few priorities here, execution status in issues and PRs,
and completed changes in the [changelog](CHANGELOG.md) and releases. When a
priority changes, adjust existing scope rather than only adding more work.
Do not turn completed work back into a future milestone or duplicate execution
logs on this page.
