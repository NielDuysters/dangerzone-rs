//! OCR backend powered by the `kreuzberg-tesseract` used for Linux

mod bindings;
mod helpers;

use std::path::PathBuf;

use anyhow::{Context, Result};
use kreuzberg_tesseract::{Pix, TesseractAPI};

use super::{OcrBackend, OcrPage, DEFAULT_DPI};

/// OCR backend powered by the `kreuzberg-tesseract` used for Linux
pub(crate) struct KreuzbergTesseractOcr {
    api: TesseractAPI,
}

impl KreuzbergTesseractOcr {
    pub(crate) fn new() -> Result<Self> {
        let api = TesseractAPI::new().context("Failed to create Tesseract API")?;
        let tessdata_dir = Self::tessdata_dir().context("Failed to find Tesseract tessdata")?;

        // Seed Tesseract with trained language data.
        // TODO: Currently we only support English. Support other languages too.
        api.init(&tessdata_dir, "eng")
            .context("Failed to initialize Tesseract API")?;

        Ok(Self { api })
    }

    /// Resolve the tessdata directory used to initialize Tesseract
    ///
    /// `TESSDATA_PREFIX` has priority when set. Otherwise we use the tessdata
    /// bundled by `kreuzberg-tesseract`.
    fn tessdata_dir() -> Option<PathBuf> {
        if let Ok(path) = std::env::var("TESSDATA_PREFIX") {
            return Some(Self::as_tessdata_dir(PathBuf::from(path)));
        }

        let mut candidates = Vec::new();

        if let Some(path) = option_env!("TESSDATA_PREFIX_BUNDLED") {
            candidates.push(Self::as_tessdata_dir(PathBuf::from(path)));
        }
        candidates.push(PathBuf::from("/usr/share/tesseract-ocr/5/tessdata"));
        candidates.push(PathBuf::from("/usr/share/tesseract-ocr/tessdata"));

        if let Ok(home) = std::env::var("HOME") {
            candidates.push(PathBuf::from(home).join(".kreuzberg-tesseract/tessdata"));
        }

        candidates.into_iter().find(|path| path.exists())
    }

    fn as_tessdata_dir(path: PathBuf) -> PathBuf {
        if path.ends_with("tessdata") {
            path
        } else {
            path.join("tessdata")
        }
    }
}

impl OcrBackend for KreuzbergTesseractOcr {
    fn ocr_page(&self, pixels: &[u8], width: u16, height: u16) -> OcrPage {
        // Pass container's bytes directly using Leptonica's Pix wrapper exposed
        // by `kreuzberg-tesseract`.
        let mut pix = match Pix::from_raw_rgb(pixels, width.into(), height.into()) {
            Ok(pix) => pix,
            Err(_) => return OcrPage::new(Vec::new()),
        };

        // The container renders pages at 150 DPI. Store that resolution on the
        // Pix as image metadata so Tesseract can interpret text size correctly.
        let _ = pix.set_resolution(DEFAULT_DPI, DEFAULT_DPI);

        // Give Tesseract the Leptonica image. `set_image_2` borrows the Pix
        // pointer; keep `pix` alive for the rest of this method.
        if self.api.set_image_2(pix.as_ptr()).is_err() {
            return OcrPage::new(Vec::new());
        }

        // Also set the source resolution on the Tesseract API. Some OCR paths
        // read DPI from the engine state rather than from the Pix metadata.
        let _ = self.api.set_source_resolution(DEFAULT_DPI);

        if self.api.recognize().is_err() {
            return OcrPage::new(Vec::new());
        }

        let iterator = match self.api.get_iterator() {
            Ok(iterator) => iterator,
            Err(_) => return OcrPage::new(Vec::new()),
        };

        OcrPage::new(helpers::extract_ocr_words(&iterator))
    }
}
