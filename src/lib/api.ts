import { invoke } from "@tauri-apps/api/core";

import type { AppInfo, DocumentDetail, DocumentSummary, ImportReport, SearchHit } from "./types";

export function inDesktopApp() {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

export function getAppInfo() {
  return invoke<AppInfo>("app_info");
}

export function listDocuments() {
  return invoke<DocumentSummary[]>("list_documents");
}

export function searchPapers(query: string) {
  return invoke<SearchHit[]>("search_papers", { query });
}

export function getDocument(id: number) {
  return invoke<DocumentDetail>("get_document", { id });
}

export function importPaths(paths: string[]) {
  return invoke<ImportReport>("import_paths", { paths });
}

export function importSamples() {
  return invoke<ImportReport>("import_samples");
}
