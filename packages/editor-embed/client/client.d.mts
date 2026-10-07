export interface EditorState {
  readonly loaded: boolean; readonly busy: boolean; readonly dirty: boolean;
  readonly draft: boolean; readonly pendingMutation: boolean;
  readonly canUndo: boolean; readonly canRedo: boolean;
  readonly fileName: string | null; readonly format: "xls" | "xlsx" | "xlsm" | "xlsb" | "ods" | null;
  readonly sheetCount: number; readonly sheetIndex: number;
  readonly capability: "read-only" | "read-write" | null; readonly reason: string | null;
}
export interface RuntimeIdentity { readonly bundleId: string; readonly packageVersion: string; readonly workerProtocol: string; }
export interface Diagnostic { readonly code: string; readonly message: string; }
export interface SavedWorkbook { readonly bytes: Uint8Array; readonly fileName: string; readonly format: "xlsx" | "xlsm"; readonly mimeType: string; readonly state: EditorState; }
export interface Editor {
  readonly instanceId: string;
  readonly ready: Promise<{ readonly state: EditorState; readonly runtime: RuntimeIdentity }>;
  getState(): EditorState | null;
  getRuntimeIdentity(): RuntimeIdentity | null;
  load(bytes: Uint8Array, options: { fileName: string; replace?: "reject" | "discard" }): Promise<EditorState>;
  save(): Promise<SavedWorkbook>;
  onChange(listener: (state: EditorState) => void): () => void;
  onDiagnostic(listener: (diagnostic: Diagnostic) => void): () => void;
  dispose(): Promise<void>;
}
export function createEditor(container: HTMLElement, options: { assetsUrl: string | URL; title?: string; expectedBundleId?: string }): Editor;
