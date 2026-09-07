import assert from "node:assert/strict";
import * as path from "node:path";
import * as vscode from "vscode";

import {
  PreviewEvent,
  RxlsPreviewApi,
  TEST_CRASH_COMMAND,
  VIEW_TYPE
} from "../../provider";

const EXTENSION_ID = "HyunjoJung.rxls-spreadsheet-preview";
const VIRTUAL_SCHEME = "rxls-e2e-memory";
const FORMATS = ["xls", "xlsx", "xlsm", "xlsb", "ods"] as const;
type Format = (typeof FORMATS)[number];

export async function run(): Promise<void> {
  const fixtures = parseFixtures(process.env.RXLS_E2E_FIXTURES);
  const formats = parseFormats(process.env.RXLS_E2E_FORMATS);
  const expectedTrust = process.env.RXLS_EXPECT_TRUST === "true";
  const failureBoundaries = process.env.RXLS_E2E_FAILURE_BOUNDARIES === "true";
  const expectedVirtual = process.env.RXLS_E2E_VIRTUAL === "true";
  const expectedPackaged = process.env.RXLS_EXPECT_PACKAGED === "true";
  const disposables: vscode.Disposable[] = [];
  const workspace = vscode.workspace.workspaceFolders?.[0];
  assert.ok(workspace, "E2E workspace is missing");

  if (expectedVirtual) {
    assertVirtualWorkspace();
    const seedDirectory = requireEnvironment("RXLS_E2E_SEED_DIRECTORY");
    const seeded = new Map<string, Uint8Array>();
    for (const format of formats) {
      const uri = vscode.Uri.file(path.join(seedDirectory, fixtures[format]));
      seeded.set(fixtures[format], await vscode.workspace.fs.readFile(uri));
    }
    const provider = new MemoryFileSystemProvider(workspace.uri, seeded);
    disposables.push(
      provider,
      vscode.workspace.registerFileSystemProvider(VIRTUAL_SCHEME, provider, {
        isCaseSensitive: true,
        isReadonly: false
      })
    );
    // Explorer may ask for the root during startup; prove the registered
    // provider resolves the actual workspace and the exact fixture bytes.
    assert.equal((await vscode.workspace.fs.stat(workspace.uri)).type, vscode.FileType.Directory);
    for (const [fileName, bytes] of seeded) {
      const uri = vscode.Uri.joinPath(workspace.uri, fileName);
      assert.deepEqual(
        Uint8Array.from(await vscode.workspace.fs.readFile(uri)),
        Uint8Array.from(bytes)
      );
    }
  }

  assert.equal(vscode.workspace.isTrusted, expectedTrust, "workspace trust mode changed");
  const extension = vscode.extensions.getExtension<RxlsPreviewApi>(EXTENSION_ID);
  assert.ok(extension, `${EXTENSION_ID} is not installed`);
  if (expectedPackaged) {
    assert.equal(
      normalizePath(extension.extensionPath),
      normalizePath(requireEnvironment("RXLS_EXPECT_EXTENSION_ROOT")),
      "VS Code loaded a source checkout instead of the installed VSIX"
    );
  }
  if (expectedVirtual) {
    assert.deepEqual(extension.packageJSON.capabilities?.virtualWorkspaces, { supported: true });
  }
  const api = await extension.activate();

  for (const format of formats) {
    const uri = vscode.Uri.joinPath(workspace.uri, fixtures[format]);
    if (expectedVirtual) {
      assertVirtualWorkspace();
      assert.equal(uri.scheme, VIRTUAL_SCHEME, "preview URI must use the virtual provider");
    }
    const loadedPromise = waitForPreview(api, uri, 0);
    await vscode.commands.executeCommand("vscode.openWith", uri, VIEW_TYPE);
    const loaded = await loadedPromise;
    assert.equal(loaded.status, "loaded");
    assert.equal(loaded.preview?.format, format);
    assert.equal(loaded.preview?.host, "vscode");
    assert.equal(loaded.preview?.rendered, true);
    assert.ok(Number(loaded.preview?.sheetCount) > 0, `${format} has no rendered sheets`);

    if (format === "xlsx" && expectedTrust) {
      const svg = await api.requestExport(uri, "svg");
      assert.equal(svg.kind, "svg");
      assert.match(new TextDecoder().decode(svg.bytes.subarray(0, 512)), /<svg/i);
      const png = await api.requestExport(uri, "png");
      assert.deepEqual([...png.bytes.subarray(0, 8)], [137, 80, 78, 71, 13, 10, 26, 10]);

      const reloadedPromise = waitForPreview(api, uri, loaded.generation);
      const bytes = await vscode.workspace.fs.readFile(uri);
      await vscode.workspace.fs.writeFile(uri, bytes);
      const reloaded = await reloadedPromise;
      assert.ok(reloaded.generation > loaded.generation, "file change did not reload preview");
      if (expectedVirtual) {
        assert.equal(vscode.Uri.parse(reloaded.uri).scheme, VIRTUAL_SCHEME);
        assertVirtualWorkspace();
      }
    }

    await vscode.commands.executeCommand("workbench.action.closeActiveEditor");
    await delay(100);
  }

  if (failureBoundaries) {
    assert.equal(expectedTrust, true, "failure-boundary tests require a trusted workspace");
    await exerciseFailureBoundaries(api, workspace.uri, fixtures);
  }

  for (const disposable of disposables.reverse()) {
    disposable.dispose();
  }
  console.log(
    `[rxls-e2e] ${JSON.stringify({
      result: "passed",
      version: vscode.version,
      mode: expectedPackaged ? "installed" : expectedVirtual ? "virtual" : expectedTrust ? "trusted" : "untrusted",
      trusted: expectedTrust,
      virtual: expectedVirtual,
      formats
    })}`
  );
}

async function exerciseFailureBoundaries(
  api: RxlsPreviewApi,
  workspace: vscode.Uri,
  fixtures: FixtureMap
): Promise<void> {
  const invalid = vscode.Uri.joinPath(workspace, fixtures.invalidFile);
  const invalidPromise = waitForError(api, invalid, 0);
  await vscode.commands.executeCommand("vscode.openWith", invalid, VIEW_TYPE);
  const invalidError = await invalidPromise;
  assert.notEqual(invalidError.code, "worker_crashed");
  await vscode.commands.executeCommand("workbench.action.closeActiveEditor");

  const oversized = vscode.Uri.joinPath(workspace, fixtures.oversizedFile);
  const oversizedPromise = waitForError(api, oversized, 0);
  await vscode.commands.executeCommand("vscode.openWith", oversized, VIEW_TYPE);
  const oversizedError = await oversizedPromise;
  assert.equal(oversizedError.code, "limit_exceeded");
  await vscode.commands.executeCommand("workbench.action.closeActiveEditor");

  const recoverable = vscode.Uri.joinPath(workspace, fixtures.xlsx);
  const loadedPromise = waitForPreview(api, recoverable, 0);
  await vscode.commands.executeCommand("vscode.openWith", recoverable, VIEW_TYPE);
  const loaded = await loadedPromise;
  const crashPromise = waitForError(api, recoverable, loaded.generation);
  await vscode.commands.executeCommand(TEST_CRASH_COMMAND, recoverable);
  const crashed = await crashPromise;
  assert.equal(crashed.code, "worker_crashed");
  const recoveredPromise = waitForPreview(api, recoverable, crashed.generation);
  await api.reload(recoverable);
  const recovered = await recoveredPromise;
  assert.ok(recovered.generation > crashed.generation);
  assert.equal(recovered.preview?.rendered, true);
  await vscode.commands.executeCommand("workbench.action.closeActiveEditor");
}

function waitForPreview(
  api: RxlsPreviewApi,
  uri: vscode.Uri,
  minimumGeneration: number
): Promise<PreviewEvent> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      subscription.dispose();
      reject(new Error(`timed out waiting for preview: ${uri.toString()}`));
    }, 45_000);
    const subscription = api.onDidChangePreview((event) => {
      if (event.uri !== uri.toString() || event.generation <= minimumGeneration) {
        return;
      }
      if (event.status === "error") {
        clearTimeout(timer);
        subscription.dispose();
        reject(new Error(`${event.code ?? "preview_error"}: ${event.message ?? "failed"}`));
      } else if (event.status === "loaded") {
        clearTimeout(timer);
        subscription.dispose();
        resolve(event);
      }
    });
  });
}

function waitForError(
  api: RxlsPreviewApi,
  uri: vscode.Uri,
  minimumGeneration: number
): Promise<PreviewEvent> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      subscription.dispose();
      reject(new Error(`timed out waiting for preview error: ${uri.toString()}`));
    }, 45_000);
    const subscription = api.onDidChangePreview((event) => {
      if (
        event.uri !== uri.toString() ||
        event.generation < minimumGeneration ||
        event.status !== "error"
      ) {
        return;
      }
      clearTimeout(timer);
      subscription.dispose();
      resolve(event);
    });
  });
}

type FixtureMap = Record<"xls" | "xlsx" | "xlsm" | "xlsb" | "ods", string> & {
  invalidFile: string;
  oversizedFile: string;
};

function parseFixtures(value: string | undefined): FixtureMap {
  if (!value) {
    throw new Error("RXLS_E2E_FIXTURES is missing");
  }
  const parsed = JSON.parse(value) as Record<string, unknown>;
  for (const format of [
    "xls",
    "xlsx",
    "xlsm",
    "xlsb",
    "ods",
    "invalidFile",
    "oversizedFile"
  ]) {
    if (typeof parsed[format] !== "string" || !parsed[format]) {
      throw new Error(`missing ${format} E2E fixture`);
    }
  }
  return parsed as FixtureMap;
}

function parseFormats(value: string | undefined): Format[] {
  if (!value) {
    return [...FORMATS];
  }
  const parsed = JSON.parse(value) as unknown;
  if (
    !Array.isArray(parsed) ||
    parsed.length === 0 ||
    parsed.some((format) => !FORMATS.includes(format as Format)) ||
    new Set(parsed).size !== parsed.length
  ) {
    throw new Error("RXLS_E2E_FORMATS must contain unique supported spreadsheet formats");
  }
  return parsed as Format[];
}

function requireEnvironment(name: string): string {
  const value = process.env[name];
  if (!value) {
    throw new Error(`${name} is missing`);
  }
  return value;
}

function normalizePath(value: string): string {
  const normalized = path.resolve(value);
  return process.platform === "win32" ? normalized.toLowerCase() : normalized;
}

function assertVirtualWorkspace(): void {
  const folders = vscode.workspace.workspaceFolders;
  assert.ok(folders, "virtual workspace folders are missing");
  assert.equal(folders.length, 1, "virtual E2E must have exactly one provider-backed root");
  assert.ok(
    folders.every((folder) => folder.uri.scheme === VIRTUAL_SCHEME),
    "virtual E2E must not retain a local or differently backed workspace folder"
  );
  assert.equal(folders[0]?.uri.path, "/workspace", "virtual workspace root changed");
}

class MemoryFileSystemProvider implements vscode.FileSystemProvider, vscode.Disposable {
  private readonly emitter = new vscode.EventEmitter<vscode.FileChangeEvent[]>();
  private readonly files = new Map<string, { bytes: Uint8Array; mtime: number }>();

  public readonly onDidChangeFile = this.emitter.event;

  public constructor(
    private readonly root: vscode.Uri,
    seeded: ReadonlyMap<string, Uint8Array>
  ) {
    const now = Date.now();
    for (const [name, bytes] of seeded) {
      this.files.set(vscode.Uri.joinPath(root, name).toString(), {
        bytes: Uint8Array.from(bytes),
        mtime: now
      });
    }
  }

  public dispose(): void {
    this.emitter.dispose();
  }

  public watch(): vscode.Disposable {
    return new vscode.Disposable(() => undefined);
  }

  public stat(uri: vscode.Uri): vscode.FileStat {
    const file = this.files.get(uri.toString());
    if (file) {
      return {
        type: vscode.FileType.File,
        ctime: file.mtime,
        mtime: file.mtime,
        size: file.bytes.byteLength
      };
    }
    if (uri.path === "/" || uri.toString() === this.root.toString()) {
      return { type: vscode.FileType.Directory, ctime: 0, mtime: 0, size: 0 };
    }
    throw vscode.FileSystemError.FileNotFound(uri);
  }

  public readDirectory(uri: vscode.Uri): [string, vscode.FileType][] {
    if (uri.path === "/") {
      return [[path.posix.basename(this.root.path), vscode.FileType.Directory]];
    }
    if (uri.toString() !== this.root.toString()) {
      throw vscode.FileSystemError.FileNotFound(uri);
    }
    return [...this.files.keys()].map((value) => [
      path.posix.basename(vscode.Uri.parse(value).path),
      vscode.FileType.File
    ]);
  }

  public createDirectory(uri: vscode.Uri): void {
    if (uri.toString() !== this.root.toString()) {
      throw vscode.FileSystemError.NoPermissions(uri);
    }
  }

  public readFile(uri: vscode.Uri): Uint8Array {
    const file = this.files.get(uri.toString());
    if (!file) {
      throw vscode.FileSystemError.FileNotFound(uri);
    }
    return Uint8Array.from(file.bytes);
  }

  public writeFile(
    uri: vscode.Uri,
    content: Uint8Array,
    options: { readonly create: boolean; readonly overwrite: boolean }
  ): void {
    const key = uri.toString();
    const exists = this.files.has(key);
    if ((!exists && !options.create) || (exists && !options.overwrite)) {
      throw vscode.FileSystemError.NoPermissions(uri);
    }
    this.files.set(key, { bytes: Uint8Array.from(content), mtime: Date.now() });
    this.emitter.fire([
      { type: exists ? vscode.FileChangeType.Changed : vscode.FileChangeType.Created, uri }
    ]);
  }

  public delete(uri: vscode.Uri): void {
    if (!this.files.delete(uri.toString())) {
      throw vscode.FileSystemError.FileNotFound(uri);
    }
    this.emitter.fire([{ type: vscode.FileChangeType.Deleted, uri }]);
  }

  public rename(oldUri: vscode.Uri, newUri: vscode.Uri): void {
    const file = this.files.get(oldUri.toString());
    if (!file || this.files.has(newUri.toString())) {
      throw vscode.FileSystemError.NoPermissions(oldUri);
    }
    this.files.delete(oldUri.toString());
    this.files.set(newUri.toString(), file);
    this.emitter.fire([
      { type: vscode.FileChangeType.Deleted, uri: oldUri },
      { type: vscode.FileChangeType.Created, uri: newUri }
    ]);
  }
}

function delay(milliseconds: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}
