import { randomUUID } from "node:crypto";
import * as path from "node:path";
import * as vscode from "vscode";

import { DocumentChangeKind, RxlsPreviewDocument } from "./document";
import {
  ExportKind,
  ExportMessage,
  MAX_OPEN_PREVIEWS,
  MAX_SESSION_BYTES,
  MAX_WORKBOOK_BYTES,
  WebviewMessage,
  parentUriPath,
  parseWebviewMessage
} from "./protocol";

export const VIEW_TYPE = "rxls.spreadsheetPreview";
export const TEST_CRASH_COMMAND = "rxls.preview.__test.crashRenderer";

export interface PreviewEvent {
  uri: string;
  generation: number;
  status: "loading" | "loaded" | "error";
  code?: string;
  message?: string;
  preview?: Record<string, unknown>;
}

export interface ExportResult {
  kind: ExportKind;
  fileName: string;
  bytes: Uint8Array;
}

export interface RxlsPreviewApi {
  readonly onDidChangePreview: vscode.Event<PreviewEvent>;
  reload(uri: vscode.Uri): Promise<void>;
  requestExport(uri: vscode.Uri, kind: ExportKind): Promise<ExportResult>;
}

interface PendingExport {
  kind: ExportKind;
  resolve(value: ExportResult): void;
  reject(error: Error): void;
  timer: NodeJS.Timeout;
}

interface PanelSession {
  readonly document: RxlsPreviewDocument;
  readonly panel: vscode.WebviewPanel;
  readonly disposables: vscode.Disposable[];
  readonly pendingExports: Map<string, PendingExport>;
  ready: boolean;
  disposed: boolean;
  generation: number;
  retainedBytes: number;
}

export class RxlsPreviewProvider implements vscode.CustomReadonlyEditorProvider<RxlsPreviewDocument> {
  private readonly sessions = new Map<vscode.WebviewPanel, PanelSession>();
  private readonly sessionsByUri = new Map<string, PanelSession>();
  private readonly previewEmitter = new vscode.EventEmitter<PreviewEvent>();
  private activeSession: PanelSession | undefined;
  private retainedBytes = 0;
  private loadQueue: Promise<void> = Promise.resolve();

  public readonly api: RxlsPreviewApi = Object.freeze({
    onDidChangePreview: this.previewEmitter.event,
    reload: async (uri: vscode.Uri) => this.reload(uri),
    requestExport: async (uri: vscode.Uri, kind: ExportKind) => this.requestExport(uri, kind)
  });

  public constructor(private readonly context: vscode.ExtensionContext) {}

  public dispose(): void {
    for (const session of [...this.sessions.values()]) {
      this.disposeSession(session);
    }
    this.previewEmitter.dispose();
  }

  public async openCustomDocument(
    uri: vscode.Uri,
    _openContext: vscode.CustomDocumentOpenContext,
    token: vscode.CancellationToken
  ): Promise<RxlsPreviewDocument> {
    if (token.isCancellationRequested) {
      throw new vscode.CancellationError();
    }
    return new RxlsPreviewDocument(uri);
  }

  public async resolveCustomEditor(
    document: RxlsPreviewDocument,
    panel: vscode.WebviewPanel,
    token: vscode.CancellationToken
  ): Promise<void> {
    if (token.isCancellationRequested) {
      return;
    }
    const viewerRoot = vscode.Uri.joinPath(this.context.extensionUri, "media", "viewer");
    panel.webview.options = {
      enableScripts: true,
      localResourceRoots: [viewerRoot]
    };

    const session: PanelSession = {
      document,
      panel,
      disposables: [],
      pendingExports: new Map(),
      ready: false,
      disposed: false,
      generation: 0,
      retainedBytes: 0
    };
    this.sessions.set(panel, session);
    this.sessionsByUri.set(document.uri.toString(), session);
    this.activeSession = session;

    session.disposables.push(
      panel.webview.onDidReceiveMessage((value: unknown) => this.onMessage(session, value)),
      panel.onDidDispose(() => this.disposeSession(session)),
      panel.onDidChangeViewState(({ webviewPanel }) => {
        if (webviewPanel.active) {
          this.activeSession = session;
        }
      }),
      document.onDidChange((kind) => this.onDocumentChange(session, kind))
    );

    panel.webview.html = await this.webviewHtml(panel.webview, viewerRoot);
  }

  public async reloadActive(): Promise<void> {
    if (!this.activeSession || this.activeSession.disposed) {
      void vscode.window.showInformationMessage("No active rxls spreadsheet preview.");
      return;
    }
    await this.load(this.activeSession, "manual");
  }

  public async crashRendererForTest(uri: vscode.Uri): Promise<void> {
    if (this.context.extensionMode !== vscode.ExtensionMode.Test) {
      throw new Error("The renderer crash hook is only available in extension tests.");
    }
    const session = this.sessionsByUri.get(uri.toString());
    if (!session || session.disposed || !session.ready) {
      throw new Error("The rxls preview is not ready.");
    }
    if (
      !(await session.panel.webview.postMessage({
        type: "host-command",
        command: "test-crash-renderer"
      }))
    ) {
      throw new Error("The rxls preview closed before the crash test.");
    }
  }

  private async reload(uri: vscode.Uri): Promise<void> {
    const session = this.sessionsByUri.get(uri.toString());
    if (!session || session.disposed) {
      throw new Error("The rxls preview is not open.");
    }
    await this.load(session, "api");
  }

  private async requestExport(uri: vscode.Uri, kind: ExportKind): Promise<ExportResult> {
    const session = this.sessionsByUri.get(uri.toString());
    if (!session || session.disposed || !session.ready) {
      throw new Error("The rxls preview is not ready.");
    }
    const requestId = randomUUID();
    const result = new Promise<ExportResult>((resolve, reject) => {
      const timer = setTimeout(() => {
        session.pendingExports.delete(requestId);
        reject(new Error("The rxls export request timed out."));
      }, 30_000);
      session.pendingExports.set(requestId, { kind, resolve, reject, timer });
    });
    const posted = await session.panel.webview.postMessage({
      type: "host-command",
      command: kind === "svg" ? "export-svg" : "export-png",
      requestId
    });
    if (!posted) {
      const pending = session.pendingExports.get(requestId);
      if (pending) {
        clearTimeout(pending.timer);
        session.pendingExports.delete(requestId);
        pending.reject(new Error("The rxls preview closed before export."));
      }
    }
    return result;
  }

  private async onMessage(session: PanelSession, value: unknown): Promise<void> {
    const message = parseWebviewMessage(value);
    if (!message || session.disposed) {
      return;
    }
    switch (message.type) {
      case "ready":
        session.ready = true;
        await this.load(session, "ready");
        break;
      case "reload":
        await this.load(session, "webview");
        break;
      case "loaded":
        if (message.generation === session.generation) {
          this.previewEmitter.fire({
            uri: session.document.uri.toString(),
            generation: message.generation,
            status: "loaded",
            preview: { ...message.preview }
          });
        }
        break;
      case "preview-error":
        if (message.generation === session.generation) {
          this.previewEmitter.fire({
            uri: session.document.uri.toString(),
            generation: message.generation,
            status: "error",
            code: message.code,
            message: message.message
          });
        }
        break;
      case "export":
        await this.onExport(session, message);
        break;
    }
  }

  private async onExport(session: PanelSession, message: ExportMessage): Promise<void> {
    if (message.requestId) {
      const pending = session.pendingExports.get(message.requestId);
      if (pending) {
        clearTimeout(pending.timer);
        session.pendingExports.delete(message.requestId);
        if (pending.kind !== message.kind) {
          pending.reject(new Error("The rxls export response kind did not match the request."));
        } else {
          pending.resolve({ kind: message.kind, fileName: message.fileName, bytes: message.bytes });
        }
      }
      return;
    }

    const parent = session.document.uri.with({ path: parentUriPath(session.document.uri.path) });
    const destination = await vscode.window.showSaveDialog({
      defaultUri: vscode.Uri.joinPath(parent, message.fileName),
      saveLabel: `Export ${message.kind.toUpperCase()}`,
      filters:
        message.kind === "svg"
          ? { "Scalable Vector Graphics": ["svg"] }
          : { "Portable Network Graphics": ["png"] }
    });
    if (!destination) {
      return;
    }
    await vscode.workspace.fs.writeFile(destination, message.bytes);
    await session.panel.webview.postMessage({
      type: "host-status",
      message: `${message.kind.toUpperCase()} exported`
    });
  }

  private onDocumentChange(session: PanelSession, kind: DocumentChangeKind): void {
    if (kind === "deleted") {
      void this.sendError(session, "file_deleted", "The source workbook was deleted.");
      return;
    }
    void this.load(session, "file-change");
  }

  private async load(session: PanelSession, _reason: string): Promise<void> {
    // Keep the global preview-count and retained-byte checks atomic across panels.
    const operation = this.loadQueue.then(
      () => this.loadNow(session),
      () => this.loadNow(session)
    );
    this.loadQueue = operation.catch(() => undefined);
    await operation;
  }

  private async loadNow(session: PanelSession): Promise<void> {
    if (!session.ready || session.disposed) {
      return;
    }
    const generation = ++session.generation;
    this.previewEmitter.fire({
      uri: session.document.uri.toString(),
      generation,
      status: "loading"
    });
    try {
      const name = path.posix.basename(session.document.uri.path);
      if (!/\.(?:xls|xlsx|xlsm|xlsb|ods)$/i.test(name)) {
        throw new HostError("unsupported_format", "The file is not a supported spreadsheet.");
      }
      const stat = await vscode.workspace.fs.stat(session.document.uri);
      if ((stat.type & vscode.FileType.Directory) !== 0) {
        throw new HostError("not_a_file", "The selected resource is not a file.");
      }
      if (stat.size > MAX_WORKBOOK_BYTES) {
        throw new HostError("limit_exceeded", "The workbook exceeds the 32 MiB preview limit.");
      }
      const loadedSessions = [...this.sessions.values()].filter(
        (candidate) => !candidate.disposed && candidate !== session && candidate.retainedBytes > 0
      );
      if (session.retainedBytes === 0 && loadedSessions.length >= MAX_OPEN_PREVIEWS) {
        throw new HostError(
          "session_limit",
          `At most ${MAX_OPEN_PREVIEWS} spreadsheet previews may be active.`
        );
      }
      const retainedWithoutCurrent = this.retainedBytes - session.retainedBytes;
      if (retainedWithoutCurrent + stat.size > MAX_SESSION_BYTES) {
        throw new HostError("aggregate_limit", "Active previews exceed the 128 MiB limit.");
      }
      const bytes = await vscode.workspace.fs.readFile(session.document.uri);
      if (session.disposed || generation !== session.generation) {
        return;
      }
      if (bytes.byteLength > MAX_WORKBOOK_BYTES) {
        throw new HostError("limit_exceeded", "The workbook exceeds the 32 MiB preview limit.");
      }
      const latestRetainedWithoutCurrent = this.retainedBytes - session.retainedBytes;
      if (latestRetainedWithoutCurrent + bytes.byteLength > MAX_SESSION_BYTES) {
        throw new HostError("aggregate_limit", "Active previews exceed the 128 MiB limit.");
      }
      const previousRetainedBytes = session.retainedBytes;
      this.retainedBytes = latestRetainedWithoutCurrent + bytes.byteLength;
      session.retainedBytes = bytes.byteLength;
      let posted: boolean;
      try {
        posted = await session.panel.webview.postMessage({
          type: "load",
          generation,
          file: { name },
          bytes: new Uint8Array(bytes)
        });
      } catch (error) {
        this.restoreRetainedBytes(session, bytes.byteLength, previousRetainedBytes);
        throw error;
      }
      if (!posted || session.disposed || generation !== session.generation) {
        if (!posted) {
          this.restoreRetainedBytes(session, bytes.byteLength, previousRetainedBytes);
        }
        return;
      }
    } catch (error) {
      if (session.disposed || generation !== session.generation) {
        return;
      }
      const hostError = asHostError(error);
      await this.sendError(session, hostError.code, hostError.message, generation);
    }
  }

  private async sendError(
    session: PanelSession,
    code: string,
    message: string,
    generation = ++session.generation
  ): Promise<void> {
    await session.panel.webview.postMessage({ type: "host-error", generation, code, message });
    this.previewEmitter.fire({
      uri: session.document.uri.toString(),
      generation,
      status: "error",
      code,
      message
    });
  }

  private disposeSession(session: PanelSession): void {
    if (session.disposed) {
      return;
    }
    session.disposed = true;
    this.sessions.delete(session.panel);
    if (this.sessionsByUri.get(session.document.uri.toString()) === session) {
      this.sessionsByUri.delete(session.document.uri.toString());
    }
    if (this.activeSession === session) {
      this.activeSession = undefined;
    }
    this.retainedBytes = Math.max(0, this.retainedBytes - session.retainedBytes);
    session.retainedBytes = 0;
    for (const disposable of session.disposables) {
      disposable.dispose();
    }
    for (const pending of session.pendingExports.values()) {
      clearTimeout(pending.timer);
      pending.reject(new Error("The rxls preview was closed."));
    }
    session.pendingExports.clear();
  }

  private restoreRetainedBytes(
    session: PanelSession,
    reservedBytes: number,
    previousBytes: number
  ): void {
    if (session.disposed || session.retainedBytes !== reservedBytes) {
      return;
    }
    this.retainedBytes = Math.max(0, this.retainedBytes - reservedBytes + previousBytes);
    session.retainedBytes = previousBytes;
  }

  private async webviewHtml(webview: vscode.Webview, viewerRoot: vscode.Uri): Promise<string> {
    const index = await vscode.workspace.fs.readFile(vscode.Uri.joinPath(viewerRoot, "index.html"));
    let html = new TextDecoder().decode(index);
    const resourceBase = `${webview.asWebviewUri(viewerRoot).toString().replace(/\/$/, "")}/`;
    const csp = [
      "default-src 'none'",
      `script-src ${webview.cspSource} 'wasm-unsafe-eval'`,
      `style-src ${webview.cspSource}`,
      `img-src ${webview.cspSource} data: blob:`,
      `font-src ${webview.cspSource}`,
      `worker-src ${webview.cspSource} blob:`,
      `connect-src ${webview.cspSource}`,
      `base-uri ${webview.cspSource}`,
      "object-src 'none'",
      "form-action 'none'"
    ].join("; ");
    const cspPattern = /<meta\s+http-equiv="Content-Security-Policy"[\s\S]*?\/>/i;
    if (!cspPattern.test(html) || !html.includes("<head>")) {
      throw new Error("The packaged rxls viewer template is invalid.");
    }
    html = html.replace(
      cspPattern,
      `<meta http-equiv="Content-Security-Policy" content="${escapeHtml(csp)}" />`
    );
    html = html.replace(
      "<head>",
      `<head>\n    <base href="${escapeHtml(resourceBase)}" />\n    <meta name="rxls-host-kind" content="vscode" />\n    <meta name="rxls-resource-base" content="${escapeHtml(resourceBase)}" />`
    );
    return html;
  }
}

class HostError extends Error {
  public constructor(public readonly code: string, message: string) {
    super(message);
  }
}

function asHostError(error: unknown): HostError {
  if (error instanceof HostError) {
    return error;
  }
  if (error instanceof vscode.FileSystemError && error.code === "FileNotFound") {
    return new HostError("file_deleted", "The source workbook is unavailable.");
  }
  return new HostError("read_failed", "The source workbook could not be read.");
}

function escapeHtml(value: string): string {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll('"', "&quot;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;");
}
