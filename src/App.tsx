import { convertFileSrc } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { ArrowLeft, Plus, Search } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";

import { HighlightedText, SnippetText } from "@/components/highlight";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Separator } from "@/components/ui/separator";
import {
  getAppInfo,
  getDocument,
  importPaths,
  importSamples,
  inDesktopApp,
  listDocuments,
  searchPapers,
} from "@/lib/api";
import type { AppInfo, DocumentDetail, DocumentSummary, SearchHit } from "@/lib/types";
import { cn } from "@/lib/utils";

const PAPER_EXTENSIONS = ["png", "jpg", "jpeg", "webp", "tif", "tiff", "gif", "pdf"];

function upsertDocument(list: DocumentSummary[], next: DocumentSummary) {
  const index = list.findIndex((document) => document.id === next.id);
  if (index === -1) {
    return [next, ...list];
  }
  const copy = list.slice();
  copy[index] = next;
  return copy;
}

function formatWhen(iso: string) {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) {
    return "";
  }
  return date.toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  });
}

function PaperThumb({ path, name }: { path: string | null; name: string }) {
  if (!path) {
    return (
      <div className="flex h-16 w-14 shrink-0 items-center justify-center rounded-md border border-border bg-accent font-serif text-lg text-primary">
        {name.slice(0, 1).toUpperCase()}
      </div>
    );
  }
  return (
    <img
      src={convertFileSrc(path)}
      alt=""
      className="h-16 w-14 shrink-0 rounded-md border border-border bg-white object-cover object-top"
    />
  );
}

function statusBadge(status: string) {
  if (status === "reading") {
    return <Badge variant="reading">Reading</Badge>;
  }
  if (status === "error") {
    return <Badge variant="error">Could not read</Badge>;
  }
  return null;
}

export default function App() {
  const desktop = inDesktopApp();
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [documents, setDocuments] = useState<DocumentSummary[]>([]);
  const [hits, setHits] = useState<SearchHit[] | null>(null);
  const [query, setQuery] = useState("");
  const [banner, setBanner] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [importing, setImporting] = useState(false);
  const [dragging, setDragging] = useState(false);
  const [selectedId, setSelectedId] = useState<number | null>(null);
  const [selectedPage, setSelectedPage] = useState(1);
  const [detail, setDetail] = useState<DocumentDetail | null>(null);
  const [detailError, setDetailError] = useState<string | null>(null);
  const queryRef = useRef(query);
  queryRef.current = query;

  const refresh = useCallback(async (nextQuery = queryRef.current) => {
    const listed = await listDocuments();
    setDocuments(listed);
    const trimmed = nextQuery.trim();
    if (!trimmed) {
      setHits(null);
      return;
    }
    setHits(await searchPapers(trimmed));
  }, []);

  useEffect(() => {
    if (!desktop) {
      return;
    }
    let cancelled = false;
    getAppInfo()
      .then((next) => {
        if (!cancelled) {
          setInfo(next);
        }
      })
      .catch((error: unknown) => {
        if (!cancelled) {
          setLoadError(error instanceof Error ? error.message : String(error));
        }
      });
    refresh().catch((error: unknown) => {
      if (!cancelled) {
        setLoadError(error instanceof Error ? error.message : String(error));
      }
    });
    return () => {
      cancelled = true;
    };
  }, [desktop, refresh]);

  useEffect(() => {
    if (!desktop) {
      return;
    }
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    void import("@tauri-apps/api/event").then(async ({ listen }) => {
      if (cancelled) {
        return;
      }
      unlisten = await listen<DocumentSummary>("import-progress", (event) => {
        setDocuments((current) => upsertDocument(current, event.payload));
        if (event.payload.status !== "reading" && queryRef.current.trim()) {
          void searchPapers(queryRef.current.trim())
            .then(setHits)
            .catch(() => undefined);
        }
      });
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [desktop]);

  useEffect(() => {
    if (!desktop) {
      return;
    }
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    void import("@tauri-apps/api/webviewWindow").then(async ({ getCurrentWebviewWindow }) => {
      if (cancelled) {
        return;
      }
      unlisten = await getCurrentWebviewWindow().onDragDropEvent((event) => {
        if (event.payload.type === "enter" || event.payload.type === "over") {
          setDragging(true);
        }
        if (event.payload.type === "leave") {
          setDragging(false);
        }
        if (event.payload.type === "drop") {
          setDragging(false);
          void addPaths(event.payload.paths);
        }
      });
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [desktop]);

  useEffect(() => {
    if (!desktop) {
      return;
    }
    const handle = window.setTimeout(() => {
      const trimmed = query.trim();
      if (!trimmed) {
        setHits(null);
        return;
      }
      searchPapers(trimmed)
        .then(setHits)
        .catch((error: unknown) => {
          setBanner(error instanceof Error ? error.message : String(error));
        });
    }, 140);
    return () => window.clearTimeout(handle);
  }, [desktop, query]);

  useEffect(() => {
    if (!desktop || selectedId == null) {
      setDetail(null);
      return;
    }
    let cancelled = false;
    setDetailError(null);
    getDocument(selectedId)
      .then((next) => {
        if (!cancelled) {
          setDetail(next);
        }
      })
      .catch((error: unknown) => {
        if (!cancelled) {
          setDetailError(error instanceof Error ? error.message : String(error));
        }
      });
    return () => {
      cancelled = true;
    };
  }, [desktop, selectedId]);

  async function addPaths(paths: string[]) {
    if (!paths.length) {
      return;
    }
    setImporting(true);
    setBanner(null);
    setLoadError(null);
    try {
      const report = await importPaths(paths);
      if (report.skipped.length) {
        setBanner(report.skipped.join(" "));
      }
      await refresh();
    } catch (error) {
      setBanner(error instanceof Error ? error.message : String(error));
    } finally {
      setImporting(false);
    }
  }

  async function chooseFiles() {
    const picked = await open({
      multiple: true,
      title: "Add papers",
      filters: [{ name: "Papers", extensions: PAPER_EXTENSIONS }],
    });
    if (picked == null) {
      return;
    }
    await addPaths(Array.isArray(picked) ? picked : [picked]);
  }

  async function loadSamples() {
    setImporting(true);
    setBanner(null);
    try {
      const report = await importSamples();
      const already = report.skipped.filter((line) => line.includes("already"));
      if (already.length && report.documents.length === 0) {
        setBanner("Those sample papers are already in the shoebox.");
      }
      await refresh();
    } catch (error) {
      setBanner(error instanceof Error ? error.message : String(error));
    } finally {
      setImporting(false);
    }
  }

  function openDocument(id: number, page = 1) {
    setSelectedPage(page);
    setSelectedId(id);
  }

  const trimmedQuery = query.trim();
  const showingSearch = trimmedQuery.length > 0;
  const emptyLibrary = documents.length === 0 && !showingSearch;
  const missingTools =
    info != null && (!info.tesseract || !info.pdftotext || !info.pdftoppm || !info.pdfinfo);
  const activePage =
    detail?.pages.find((page) => page.pageNumber === selectedPage) ?? detail?.pages[0];

  return (
    <div className="flex h-full min-h-0 flex-col bg-background text-foreground">
      <header className="flex flex-col gap-4 px-4 py-4 sm:px-6">
        <div className="flex items-end justify-between gap-4">
          <div>
            <p className="font-serif text-3xl leading-none tracking-tight">Shoebox</p>
            <p className="mt-1 text-sm text-muted-foreground">
              Receipts, warranties, and letters. Nothing leaves this machine.
            </p>
          </div>
          <Button onClick={() => void chooseFiles()} disabled={!desktop || importing}>
            <Plus />
            Add papers
          </Button>
        </div>
        <div className="relative">
          <Search className="pointer-events-none absolute top-1/2 left-4 size-4 -translate-y-1/2 text-muted-foreground" />
          <Input
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder="Search the shoebox"
            aria-label="Search"
            className="pl-11"
            disabled={!desktop}
          />
        </div>
      </header>

      {missingTools && (
        <div role="alert" className="mx-4 mb-3 rounded-2xl bg-primary/10 px-4 py-3 text-sm text-primary sm:mx-6">
          {!info?.tesseract && "Tesseract is missing, so photos cannot be read. "}
          {(!info?.pdftotext || !info?.pdftoppm || !info?.pdfinfo) &&
            "Poppler is missing, so PDFs cannot be read. "}
          Install tesseract-ocr and poppler-utils, then reopen Shoebox.
        </div>
      )}
      {!desktop && (
        <div role="status" className="mx-4 mb-3 rounded-2xl bg-accent px-4 py-3 text-sm sm:mx-6">
          Dropping papers and search run in the Shoebox window. This page is the same interface.
        </div>
      )}
      {loadError && (
        <div role="alert" className="mx-4 mb-3 rounded-2xl bg-primary/10 px-4 py-3 text-sm text-primary sm:mx-6">
          {loadError}
        </div>
      )}
      {banner && (
        <div role="status" className="mx-4 mb-3 rounded-2xl bg-accent px-4 py-3 text-sm sm:mx-6">
          {banner}
        </div>
      )}

      <div className="flex min-h-0 flex-1 border-t border-border">
        <main className={cn("min-h-0 flex-1 overflow-y-auto", detail && "hidden md:block")}>
          {emptyLibrary ? (
            <EmptyState
              dragging={dragging}
              importing={importing}
              samplesAvailable={info?.samplesAvailable ?? false}
              desktop={desktop}
              onChoose={() => void chooseFiles()}
              onSamples={() => void loadSamples()}
            />
          ) : showingSearch ? (
            <SearchResults
              query={trimmedQuery}
              hits={hits}
              onOpen={openDocument}
            />
          ) : (
            <Library documents={documents} importing={importing} onOpen={openDocument} />
          )}
        </main>

        {selectedId != null && (
          <aside className="flex min-h-0 w-full flex-col border-border bg-card md:w-[min(460px,46%)] md:border-l">
            <div className="flex items-center gap-2 px-4 py-3">
              <Button variant="ghost" size="sm" onClick={() => setSelectedId(null)}>
                <ArrowLeft />
                Back to papers
              </Button>
            </div>
            <Separator />
            <div className="min-h-0 flex-1 overflow-y-auto px-4 py-4">
              {detailError && (
                <p role="alert" className="text-sm text-primary">
                  {detailError}
                </p>
              )}
              {!detail && !detailError && <p className="text-sm text-muted-foreground">Opening…</p>}
              {detail && (
                <article>
                  <h2 className="font-serif text-2xl leading-tight">{detail.originalName}</h2>
                  {detail.error && (
                    <p className="mt-2 text-sm text-primary">{detail.error}</p>
                  )}
                  {detail.note && <p className="mt-2 text-sm text-muted-foreground">{detail.note}</p>}
                  {detail.pages.length > 1 && (
                    <div className="mt-4 flex flex-wrap gap-2">
                      {detail.pages.map((page) => (
                        <Button
                          key={page.pageNumber}
                          size="sm"
                          variant={page.pageNumber === activePage?.pageNumber ? "default" : "outline"}
                          onClick={() => setSelectedPage(page.pageNumber)}
                        >
                          Page {page.pageNumber}
                        </Button>
                      ))}
                    </div>
                  )}
                  {activePage?.imagePath && (
                    <img
                      src={convertFileSrc(activePage.imagePath)}
                      alt={`Page ${activePage.pageNumber} of ${detail.originalName}`}
                      className="mt-4 max-h-72 w-full rounded-xl border border-border bg-white object-contain object-top"
                    />
                  )}
                  <div className="mt-4 text-sm leading-6 whitespace-pre-wrap">
                    {activePage?.text ? (
                      <HighlightedText text={activePage.text} query={trimmedQuery} />
                    ) : (
                      <p className="text-muted-foreground">
                        Shoebox could not read any words on this page.
                      </p>
                    )}
                  </div>
                  <p className="mt-6 text-xs break-all text-muted-foreground">{detail.storedPath}</p>
                </article>
              )}
            </div>
          </aside>
        )}
      </div>

      <footer className="border-t border-border px-4 py-2 text-xs text-muted-foreground sm:px-6">
        {info?.dataDir
          ? `Stored in ${info.dataDir}. Nothing is uploaded.`
          : "Stored on this machine. Nothing is uploaded."}
      </footer>

      {dragging && (
        <div className="pointer-events-none fixed inset-3 rounded-3xl border-2 border-dashed border-primary bg-primary/10" />
      )}
    </div>
  );
}

function EmptyState({
  dragging,
  importing,
  samplesAvailable,
  desktop,
  onChoose,
  onSamples,
}: {
  dragging: boolean;
  importing: boolean;
  samplesAvailable: boolean;
  desktop: boolean;
  onChoose: () => void;
  onSamples: () => void;
}) {
  return (
    <div className="flex h-full items-center justify-center px-6 py-10">
      <div
        className={cn(
          "w-full max-w-xl rounded-[2rem] border-2 border-dashed border-border bg-card px-8 py-14 text-center",
          dragging && "border-primary bg-accent",
        )}
      >
        <p className="font-serif text-4xl leading-tight">Drop receipts, warranties, and letters</p>
        <p className="mx-auto mt-3 max-w-sm text-muted-foreground">
          They stay on this machine. Shoebox reads the words, then you can search them with the
          network off.
        </p>
        <div className="mt-6 flex flex-wrap items-center justify-center gap-3">
          <Button onClick={onChoose} disabled={!desktop || importing}>
            <Plus />
            Add papers
          </Button>
          {samplesAvailable && (
            <Button variant="outline" onClick={onSamples} disabled={importing}>
              {importing ? "Reading sample papers…" : "Try sample papers"}
            </Button>
          )}
        </div>
      </div>
    </div>
  );
}

function Library({
  documents,
  importing,
  onOpen,
}: {
  documents: DocumentSummary[];
  importing: boolean;
  onOpen: (id: number, page?: number) => void;
}) {
  return (
    <ul className="divide-y divide-border">
      {importing && documents.every((document) => document.status !== "reading") && (
        <li className="px-6 py-4 text-sm text-muted-foreground">Reading…</li>
      )}
      {documents.map((document) => (
        <li key={document.id}>
          <button
            type="button"
            className="flex w-full items-center gap-4 px-4 py-3 text-left hover:bg-accent/70 sm:px-6"
            onClick={() => onOpen(document.id)}
          >
            <PaperThumb path={document.thumbPath} name={document.originalName} />
            <span className="min-w-0 flex-1">
              <span className="flex flex-wrap items-center gap-2">
                <span className="truncate font-medium">{document.originalName}</span>
                {statusBadge(document.status)}
              </span>
              <span className="mt-1 block text-sm text-muted-foreground">
                {document.status === "error"
                  ? document.error
                  : document.status === "reading"
                    ? "Reading the words on this paper."
                    : `${document.pageCount} ${document.pageCount === 1 ? "page" : "pages"} · ${formatWhen(document.importedAt)}`}
              </span>
            </span>
          </button>
        </li>
      ))}
    </ul>
  );
}

function SearchResults({
  query,
  hits,
  onOpen,
}: {
  query: string;
  hits: SearchHit[] | null;
  onOpen: (id: number, page?: number) => void;
}) {
  if (hits == null) {
    return <p className="px-6 py-8 text-sm text-muted-foreground">Searching…</p>;
  }
  if (hits.length === 0) {
    return (
      <div className="px-6 py-16 text-center">
        <p className="font-serif text-3xl">Nothing in the shoebox matches</p>
        <p className="mt-2 text-muted-foreground">“{query}”</p>
      </div>
    );
  }
  return (
    <ul className="divide-y divide-border">
      {hits.map((hit) => (
        <li key={`${hit.documentId}-${hit.pageNumber}`}>
          <button
            type="button"
            className="flex w-full items-start gap-4 px-4 py-3 text-left hover:bg-accent/70 sm:px-6"
            onClick={() => onOpen(hit.documentId, hit.pageNumber)}
          >
            <PaperThumb path={hit.thumbPath} name={hit.originalName} />
            <span className="min-w-0 flex-1">
              <span className="block truncate font-medium">{hit.originalName}</span>
              <span className="mt-1 block text-xs tracking-wide text-muted-foreground uppercase">
                Page {hit.pageNumber}
              </span>
              <span className="mt-1 block text-sm leading-6 text-foreground/90">
                {hit.snippet ? <SnippetText value={hit.snippet} /> : "Matched the file name."}
              </span>
            </span>
          </button>
        </li>
      ))}
    </ul>
  );
}
