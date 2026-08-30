import * as vscode from "vscode";

import {
  RxlsPreviewApi,
  RxlsPreviewProvider,
  TEST_CRASH_COMMAND,
  VIEW_TYPE
} from "./provider";

export function activate(context: vscode.ExtensionContext): RxlsPreviewApi {
  const provider = new RxlsPreviewProvider(context);
  context.subscriptions.push(
    provider,
    vscode.window.registerCustomEditorProvider(VIEW_TYPE, provider, {
      webviewOptions: { retainContextWhenHidden: false },
      supportsMultipleEditorsPerDocument: false
    }),
    vscode.commands.registerCommand("rxls.preview.reload", () => provider.reloadActive())
  );
  if (context.extensionMode === vscode.ExtensionMode.Test) {
    context.subscriptions.push(
      vscode.commands.registerCommand(TEST_CRASH_COMMAND, (uri: vscode.Uri) =>
        provider.crashRendererForTest(uri)
      )
    );
  }
  return provider.api;
}

export function deactivate(): void {}
