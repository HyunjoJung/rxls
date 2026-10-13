# rxls MCP server

`rxls-mcp` is the local Model Context Protocol adapter for the
[`rxls`](https://github.com/HyunjoJung/rxls) spreadsheet toolkit. It reads XLS,
XLSX, XLSB, and ODS through typed sessions, and applies package-preserving cell
edits to XLSX/XLSM files.

The server uses newline-delimited MCP over standard input/output. Workbook
bytes never enter JSON messages and no network listener is opened.

## Build

```console
cargo build --manifest-path bindings/mcp/Cargo.toml --locked --release
```

The adapter's tests, release build, and packaged-source verification use Rust
1.88.0. Before a new core version reaches crates.io, ordinary single-package
`cargo package` cannot resolve the normalized registry dependency. CI instead
uses Cargo 1.96.1's interdependent-package overlay in an isolated, bounded copy
of the exact Git revision, with `RUSTC` and `RUSTDOC` explicitly pinned to
1.88.0. Both the core and intentionally `publish = false` MCP archives are
packaged and compiled; neither is published, patched, or verified with
`--no-verify`. Source manifests and the MCP locked graph stay unchanged.

On a clean checkout, reproduce that verification with:

```console
rustup toolchain install 1.88.0 1.96.1 --profile minimal --no-self-update
python3 -m unittest discover -s bindings/mcp/scripts -p 'test_*.py'
python3 bindings/mcp/scripts/package_candidate.py --expected-sha "$(git rev-parse HEAD)"
```

`bindings/mcp/target/package-candidate/` contains both verified archives and
`mcp-package-candidate.json`, binding their inventory and SHA-256 digests to the
source revision, snapshot, locked graph, and actual compiler versions. This
verification-only core archive is not the canonical release artifact.

## Configure

Run the binary with one or more explicit roots. Relative workbook paths are
resolved from the server process working directory and must remain below an
allowed root after canonicalization.

```json
{
  "mcpServers": {
    "rxls": {
      "command": "/absolute/path/to/rxls-mcp",
      "args": ["--root", "/absolute/path/to/spreadsheets"]
    }
  }
}
```

With no `--root`, the current directory is the only allowed root.

## Tools

| Tool | Purpose |
| --- | --- |
| `workbook_open` | Open a bounded local XLS/XLSX/XLSM/XLSB/ODS session |
| `workbook_list_sessions` | List active sessions and retained byte totals |
| `workbook_inspect` | Inspect format, sheets, provenance, and edit capability |
| `workbook_read_range` | Read up to 10,000 cells as typed structured output |
| `workbook_compare` | Compare typed and displayed values across up to 10,000 cells |
| `workbook_export_sheet` | Export bounded CSV, Markdown, or HTML |
| `workbook_set_cells` | Atomically set values or write formulas to up to 100 cells |
| `workbook_save_copy` | Publish a new same-format XLSX/XLSM copy without overwrite |
| `workbook_close` | Close a session and release retained bytes |

## Open, read, edit, and save a copy

From the repository root, generate a project-owned XLSX in a fresh temporary
directory and build the server. The core example uses Rust 1.85; the MCP
adapter requires Rust 1.88.

```sh
walkthrough_root=$(mktemp -d)
cargo +1.85.0 run --locked --example api_journeys -- "$walkthrough_root/source.xlsx"
cargo +1.88.0 build --manifest-path bindings/mcp/Cargo.toml --locked
printf 'Allowed root: %s\n' "$walkthrough_root"
bindings/mcp/target/debug/rxls-mcp --root "$walkthrough_root"
```

The last command starts the stdio server; configure your existing MCP client
to launch that binary with the same `--root` argument. Do not paste the objects
below directly into its stdin: they are **tool arguments**, not complete
JSON-RPC transport messages. Replace `/absolute/allowed/root` with the directory
printed above. Replace every `<SESSION_ID>` with the opaque `session_id`
returned by `workbook_open`.

1. Call `workbook_open`:

   ```json
   { "path": "/absolute/allowed/root/source.xlsx" }
   ```

   Check that the result reports format `xlsx`, worksheet `Data`, and
   `edit_capability: "read_write_preserving"` before editing.

2. Call `workbook_read_range`:

   ```json
   { "session_id": "<SESSION_ID>", "sheet": "Data", "range": "A1:B2" }
   ```

   The generated sample has `amount` in A1 and the number `84` in A2. B2 is
   `A2*2` with a retained cached value of `42`: the generating example edits A2
   without recalculating. Reading a cache does not establish a current result.

3. Call `workbook_set_cells` with an atomic pair of edits:

   ```json
   {
     "session_id": "<SESSION_ID>",
     "sheet": "Data",
     "edits": [
       { "kind": "set", "cell": "A2", "value": { "type": "number", "value": 126 } },
       { "kind": "formula", "cell": "B2", "formula": "=A2*2", "cached": { "type": "number", "value": 252 } }
     ]
   }
   ```

   Check `applied_edits: 2`. The formula cache here is explicitly supplied by
   the caller; this call does not claim that the server recalculated it.

4. Read the edited cells **before saving**, using `workbook_read_range`:

   ```json
   { "session_id": "<SESSION_ID>", "sheet": "Data", "range": "A2:B2" }
   ```

   In `rows[0]`, A2 has `value: { "type": "number", "value": 126 }`.
   B2 has `value.type: "formula"`, `formula: "A2*2"`, and the numeric cached
   value `252`.

5. Call `workbook_save_copy`:

   ```json
   { "session_id": "<SESSION_ID>", "path": "/absolute/allowed/root/edited.xlsx" }
   ```

   The destination must not exist, must stay below an allowed root, and must
   retain the source format (`.xlsx` here; `.xlsm` for an XLSM source). An
   existing destination is rejected; choose a new filename on a later run.
   The source remains unchanged. For an independent read-back, open
   `edited.xlsx` in a new session and read `Data!A2:B2` with that new ID.

6. Call `workbook_close` for each session you opened:

   ```json
   { "session_id": "<SESSION_ID>" }
   ```

Check each tool result for an error before using its structured output or
continuing to the next step. Closing an unsaved session discards its edits;
save-copy is the explicit persistence operation.

## Security boundaries

- Existing paths and allowed roots are canonicalized before comparison.
- Workbook inputs are capped at 32 MiB; four sessions may retain 128 MiB total.
- One JSON-RPC line and one structured tool result are each capped at 1 MiB.
- Range comparisons inspect at most 10,000 cells and return at most 100 detailed
  differences within a 512 KiB detail budget, plus complete count and explicit
  count/size truncation status.
- Workbook data is accepted by local path only, never as base64 in MCP JSON.
- XLS, XLSB, and ODS sessions are read-only. XLSX/XLSM edits require rxls's
  lossless retained-package capability.
- Save-copy rejects existing destinations and atomically publishes a complete
  sibling file without overwriting.

This crate is currently shipped from the rxls repository and is not yet
published independently. The license is MIT.
