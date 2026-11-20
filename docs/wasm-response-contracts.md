# Shared render-worker response contracts

Part A of #153 initially retained `rxls.render-worker.v2`; no
package subpath, request, response field, or resource ceiling was added by that step.

`bindings/render-wasm/js/response-contracts.mjs` owns the common workbook,
document-property, edit-state, and inspected-cell rules. Both the client and
worker still validate responses. A factory supplies each boundary's error
classification and limits; sharing rules does not mean trusting the other
boundary or removing checks.

| Responsibility | Owner |
| --- | --- |
| Data shape, cell kinds, image totals, edit-state invariants | Shared internal validators |
| WASM JSON byte preflight and negotiated history limits | Worker runtime |
| Request/document/coordinate identity, readiness, Promise lifecycle | Client |
| Queues, cancellation, resource reservations, session release | Existing worker/client code |

The client retains its existing hard limits. The worker passes its validated
WASM output/history limits, including lower advertised ceilings. Invalid
WASM data still produces `wasm_api_mismatch` at `wasm`; invalid client-side
responses still produce `worker_message_error` at `worker`. Data error messages
now come from the common validator rather than separate generic worker text.

## Deliberate rule convergence

The worker now rejects extra edit-state fields itself instead of forwarding
them to be rejected by the client. It also uses the client's plain-object and
UTF-8-bounded sheet-name predicates. Its existing bounded JSON parser remains
in front of these checks. No previously rejected client response is accepted
by this change. Null-prototype records remain valid; malformed cell caches
still have the same maximum nesting depth of 16.

The internal module has an accompanying declaration and is registered in the
strict npm file/declaration allowlists. It is not added to `package.json`
exports. Existing package-fixture construction automatically includes it
through `DECLARATION_MODULES`.

## Checks

From the repository root:

```sh
node --test bindings/render-wasm/tests/response-contracts.test.mjs
node --test bindings/render-wasm/tests/response-boundaries.test.mjs
tsc --noEmit --strict --module NodeNext --moduleResolution NodeNext --target ES2022 \
  bindings/render-wasm/tests/response-contracts-consumer.mts
python3 scripts/test_response_contract_package.py
npm --prefix bindings/render-wasm test
python3 scripts/test_check_render_package.py
```

The pure contract tests exercise the same fixture matrix with both error
policies. The loopback boundary tests run the real client, worker runtime and
protocol, using a fake Rust session and fake browser transport; they do not
replace the real WASM/browser/package gates. The focused Python tests check
module registration and declaration matching, not a built npm artifact.

The existing browser and installed-package checks must still pass before
merging. This PR does not claim parsing, rendering or edit-latency improvements.

## Issue #153

The Worker shares response rules, tracks calculation observations with the
Rust workbook history, and assembles JSON from typed results. The edit-path
measurements below compare the internal JSON change on native and browser builds.

## Internal JSON assembly

Part C constructs a validated `WorkbookInspection` and a typed mutation envelope
before encoding JSON. Recalculation joins the same envelope, so neither workbook
inspection nor mutation results are encoded and then parsed back inside Rust.
The final conversion to a canonical JSON value preserves the prior response
bytes, including key order, nulls, and number representation. Public inspection
responses retain their original struct field order.

Inspection retains its own output ceiling: mutation assembly measures its
serialized UTF-8 bytes with a counting writer before encoding the final result.
The complete result also retains the existing output ceiling. Both checks occur
before swapping the candidate workbook or changing undo/redo history. Snapshot,
package save/reparse, evaluator, and preservation validation remain unchanged.

## Version 3 calculation observation

Part B intentionally replaces `rxls.render-worker.v2` with
`rxls.render-worker.v3`. The v2 exact-key response is not extended in place.
Deploy the client, Worker, and WASM from the same package version. A v3 client
rejects v2 Worker or WASM capability identities with `worker_message_error`;
a v3 Worker rejects old client requests with `protocol_mismatch` and old or
unversioned WASM modules with `wasm_api_mismatch` before opening a document.
No permissive unknown-field fallback or mixed-version compatibility is offered.

`EditState` schema 2 adds exactly one `calculation` object, containing `status`
and `summary`. `unknown` requires a null summary: opening or reopening a saved
workbook never establishes fresh evaluator evidence. `observed` contains the
last successful whole-workbook supported-evaluator summary. `stale` preserves
that same bounded summary after a cell edit without whole-workbook recalculation.
The per-operation `recalculation` summary retains its original meaning and keys.
Neither observation nor a zero warning count guarantees full Excel calculation
compatibility. Unsupported formulas continue to retain their existing caches.

Rust owns each observation with the corresponding workbook and undo/redo
snapshot. Property edits preserve it, successful recalculation replaces it,
cell edits without recalculation mark it stale, and failed edits leave it and
all history unchanged. Undo/redo restores the observation from the restored
snapshot; history eviction therefore cannot desynchronize the two. The fixed
observation state and bounded static-reason vector allocation count against the
existing history byte budget. No formula strings, cell text, or workbook copies
are added to this metadata, and no observation is persisted into OOXML.

The viewer formats the current Rust observation and each operation report. It
has no warning undo/redo stacks, and labels both initial uncertainty and stale
observations rather than treating missing recalculation as successful checking.

## Edit-path measurements

The Stage D comparison used parent
`04f9f004cc6098655f333f9cec231e6481872c6c` and Part C
`48bf8a21c77dead9605d5db5407beb6ef35a38a4`. Both used the same locked
dependencies and Rust 1.85.0 release profile on Windows 10 (Ryzen 5 3600).
Browser runs used Chrome for Testing 150.0.7871.115, Node 24.18.0 and the
same viewer bundle. Each pair alternated the order across repetitions.

The `editing_benchmark` example used a 211,074-byte XLSX with
64,000 numeric cells and 200 formulas. In 18 measured samples per revision,
the median moved from 6,478 to 6,375 ms for eight sequential edits,
1,235 to 1,163 ms for one 1×8 paste, 1,399 to 1,428 ms for a 10,000×1
paste, and 1,349 to 1,308 ms for a 1×10,000 paste. A separate test with
20 samples per revision on the same input measured 1,112 to 1,118 ms
for one recalculating edit, 269 to 264 ms for Undo, and 228 to 233 ms
for Redo. Read-only inspection took about 0.010 ms and returned 313
bytes. This last measurement covers public inspection alone.

In three fresh browser contexts per revision, Worker initialization
took a median 66.6 to 53.7 ms and workbook open took 244.2 to 240.7 ms.
Across 18 warm Worker samples the medians went from 1,146.6 to 1,135.5 ms
for one edit, 1,051.4 to 1,058.6 ms for a 1×8 paste, 301.2 to 296.6 ms
for Undo, and 260.6 to 263.6 ms for Redo. These include the full Client
request and validated Worker response.

The viewer used a separate 4,724-byte XLSX with 360 populated cells and
40 formulas. Across 30 successful SVG replacements in five fresh
contexts per revision, the median click-to-ready frame went from 63.5
to 67.7 ms. The mutation Worker round trip went from 13.7 to 14.0 ms
and the interactive render Worker round trip from 16.3 to 17.4 ms.
The larger fixture was rejected by the full-sheet renderer on both
revisions and was excluded from this comparison.

Native peak working set in the original benchmark was 334.1 to 334.5
MiB. Warm Worker V8 used heap was about 1.22 MB on both revisions.
WASM linear memory capacity went from 184.8 to 176.6 MiB, and compressed
edit history used 3,388,242 bytes on each revision. Linear memory capacity
and the 128 MiB Worker resource budgets measure different quantities.
Browser process RSS was not measured in this Windows comparison.

The latency ranges overlapped on this host. Save/reparse, recalculation,
and response assembly were measured together. The data supports keeping
Part C scoped to code simplification. Re-run the native
baseline with `cargo +1.85.0 run --release --locked --manifest-path
bindings/render-wasm/Cargo.toml --example editing_benchmark` at both
revisions. Browser timings require a real WASM build and separate fresh
and warm measurements.
