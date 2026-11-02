use std::process::Command;

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tools {
    pub tesseract: bool,
    pub pdftotext: bool,
    pub pdftoppm: bool,
    pub pdfinfo: bool,
}

impl Tools {
    pub fn detect() -> Self {
        Self {
            tesseract: binary_exists("tesseract"),
            pdftotext: binary_exists("pdftotext"),
            pdftoppm: binary_exists("pdftoppm"),
            pdfinfo: binary_exists("pdfinfo"),
        }
    }
}

fn binary_exists(name: &str) -> bool {
    Command::new(name).arg("--version").output().is_ok()
}

pub fn clean_text(text: &str) -> String {
    text.chars()
        .filter(|ch| {
            *ch == '\n' || *ch == '\t' || *ch == '\u{1}' || *ch == '\u{2}' || !ch.is_control()
        })
        .collect::<String>()
        .trim()
        .to_string()
}

pub fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .chars()
        .take(180)
        .collect()
}
