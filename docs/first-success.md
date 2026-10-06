# First successful file operation

[Back to README](../README.md) · [Contributor checks](../CONTRIBUTING.md#choose-a-focused-check)

Choose one path. Repository commands run from its root; Rust examples create synthetic data or
use existing project-owned samples. Core 0.1.4 is published; a source checkout includes later work.
Pin that checkout when reporting results. The worker, VS Code extension and MCP adapter have
their own versions; see [release/source boundaries](compatibility.md#release-and-source-boundaries).

## Rust: typed records without an input file

Use Rust 1.85+ and this checkout's [typed_rows example](../examples/typed_rows.rs):

```sh
cargo +1.85.0 run --locked --features serde --example typed_rows
```

It writes and reopens a tiny XLSX in memory, treats the header as field names and propagates
deserialization errors. `serde` is opt-in; `xlsx` is enabled by default. Expected stdout:

```text
Road: price=125.5, awarded=true
Bridge: price=88, awarded=false
```

For create/read/evaluate/edit/save/reopen, run the existing [API journey](../examples/api_journeys.rs):

```sh
cargo +1.85.0 run --locked --example api_journeys -- target/first-success.xlsx
```

That example checks the operations and writes a workbook with worksheet `Data` and value `84`
in A2. To use typed rows in a separate application, select `rxls = { version = "=0.1.4",
features = ["xlsx", "serde"] }` and `serde` with its `derive` feature; the new example command
belongs to this checkout, while its public API works with the released crate.

## CLI: inspect the generated file

After the API journey above, use the current source CLI:

```sh
cargo +1.85.0 run --locked --bin rxls -- info target/first-success.xlsx
```

The output identifies worksheet `Data`. For a registry installation instead:

```sh
cargo install rxls --version =0.1.4 --locked
rxls info target/first-success.xlsx
```

Continue with the existing [extract](../examples/extract.rs) or [metadata](../examples/metadata.rs)
example and the [CLI contract](compatibility.md#export-diagnostics-cli-wasm-mcp-and-vs-code).
Changing a usage example can be a small contribution; check current issue comments and PRs
before choosing a task.

## Browser: open the same workbook

[Open the viewer](https://hyunjojung.github.io/rxls/), choose `target/first-success.xlsx`, select
`Data` and confirm A2 displays `84`. On an editable XLSX/XLSM, make a cell edit, use Enter, then
Save workbook to download a copy and reopen it. XLS/XLSB/ODS remain read-only. The hosted
deployment may differ from a local checkout; record its observed build/runtime when reporting a bug.

For source UI work, follow the [viewer setup](../viewer/README.md#development) for the pinned
WASM/runtime and Vite commands before using `npm --prefix viewer run dev`. Start contributions
with a focused UI reproduction or existing Node/browser test; check whether another person owns
the issue before editing its files.

## MCP: open a local session

The source-only MCP adapter requires Rust 1.88, independently of the core:

```sh
cargo +1.88.0 build --manifest-path bindings/mcp/Cargo.toml --locked --release
```

Configure an existing MCP client using the [server guide](../bindings/mcp/README.md#configure).
Use an absolute binary path (`rxls-mcp.exe` on Windows) and an explicit `--root` containing the
generated workbook. Invoke `workbook_open` with this **tool argument object**, replacing the path:

```json
{ "path": "/absolute/path/to/target/first-success.xlsx" }
```

Success returns a `session_id`, format `xlsx` and worksheet `Data`. Close that session with
`workbook_close` using this tool argument object:

```json
{ "session_id": "the returned session_id" }
```

The adapter is versioned 0.1.0 and is not an independently published crate. Use the existing tool
list/schema for follow-up reads; source build and installed binary verification are separate results.

## If the first operation fails

| Observed error | Next action |
| --- | --- |
| `typed_rows` requires `serde`/`xlsx`, or `RangeDeserializerBuilder` is unavailable | Enable `serde`; with defaults disabled, enable both `xlsx,serde`. |
| Serde reports `missing field` or an invalid field type | Match the header to struct field names and inspect that cell's typed value. |
| MCP dependencies require Rust 1.88 | Use `cargo +1.88.0` for the separate adapter. |
| `RXLS_MCP_PATH_NOT_FOUND` / `RXLS_MCP_PATH_OUTSIDE_ROOT` | Use an existing absolute path inside the configured canonical root. |
| Browser worker/WASM startup mismatch | Rebuild and prepare the matching runtime using the viewer guide and toolchain lock. |

Report the exact version/revision, command or tool arguments, feature set, error and expected result.
Use a small generated workbook. [The contribution guide](../CONTRIBUTING.md#your-first-contribution)
explains coordination, draft PRs and the focused checks for the changed area.
