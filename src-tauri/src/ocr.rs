use std::path::Path;
use std::process::Command;

use crate::tools::{clean_text, first_line};

pub fn read_image(path: &Path) -> Result<String, String> {
    let output = Command::new("tesseract")
        .arg(path)
        .arg("stdout")
        .arg("-l")
        .arg("eng")
        .output()
        .map_err(|error| format!("Could not run Tesseract. {error}"))?;

    if !output.status.success() {
        let detail = first_line(&String::from_utf8_lossy(&output.stderr));
        if detail.is_empty() {
            return Err("Tesseract could not read this image.".into());
        }
        return Err(format!("Tesseract could not read this image. {detail}"));
    }

    Ok(clean_text(&String::from_utf8_lossy(&output.stdout)))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::read_image;

    #[test]
    fn reads_the_dishwasher_warranty_card() {
        if !PathBuf::from("/usr/bin/tesseract").is_file() {
            return;
        }
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../fixtures/samples/warranty-dishwasher.png");
        let text = read_image(&path).unwrap().to_lowercase();
        assert!(
            text.contains("dishwasher warranty"),
            "unexpected OCR text:\n{text}"
        );
    }

    #[test]
    fn reads_the_hardware_total() {
        if !PathBuf::from("/usr/bin/tesseract").is_file() {
            return;
        }
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../fixtures/samples/receipt-cedar-hardware.png");
        let text = read_image(&path).unwrap();
        assert!(text.contains("42.99"), "unexpected OCR text:\n{text}");
        assert!(text.to_lowercase().contains("total"), "unexpected OCR text:\n{text}");
    }
}
