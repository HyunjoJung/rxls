# rxls-wasm

`rxls-wasm` is the synchronous WebAssembly adapter for the `rxls` spreadsheet
reader. It accepts spreadsheet bytes and exposes text, CSV, HTML, Markdown, and
stable diagnostic JSON outputs.

The package reads `.xls`, `.xlsx`, `.xlsm`, `.xlsb`, and `.ods`. It is read-only:
it does not create, modify, or preserve spreadsheet packages. The synchronous
exports are `extractText`, `toCsv`, `toHtml`, `toMarkdown`, `reportJson`,
`maxInputBytes`, and `maxExportOutputBytes`.

```sh
npm install rxls-wasm
```

## Node.js

Node.js 20 or newer is required. The Node binding initializes synchronously
when loaded and does not export the browser-only default initializer.

```js
const fs = require("node:fs");
const { reportJson, maxInputBytes } = require("rxls-wasm");

const bytes = fs.readFileSync("workbook.xlsx");
if (bytes.byteLength > maxInputBytes()) throw new Error("workbook is too large");
const report = JSON.parse(reportJson(bytes));
console.log(report.stats);
```

Node ESM consumers can use the same named exports:

```js
import { reportJson } from "rxls-wasm";
```

## Browser

Bundlers that honor the `browser` export condition select the web binding and
its browser-specific declarations. Initialize it before calling an adapter:

```js
import init, { reportJson } from "rxls-wasm";

await init();
const bytes = new Uint8Array(await file.arrayBuffer());
const report = JSON.parse(reportJson(bytes));
```

Adapter failures are JavaScript `Error` objects with the stable fields `name`
(`"RxlsError"`), `kind`, `message`, `location`, and nullable string `cause`.
The generated TypeScript declarations include `RxlsErrorObject` for narrowing.

## Memory and input contract

The API is synchronous. `wasm-bindgen` first copies every `Uint8Array` into
WebAssembly memory, and parsing may allocate additional bounded buffers. Inputs
are therefore limited to 32 MiB (`maxInputBytes()`). Check the size before the
call, avoid concurrent parses of large files, release references to input and
output buffers promptly, and move parsing to a Web Worker when UI latency
matters. Streaming is not currently supported.

The release gate exercises tracked XLS, XLSX, XLSM, XLSB, and ODS fixtures in
Node and Chromium. It calls every public export, compares text, CSV, HTML, and
diagnostic report semantics with an independently compiled native helper, and
probes malformed inputs, invalid sheet indexes, the exact input limit, and one
byte over it. It also installs the packed archive into a clean consumer, checks
CommonJS, Node ESM, browser-conditional ESM, and compiles a strict TypeScript
consumer. The shipped demo is driven in Chromium through ready, successful
upload/export, malformed-file, and input-limit states.

The same gate enforces these distribution ceilings:

| Artifact | Maximum |
| --- | ---: |
| Each generated `.wasm` target | 2 MiB |
| Each JavaScript glue target | 128 KiB |
| Packed npm archive | 2 MiB |

Build and validate the candidate with `bash scripts/build-wasm-package.sh`.
The npm package also includes generated TypeScript declarations, the browser
demo, and license notices. `wasm-size-report.json` is published separately as
release evidence and is not embedded in the package it describes.
