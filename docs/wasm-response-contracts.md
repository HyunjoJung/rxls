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

## Remaining issue scope

Parts A, B, and C are implemented here: shared response rules, Rust-owned
calculation observations under protocol v3, and internal JSON assembly as
described below. Part D requires separately recorded before/after native and
browser measurements; contract tests alone do not establish performance gains.

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
