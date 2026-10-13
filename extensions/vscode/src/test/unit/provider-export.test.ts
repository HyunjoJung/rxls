import assert from "node:assert/strict";
import * as path from "node:path";
import test from "node:test";
import type * as vscode from "vscode";

import type { RxlsPreviewProvider } from "../../provider";
import { MAX_EXPORT_BYTES } from "../../protocol";

const SVG = new TextEncoder().encode("<svg></svg>");
const PNG = Uint8Array.from([137, 80, 78, 71, 13, 10, 26, 10, 0]);
const LONG_STEM = "W".repeat(80) + "-" + "S".repeat(31) + "-sheet";

class TestUri {
  public constructor(public readonly path: string, public readonly scheme = "rxls-test") {}
  public with(change: { path?: string }): TestUri { return new TestUri(change.path ?? this.path, this.scheme); }
  public toString(): string { return `${this.scheme}:${this.path}`; }
  public static joinPath(base: TestUri, ...segments: string[]): TestUri {
    return new TestUri(path.posix.join(base.path, ...segments), base.scheme);
  }
}

class MockEmitter<T> {
  private readonly listeners = new Set<(value: T) => void>();
  public readonly event = (listener: (value: T) => void) => {
    this.listeners.add(listener);
    return { dispose: () => this.listeners.delete(listener) };
  };
  public fire(value: T): void { for (const listener of this.listeners) listener(value); }
  public dispose(): void { this.listeners.clear(); }
}

interface Calls {
  messages: Record<string, unknown>[];
  dialogs: Record<string, unknown>[];
  writes: { uri: TestUri; bytes: Uint8Array }[];
  destination: TestUri | undefined;
  dialogError?: Error;
  writeError?: Error;
}
let current: Calls;
const disposable = () => ({ dispose() {} });
const mockVscode = {
  Uri: TestUri,
  EventEmitter: MockEmitter,
  ExtensionMode: { Test: 3 },
  FileType: { File: 1, Directory: 2 },
  RelativePattern: class {},
  CancellationError: class extends Error {},
  FileSystemError: class extends Error {},
  window: {
    async showSaveDialog(options: Record<string, unknown>): Promise<TestUri | undefined> {
      current.dialogs.push(options);
      if (current.dialogError) throw current.dialogError;
      return current.destination;
    }
  },
  workspace: {
    createFileSystemWatcher() { return { dispose() {}, onDidChange: disposable, onDidCreate: disposable, onDidDelete: disposable }; },
    fs: {
      async stat() { return { type: 1, size: 3 }; },
      async readFile(uri: TestUri) {
        return uri.path.endsWith("index.html")
          ? new TextEncoder().encode('<head><meta http-equiv="Content-Security-Policy" content="default-src none" /></head>')
          : Uint8Array.from([1, 2, 3]);
      },
      async writeFile(uri: TestUri, bytes: Uint8Array): Promise<void> {
        current.writes.push({ uri, bytes: Uint8Array.from(bytes) });
        if (current.writeError) throw current.writeError;
      }
    }
  }
};

// The real provider is loaded against a bounded VS Code host double. Restore
// the CommonJS loader immediately; other modules/tests retain their own host.
const loader = require("node:module") as { _load(request: string, parent: unknown, isMain: boolean): unknown };
const originalLoad = loader._load;
let Provider: typeof RxlsPreviewProvider;
try {
  loader._load = function (request, parent, isMain) {
    return request === "vscode" ? mockVscode : originalLoad.call(this, request, parent, isMain);
  };
  Provider = (require("../../provider") as typeof import("../../provider")).RxlsPreviewProvider;
} finally {
  loader._load = originalLoad;
}

async function createHost() {
  current = { messages: [], dialogs: [], writes: [], destination: new TestUri("/exports/result.svg") };
  const calls = current;
  let receiver: ((value: unknown) => unknown) | undefined;
  const uri = new TestUri("/workspace/report.xlsx");
  const context = { extensionUri: new TestUri("/extension"), extensionMode: 3 } as unknown as vscode.ExtensionContext;
  const provider = new Provider(context);
  const panel = {
    webview: {
      options: {}, html: "", cspSource: "rxls-test:",
      asWebviewUri: (value: TestUri) => value,
      onDidReceiveMessage(callback: (value: unknown) => unknown) { receiver = callback; return disposable(); },
      async postMessage(value: Record<string, unknown>): Promise<boolean> { calls.messages.push(value); return true; }
    },
    onDidDispose: disposable,
    onDidChangeViewState: disposable
  } as unknown as vscode.WebviewPanel;
  const token = { isCancellationRequested: false } as vscode.CancellationToken;
  const document = await provider.openCustomDocument(uri as unknown as vscode.Uri, {} as vscode.CustomDocumentOpenContext, token);
  await provider.resolveCustomEditor(document, panel, token);
  async function receive(value: unknown): Promise<void> { assert.ok(receiver); await receiver(value); }
  await receive({ type: "ready" });
  return {
    calls, provider, uri,
    receive,
    statuses: () => calls.messages.filter((message) => message.type === "host-status").map((message) => message.message),
    close() { provider.dispose(); document.dispose(); }
  };
}

function within<T>(promise: Promise<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("export did not settle promptly")), 500);
    promise.then((value) => { clearTimeout(timer); resolve(value); }, (error: unknown) => { clearTimeout(timer); reject(error); });
  });
}

function request(host: Awaited<ReturnType<typeof createHost>>, kind: "svg" | "png") {
  const result = host.provider.api.requestExport(host.uri as unknown as vscode.Uri, kind);
  // Attach before delivery; a matching rejection cannot become unhandled.
  const observed = result.then((value) => ({ value, error: undefined }), (error: Error) => ({ value: undefined, error }));
  const command = host.calls.messages.filter((message) => message.type === "host-command").at(-1);
  assert.ok(command && typeof command.requestId === "string");
  return { requestId: command.requestId, observed };
}

test("accepted long toolbar exports reach the real save and write route with bounded leaf names", async () => {
  for (const kind of ["svg", "png"] as const) {
    const host = await createHost();
    try {
      const bytes = kind === "svg" ? SVG : PNG;
      await host.receive({ type: "export", requestId: null, kind, fileName: `${LONG_STEM}.${kind}`, bytes });
      assert.equal(host.calls.dialogs.length, 1);
      const uri = host.calls.dialogs[0]?.defaultUri as TestUri;
      assert.equal(path.posix.dirname(uri.path), "/workspace");
      assert.equal(path.posix.basename(uri.path), LONG_STEM.slice(0, 116) + `.${kind}`);
      assert.equal(host.calls.writes.length, 1);
      assert.deepEqual(host.calls.writes[0]?.bytes, bytes);
      assert.deepEqual(host.statuses(), [`${kind.toUpperCase()} exported`]);
    } finally { host.close(); }
  }
});

test("accepted long API responses return bytes without opening a save dialog", async () => {
  for (const kind of ["svg", "png"] as const) {
    const host = await createHost();
    try {
      const pending = request(host, kind);
      const bytes = kind === "svg" ? SVG : PNG;
      const response = { type: "export", requestId: pending.requestId, kind, fileName: `${LONG_STEM}.${kind}`, bytes };
      await host.receive(response);
      const outcome = await within(pending.observed);
      assert.equal(outcome.error, undefined);
      assert.deepEqual(outcome.value, { kind, fileName: LONG_STEM.slice(0, 116) + `.${kind}`, bytes });
      assert.deepEqual(host.statuses(), [`${kind.toUpperCase()} export ready`]);
      await host.receive(response); // Settled identity is now unknown and ignored.
      assert.equal(host.statuses().length, 1);
      assert.equal(host.calls.dialogs.length + host.calls.writes.length, 0);
    } finally { host.close(); }
  }
});

test("identifiable invalid responses promptly reject the matching request without save or write", async () => {
  const cases = [
    { fileName: "wrong.png", bytes: SVG },
    { fileName: "A".repeat(177) + ".svg", bytes: SVG },
    { fileName: "report.svg", bytes: new Uint8Array() },
    { fileName: "report.svg", bytes: new TextEncoder().encode("private malformed content") },
    { fileName: "report.svg", bytes: new Uint8Array(MAX_EXPORT_BYTES + 1) }
  ];
  for (const payload of cases) {
    const host = await createHost();
    try {
      const pending = request(host, "svg");
      await host.receive({ type: "export", requestId: pending.requestId, kind: "svg", ...payload });
      const outcome = await within(pending.observed);
      assert.ok(outcome.error instanceof Error);
      assert.match(outcome.error.message, /response was invalid/);
      assert.equal(outcome.value, undefined);
      assert.deepEqual(host.statuses(), ["SVG export failed"]);
      await host.receive({ type: "export", requestId: pending.requestId, kind: "svg", ...payload });
      assert.equal(host.statuses().length, 1);
      assert.equal(host.calls.dialogs.length + host.calls.writes.length, 0);
    } finally { host.close(); }
  }
});

test("a matching valid response of the wrong kind rejects with a fixed requested-kind status", async () => {
  const host = await createHost();
  try {
    const pending = request(host, "svg");
    await host.receive({ type: "export", requestId: pending.requestId, kind: "png", fileName: "report.png", bytes: PNG });
    const outcome = await within(pending.observed);
    assert.ok(outcome.error instanceof Error);
    assert.match(outcome.error.message, /kind did not match/);
    assert.deepEqual(host.statuses(), ["SVG export failed"]);
    assert.equal(host.calls.dialogs.length + host.calls.writes.length, 0);
  } finally { host.close(); }
});

test("invalid and unknown request identities leave the actual pending request untouched", async () => {
  const host = await createHost();
  try {
    const pending = request(host, "svg");
    let settled = false;
    void pending.observed.then(() => { settled = true; });
    for (const requestId of [undefined, "", "A".repeat(65), 1, "unknown-request"]) {
      await host.receive({ type: "export", requestId, kind: "svg", fileName: "wrong.png", bytes: SVG });
    }
    await host.receive({ type: "export", requestId: pending.requestId, kind: "pdf", fileName: "report.svg", bytes: SVG });
    await host.receive({ type: "loaded", requestId: pending.requestId, kind: "svg" });
    assert.equal(settled, false);
    assert.deepEqual(host.statuses(), []);
    assert.equal(host.calls.dialogs.length + host.calls.writes.length, 0);
    await host.receive({ type: "export", requestId: pending.requestId, kind: "svg", fileName: "report.svg", bytes: SVG });
    assert.equal((await within(pending.observed)).error, undefined);
  } finally { host.close(); }
});

test("an identifiable invalid toolbar payload sends only a fixed failure status", async () => {
  const host = await createHost();
  try {
    await host.receive({ type: "export", requestId: null, kind: "png", fileName: "private-path.svg", bytes: SVG });
    assert.deepEqual(host.statuses(), ["PNG export failed"]);
    assert.equal(host.calls.dialogs.length + host.calls.writes.length, 0);
  } finally { host.close(); }
});

test("cancelled save reports cancellation and never writes or reports exported", async () => {
  const host = await createHost();
  try {
    host.calls.destination = undefined;
    await host.receive({ type: "export", requestId: null, kind: "svg", fileName: "report.svg", bytes: SVG });
    assert.equal(host.calls.dialogs.length, 1);
    assert.equal(host.calls.writes.length, 0);
    assert.deepEqual(host.statuses(), ["SVG export cancelled"]);
  } finally { host.close(); }
});

for (const mode of ["dialog", "write"] as const) {
  test(`${mode} failure uses a fixed failure status without leaking the exception`, async () => {
    const host = await createHost();
    try {
      const error = new Error("private destination path and file-system detail");
      if (mode === "dialog") host.calls.dialogError = error;
      else host.calls.writeError = error;
      await host.receive({ type: "export", requestId: null, kind: "svg", fileName: "report.svg", bytes: SVG });
      assert.equal(host.calls.dialogs.length, 1);
      assert.equal(host.calls.writes.length, mode === "write" ? 1 : 0);
      assert.deepEqual(host.statuses(), ["SVG export failed"]);
    } finally { host.close(); }
  });
}

test("disposed sessions ignore accepted and rejected export traffic", async () => {
  const host = await createHost();
  host.close();
  await host.receive({ type: "export", requestId: null, kind: "svg", fileName: "report.svg", bytes: SVG });
  await host.receive({ type: "export", requestId: null, kind: "svg", fileName: "wrong.png", bytes: SVG });
  assert.deepEqual(host.statuses(), []);
  assert.equal(host.calls.dialogs.length + host.calls.writes.length, 0);
});
