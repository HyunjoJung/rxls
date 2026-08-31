# rxls Spreadsheet Preview

Open spreadsheet files in a local, read-only VS Code preview backed by the
same Rust/WebAssembly renderer as the public rxls viewer.

The extension registers previews for the formats listed in the canonical
[rxls compatibility contract](https://github.com/HyunjoJung/rxls/blob/main/docs/compatibility.md).
Use the sheet and page controls to navigate, zoom in or out, reload the source,
and export the current view as SVG or PNG.

| Format | Preview | Source modification |
|---|:---:|:---:|
| XLS | Yes | No |
| XLSX | Yes | No |
| XLSM | Yes | No |
| XLSB | Yes | No |
| ODS | Yes | No |

## Install

| Channel | Recommended use |
|---|---|
| [Visual Studio Marketplace](https://marketplace.visualstudio.com/items?itemName=HyunjoJung.rxls-spreadsheet-preview) | Install directly in VS Code |
| [Open VSX Registry](https://open-vsx.org/extension/HyunjoJung/rxls-spreadsheet-preview) | Install in VSCodium and other Open VSX clients |
| [GitHub release](https://github.com/HyunjoJung/rxls/releases/tag/vscode-v0.1.0) | Audit or install the canonical VSIX offline |

The extension identifier is `HyunjoJung.rxls-spreadsheet-preview`. From a
terminal with VS Code on `PATH`:

```console
code --install-extension HyunjoJung.rxls-spreadsheet-preview
```

The verified 0.1.0 VSIX in the GitHub release has SHA-256
`3d1307502220c65d2755d7c1ef214a8117127b475fdae0c6da514f6ab3eecfd8`.
For a manual install, download the `.vsix` and matching `.sha256`, then choose
**Extensions: Install from VSIX...** in VS Code.

## Security and privacy

- Workbook bytes are read with `vscode.workspace.fs` and sent only to the
  extension's isolated webview worker. The extension has no telemetry or
  network client.
- Every file is treated as untrusted data. Workbook content is parsed, never
  executed, and cannot supply scripts, resource paths, or extension settings.
- Restricted Mode and virtual workspaces are supported because the extension
  does not execute workspace code or consume workspace configuration.
- Inputs are limited to 32 MiB. At most four active previews may account for
  128 MiB of workbook bytes. SVG/PNG messages are limited to 16 MiB.
- The webview CSP permits only packaged scripts, styles, images, the dedicated
  worker, and its WASM resource. It permits no external network origin.
- Local and provider-backed resources are watched when VS Code exposes a file
  watcher. The toolbar reload command remains available if a provider cannot
  emit change events.
- A renderer crash is contained to the worker. The preview reports the failure
  and can be reloaded without restarting VS Code.

The preview does not modify the source workbook. Browser editing support is a
separate rxls product surface and is intentionally unavailable here.

## Development

From the repository root:

```console
npm ci --prefix viewer --ignore-scripts
npm ci --prefix extensions/vscode --ignore-scripts
npm --prefix extensions/vscode run test:unit
npm --prefix extensions/vscode run test:e2e
npm --prefix extensions/vscode run package:vsix
```

`package:vsix` builds twice, normalizes both archives, compares their SHA-256
digests, verifies packaged licenses and renderer bytes, and writes the final
VSIX plus checksum under `extensions/vscode/target/`.

The E2E suite opens all five formats in trusted and Restricted Mode workspaces.
It also verifies SVG/PNG export and reload-on-change. CI repeats the format
matrix on Linux, macOS, and Windows with VS Code 1.134.0.

The license is MIT. Bundled dependency notices are included in the VSIX.
