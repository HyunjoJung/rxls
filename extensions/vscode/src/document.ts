import * as path from "node:path";
import * as vscode from "vscode";

export type DocumentChangeKind = "changed" | "created" | "deleted";

export class RxlsPreviewDocument implements vscode.CustomDocument {
  private readonly changeEmitter = new vscode.EventEmitter<DocumentChangeKind>();
  private readonly disposables: vscode.Disposable[] = [];
  private debounce: NodeJS.Timeout | undefined;

  public readonly onDidChange = this.changeEmitter.event;

  public constructor(public readonly uri: vscode.Uri) {
    this.watch();
  }

  public dispose(): void {
    if (this.debounce) {
      clearTimeout(this.debounce);
    }
    for (const disposable of this.disposables) {
      disposable.dispose();
    }
    this.changeEmitter.dispose();
  }

  private watch(): void {
    const fileName = path.posix.basename(this.uri.path);
    if (!fileName) {
      return;
    }
    const parent = this.uri.with({ path: parentPath(this.uri.path) });
    try {
      const watcher = vscode.workspace.createFileSystemWatcher(
        new vscode.RelativePattern(parent, fileName),
        false,
        false,
        false
      );
      this.disposables.push(
        watcher,
        watcher.onDidChange(() => this.schedule("changed")),
        watcher.onDidCreate(() => this.schedule("created")),
        watcher.onDidDelete(() => this.schedule("deleted"))
      );
    } catch {
      // Provider-backed resources may not expose watch support. Manual reload remains available.
    }
  }

  private schedule(kind: DocumentChangeKind): void {
    if (this.debounce) {
      clearTimeout(this.debounce);
    }
    this.debounce = setTimeout(() => {
      this.debounce = undefined;
      this.changeEmitter.fire(kind);
    }, 120);
  }
}

function parentPath(value: string): string {
  const index = value.lastIndexOf("/");
  return index <= 0 ? "/" : value.slice(0, index);
}
