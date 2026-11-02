use std::path::{Path, PathBuf};
use std::process::Command;

use crate::ocr;
use crate::tools::{clean_text, first_line, Tools};

const MAX_PAGES: i32 = 30;
const TEXT_PAGE_LETTERS: usize = 20;

pub struct PdfPage {
    pub number: i32,
    pub text: String,
    pub image_path: Option<PathBuf>,
    pub warning: Option<String>,
}

pub struct PdfRead {
    pub pages: Vec<PdfPage>,
    pub truncated: bool,
}

pub fn read_pdf(path: &Path, dir: &Path, tools: Tools) -> Result<PdfRead, String> {
    if !tools.pdftotext || !tools.pdfinfo {
        return Err(
            "Poppler is missing, so PDFs cannot be read. Install poppler-utils.".into(),
        );
    }

    let total = page_count(path)?;
    if total == 0 {
        return Err("This PDF has no pages.".into());
    }
    let truncated = total > MAX_PAGES;
    let last = total.min(MAX_PAGES);
    let mut pages = Vec::with_capacity(last as usize);

    for number in 1..=last {
        let extracted = page_text(path, number).unwrap_or_default();
        let image_path = if tools.pdftoppm {
            render_page(path, number, dir).ok()
        } else {
            None
        };

        let (text, warning) = if letter_count(&extracted) >= TEXT_PAGE_LETTERS {
            (extracted, None)
        } else if let Some(image) = &image_path {
            if !tools.tesseract {
                (
                    String::new(),
                    Some(
                        "This page looks like a scan, and Tesseract is not installed.".to_string(),
                    ),
                )
            } else {
                match ocr::read_image(image) {
                    Ok(text) => (text, None),
                    Err(error) => (String::new(), Some(error)),
                }
            }
        } else {
            (
                extracted,
                Some("This page had no text layer and could not be rendered.".into()),
            )
        };

        pages.push(PdfPage {
            number,
            text,
            image_path,
            warning,
        });
    }

    Ok(PdfRead { pages, truncated })
}

fn page_count(path: &Path) -> Result<i32, String> {
    let output = Command::new("pdfinfo")
        .arg(path)
        .output()
        .map_err(|error| format!("Could not run pdfinfo. {error}"))?;
    if !output.status.success() {
        let detail = first_line(&String::from_utf8_lossy(&output.stderr));
        if detail.is_empty() {
            return Err("Could not open this PDF.".into());
        }
        return Err(format!("Could not open this PDF. {detail}"));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("Pages:") {
            if let Ok(count) = rest.trim().parse::<i32>() {
                return Ok(count);
            }
        }
    }
    Err("Could not tell how many pages this PDF has.".into())
}

fn page_text(path: &Path, number: i32) -> Result<String, String> {
    let page = number.to_string();
    let output = Command::new("pdftotext")
        .args(["-enc", "UTF-8", "-layout", "-f", &page, "-l", &page])
        .arg(path)
        .arg("-")
        .output()
        .map_err(|error| format!("Could not run pdftotext. {error}"))?;
    if !output.status.success() {
        let detail = first_line(&String::from_utf8_lossy(&output.stderr));
        if detail.is_empty() {
            return Err("Could not read the text in this PDF.".into());
        }
        return Err(format!("Could not read the text in this PDF. {detail}"));
    }
    Ok(clean_text(&String::from_utf8_lossy(&output.stdout)))
}

fn render_page(path: &Path, number: i32, dir: &Path) -> Result<PathBuf, String> {
    let page = number.to_string();
    let prefix = dir.join(format!("page-{number}"));
    let output = Command::new("pdftoppm")
        .args(["-png", "-r", "130", "-f", &page, "-l", &page, "-singlefile"])
        .arg(path)
        .arg(&prefix)
        .output()
        .map_err(|error| format!("Could not run pdftoppm. {error}"))?;
    if !output.status.success() {
        return Err(first_line(&String::from_utf8_lossy(&output.stderr)));
    }
    let image = PathBuf::from(format!("{}.png", prefix.display()));
    if image.is_file() {
        Ok(image)
    } else {
        Err("The PDF page did not render.".into())
    }
}

fn letter_count(text: &str) -> usize {
    text.chars().filter(|ch| ch.is_alphanumeric()).count()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::read_pdf;
    use crate::tools::Tools;

    #[test]
    fn letter_pdf_keeps_the_name_from_the_text_layer() {
        let tools = Tools::detect();
        if !tools.pdftotext {
            return;
        }
        let dir = std::env::temp_dir().join(format!("shoebox-letter-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../fixtures/samples/letter-mara-ellison.pdf");
        let read = read_pdf(&path, &dir, tools).unwrap();
        let text = read.pages[0].text.to_lowercase();
        assert!(text.contains("mara ellison"), "{text}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scanned_pdf_is_read_with_ocr() {
        let tools = Tools::detect();
        if !tools.pdftotext || !tools.pdftoppm || !tools.tesseract {
            return;
        }
        let dir = std::env::temp_dir().join(format!("shoebox-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/samples/scan-market-receipt.pdf");
        let read = read_pdf(&path, &dir, tools).unwrap();
        let text = read.pages[0].text.to_lowercase();
        assert!(text.contains("northline"), "{text}");
        assert!(read.pages[0].image_path.is_some());
        let _ = fs::remove_dir_all(&dir);
    }
}
