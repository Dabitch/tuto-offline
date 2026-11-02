export type AppInfo = {
  tesseract: boolean;
  pdftotext: boolean;
  pdftoppm: boolean;
  pdfinfo: boolean;
  samplesAvailable: boolean;
  dataDir: string;
};

export type DocumentSummary = {
  id: number;
  originalName: string;
  status: "reading" | "ready" | "error" | string;
  error: string | null;
  note: string | null;
  pageCount: number;
  thumbPath: string | null;
  importedAt: string;
};

export type SearchHit = {
  documentId: number;
  pageNumber: number;
  originalName: string;
  snippet: string;
  thumbPath: string | null;
};

export type PageDetail = {
  pageNumber: number;
  imagePath: string | null;
  text: string;
};

export type DocumentDetail = {
  id: number;
  originalName: string;
  storedPath: string;
  status: string;
  error: string | null;
  note: string | null;
  pages: PageDetail[];
};

export type ImportReport = {
  documents: DocumentSummary[];
  skipped: string[];
};
