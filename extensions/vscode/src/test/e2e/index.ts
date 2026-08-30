import assert from "node:assert/strict";
import * as vscode from "vscode";

import {
  PreviewEvent,
  RxlsPreviewApi,
  TEST_CRASH_COMMAND,
  VIEW_TYPE
} from "../../provider";

const EXTENSION_ID = "HyunjoJung.rxls-spreadsheet-preview";

export async function run(): Promise<void> {
  const fixtures = parseFixtures(process.env.RXLS_E2E_FIXTURES);
  const expectedTrust = process.env.RXLS_EXPECT_TRUST === "true";
  assert.equal(vscode.workspace.isTrusted, expectedTrust, "workspace trust mode changed");
  const extension = vscode.extensions.getExtension<RxlsPreviewApi>(EXTENSION_ID);
  assert.ok(extension, `${EXTENSION_ID} is not installed`);
  const api = await extension.activate();
  const workspace = vscode.workspace.workspaceFolders?.[0];
  assert.ok(workspace, "E2E workspace is missing");

  for (const format of ["xls", "xlsx", "xlsm", "xlsb", "ods"] as const) {
    const uri = vscode.Uri.joinPath(workspace.uri, fixtures[format]);
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
    }

    await vscode.commands.executeCommand("workbench.action.closeActiveEditor");
    await delay(100);
  }

  if (expectedTrust) {
    await exerciseFailureBoundaries(api, workspace.uri, fixtures);
  }
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

function delay(milliseconds: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}
