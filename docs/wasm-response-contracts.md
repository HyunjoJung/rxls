# Shared render-worker response contracts

Part A of #153. The wire protocol remains `rxls.render-worker.v2` and no
package subpath, request, response field, or resource ceiling is added.

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

This change does not move formula warning history into Rust (B), change Rust
JSON assembly (C), or provide performance measurements (D). Those require
separate changes. In particular, B must establish a versioned or negotiated
response contract before new document-state fields can be added safely.

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
