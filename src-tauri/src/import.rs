use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use image::imageops::FilterType;
use tauri::{AppHandle, Emitter, Manager};

use crate::db::{self, DocumentSummary, NewPage};
use crate::ocr;
use crate::pdf;
use crate::tools::Tools;

const MAX_BYTES: u64 = 40 * 1024 * 1024;

pub struct AppState {
    pub db: Mutex<rusqlite::Connection>,
    pub data_dir: PathBuf,
    pub tools: Tools,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub documents: Vec<DocumentSummary>,
    pub skipped: Vec<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub tesseract: bool,
    pub pdftotext: bool,
    pub pdftoppm: bool,
    pub pdfinfo: bool,
    pub samples_available: bool,
    pub data_dir: String,
}

pub fn app_info(state: &AppState) -> AppInfo {
    AppInfo {
        tesseract: state.tools.tesseract,
        pdftotext: state.tools.pdftotext,
        pdftoppm: state.tools.pdftoppm,
        pdfinfo: state.tools.pdfinfo,
        samples_available: samples_dir().is_some(),
        data_dir: state.data_dir.display().to_string(),
    }
}

pub fn samples_dir() -> Option<PathBuf> {
    let candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/samples");
    let cleaned = std::fs::canonicalize(&candidate).unwrap_or(candidate);
    if cleaned.is_dir() {
        Some(cleaned)
    } else {
        None
    }
}

pub fn ingest_paths(
    app: &AppHandle,
    state: &AppState,
    paths: Vec<String>,
) -> Result<ImportReport, String> {
    let mut documents = Vec::new();
    let mut skipped = Vec::new();
    for raw in paths {
        let path = PathBuf::from(&raw);
        if path.is_dir() {
            skipped.push(format!(
                "{} is a folder. Drop the papers inside it.",
                display_name(&path)
            ));
            continue;
        }
        if !path.is_file() {
            skipped.push(format!("Could not find {raw}."));
            continue;
        }
        documents.push(import_one(app, state, &path)?);
    }
    Ok(ImportReport {
        documents,
        skipped,
    })
}

pub fn ingest_samples(app: &AppHandle, state: &AppState) -> Result<ImportReport, String> {
    let Some(dir) = samples_dir() else {
        return Err("The sample papers are not on this machine.".into());
    };
    let ready = {
        let conn = state.db.lock().map_err(|error| error.to_string())?;
        db::ready_names(&conn)?
    };
    let mut paths = Vec::new();
    let mut skipped = Vec::new();
    let mut entries = fs::read_dir(&dir).map_err(|error| error.to_string())?;
    let mut files = Vec::new();
    while let Some(entry) = entries.next() {
        let entry = entry.map_err(|error| error.to_string())?;
        files.push(entry.path());
    }
    files.sort();
    for path in files {
        if !path.is_file() {
            continue;
        }
        let name = display_name(&path);
        if !is_supported(&path) {
            continue;
        }
        if ready.iter().any(|existing| existing == &name) {
            skipped.push(format!("{name} is already in the shoebox."));
            continue;
        }
        paths.push(path.display().to_string());
    }
    let mut report = ingest_paths(app, state, paths)?;
    report.skipped.splice(0..0, skipped);
    Ok(report)
}

fn import_one(app: &AppHandle, state: &AppState, source: &Path) -> Result<DocumentSummary, String> {
    let name = display_name(source);
    let id = {
        let conn = state.db.lock().map_err(|error| error.to_string())?;
        db::insert_document(&conn, &name)?
    };
    emit(app, state, id);

    let outcome = (|| -> Result<(), String> {
        let bytes = source.metadata().map_err(|error| error.to_string())?.len();
        if bytes > MAX_BYTES {
            return Err("This file is larger than 40 MB.".into());
        }
        let ext = extension(source);
        if !is_supported(source) {
            return Err("Shoebox reads photos (PNG, JPEG, WebP, TIFF, GIF) and PDFs.".into());
        }
        let dir = state.data_dir.join("library").join(id.to_string());
        fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        let stored = dir.join(format!("original.{ext}"));
        fs::copy(source, &stored).map_err(|error| format!("Could not copy this file. {error}"))?;
        {
            let conn = state.db.lock().map_err(|error| error.to_string())?;
            db::set_stored_path(&conn, id, &stored.display().to_string())?;
        }

        if ext == "pdf" {
            store_pdf(state, id, &stored, &dir)
        } else {
            store_image(state, id, &stored, &dir)
        }
    })();

    if let Err(message) = outcome {
        let conn = state.db.lock().map_err(|error| error.to_string())?;
        db::mark_error(&conn, id, &message)?;
    }
    let summary = {
        let conn = state.db.lock().map_err(|error| error.to_string())?;
        db::get_summary(&conn, id)?
    };
    let _ = app.emit("import-progress", &summary);
    Ok(summary)
}

fn store_image(state: &AppState, id: i64, stored: &Path, dir: &Path) -> Result<(), String> {
    if !state.tools.tesseract {
        return Err("Tesseract is not installed, so this photo cannot be read.".into());
    }
    let (page_image, thumb) = write_image_variants(stored, dir)?;
    let text = ocr::read_image(&page_image)?;
    let conn = state.db.lock().map_err(|error| error.to_string())?;
    db::finish_document(
        &conn,
        id,
        "ready",
        None,
        None,
        Some(&thumb.display().to_string()),
        &[NewPage {
            page_number: 1,
            image_path: Some(page_image.display().to_string()),
            text,
        }],
    )
}

fn store_pdf(state: &AppState, id: i64, stored: &Path, dir: &Path) -> Result<(), String> {
    let read = pdf::read_pdf(stored, dir, state.tools)?;
    let thumb = thumbnail_from_first_image(dir, &read.pages);
    let any_text = read.pages.iter().any(|page| !page.text.trim().is_empty());
    let warning = read
        .pages
        .iter()
        .find_map(|page| page.warning.clone());
    let (status, error) = if any_text {
        ("ready", None)
    } else if let Some(warning) = warning {
        ("error", Some(warning))
    } else {
        ("error", Some("No text could be read from this PDF.".into()))
    };
    let note = if read.truncated {
        Some("Only the first 30 pages were read.".to_string())
    } else {
        None
    };
    let pages = read
        .pages
        .into_iter()
        .map(|page| NewPage {
            page_number: page.number,
            image_path: page.image_path.map(|path| path.display().to_string()),
            text: page.text,
        })
        .collect::<Vec<_>>();
    let conn = state.db.lock().map_err(|error| error.to_string())?;
    db::finish_document(
        &conn,
        id,
        status,
        error.as_deref(),
        note.as_deref(),
        thumb.as_deref(),
        &pages,
    )
}

fn write_image_variants(source: &Path, dir: &Path) -> Result<(PathBuf, PathBuf), String> {
    let image = image::open(source).map_err(|error| format!("Could not open this image. {error}"))?;
    let display = if image.width().max(image.height()) > 2000 {
        image.resize(2000, 2000, FilterType::Triangle)
    } else {
        image
    };
    let page_path = dir.join("page-1.jpg");
    let thumb_path = dir.join("thumb.jpg");
    display
        .thumbnail(360, 360)
        .save(&thumb_path)
        .map_err(|error| format!("Could not save a thumbnail. {error}"))?;
    display
        .save(&page_path)
        .map_err(|error| format!("Could not save a preview. {error}"))?;
    Ok((page_path, thumb_path))
}

fn thumbnail_from_first_image(dir: &Path, pages: &[pdf::PdfPage]) -> Option<String> {
    let source = pages.iter().find_map(|page| page.image_path.as_ref())?;
    let image = image::open(source).ok()?;
    let thumb_path = dir.join("thumb.jpg");
    image.thumbnail(360, 360).save(&thumb_path).ok()?;
    Some(thumb_path.display().to_string())
}

fn emit(app: &AppHandle, state: &AppState, id: i64) {
    let summary = state
        .db
        .lock()
        .ok()
        .and_then(|conn| db::get_summary(&conn, id).ok());
    if let Some(summary) = summary {
        let _ = app.emit("import-progress", summary);
    }
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn is_supported(path: &Path) -> bool {
    matches!(
        extension(path).as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "tif" | "tiff" | "gif" | "pdf"
    )
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("paper")
        .to_string()
}

pub fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    Ok(dir)
}
