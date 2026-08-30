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
| `workbook_export_sheet` | Export bounded CSV, Markdown, or HTML |
| `workbook_set_cells` | Atomically set values or write formulas to up to 100 cells |
| `workbook_save_copy` | Publish a new same-format XLSX/XLSM copy without overwrite |
| `workbook_close` | Close a session and release retained bytes |

## Security boundaries

- Existing paths and allowed roots are canonicalized before comparison.
- Workbook inputs are capped at 32 MiB; four sessions may retain 128 MiB total.
- One JSON-RPC line and one structured tool result are each capped at 1 MiB.
- Workbook data is accepted by local path only, never as base64 in MCP JSON.
- XLS, XLSB, and ODS sessions are read-only. XLSX/XLSM edits require rxls's
  lossless retained-package capability.
- Save-copy rejects existing destinations and atomically publishes a complete
  sibling file without overwriting.

This crate is currently shipped from the rxls repository and is not yet
published independently. The license is MIT.
