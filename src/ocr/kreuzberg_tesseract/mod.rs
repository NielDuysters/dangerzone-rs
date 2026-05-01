//! OCR backend powered by the `kreuzberg-tesseract` used for Linux

use std::path::PathBuf;

use anyhow::{Context, Result};
use kreuzberg_tesseract::{Pix, ResultIterator, TesseractAPI};

use crate::DPI;

use super::{OcrBackend, OcrPage, OcrVBox, OcrWord};

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
        // See https://github.com/freedomofpress/dangerzone-rs/issues/14
        api.init(&tessdata_dir, "eng")
            .context("Failed to initialize Tesseract API")?;

        Ok(Self { api })
    }

    /// Resolve the tessdata directory used to initialize Tesseract
    ///
    /// `TESSDATA_PREFIX` has priority when set. Otherwise we use the tessdata
    /// bundled by `kreuzberg-tesseract`.
    fn tessdata_dir() -> Option<PathBuf> {
        // Honor the standard Tesseract override first. Callers may point this
        // either at the tessdata directory itself or at its parent.
        if let Ok(path) = std::env::var("TESSDATA_PREFIX") {
            return Some(Self::as_tessdata_dir(PathBuf::from(path)));
        }

        let mut candidates = Vec::new();

        // `kreuzberg-tesseract`'s build script downloads bundled English
        // traineddata under {TESSDATA_PREFIX_BUNDLED}/tessdata and exports the
        // prefix at compile time.
        if let Some(path) = option_env!("TESSDATA_PREFIX_BUNDLED") {
            candidates.push(Self::as_tessdata_dir(PathBuf::from(path)));
        }

        // Common Linux package locations for Tesseract 5 and older distro
        // layouts. These are used when tessdata was installed system-wide.
        candidates.push(PathBuf::from("/usr/share/tesseract-ocr/5/tessdata"));
        candidates.push(PathBuf::from("/usr/share/tesseract-ocr/tessdata"));

        // Fallback to the Linux cache directory used by `kreuzberg-tesseract`
        // when its bundled tessdata was downloaded during build.
        if let Ok(home) = std::env::var("HOME") {
            candidates.push(PathBuf::from(home).join(".kreuzberg-tesseract/tessdata"));
        }

        candidates.into_iter().find(|path| path.exists())
    }

    // TESSDATA_PREFIX can either mean the tessdata directory itself or the parent prefix. This is
    // also used interchangebly in the documentation. Normalize both.
    fn as_tessdata_dir(path: PathBuf) -> PathBuf {
        if path.ends_with("tessdata") {
            path
        } else {
            path.join("tessdata")
        }
    }
}

impl OcrBackend for KreuzbergTesseractOcr {
    fn ocr_page(&self, pixels: &[u8], width: u16, height: u16) -> Result<OcrPage> {
        // Pass container's bytes directly using Leptonica's Pix wrapper
        // exposed by `kreuzberg-tesseract`.
        let rgb_pix = Pix::from_raw_rgb(pixels, width.into(), height.into())
            .context("Failed to create Leptonica Pix from RGB pixels")?;

        // Convert the container-rendered RGB page to grayscale before OCR.
        let mut pix = rgb_pix
            .to_grayscale()
            .context("Failed to convert OCR image to grayscale")?;

        let dpi = DPI as i32;

        // Store the container-rendered resolution on the Pix as image metadata
        // so Tesseract can interpret text size correctly.
        pix.set_resolution(dpi, dpi)
            .context("Failed to set OCR image resolution")?;

        // Give Tesseract the Leptonica image. `set_image_2` borrows the Pix
        // pointer; keep `pix` alive for the rest of this method.
        self.api
            .set_image_2(pix.as_ptr())
            .context("Failed to pass image to Tesseract")?;

        self.api
            .recognize()
            .context("Failed to run Tesseract recognition")?;

        let iterator = self
            .api
            .get_iterator()
            .context("Failed to get Tesseract result iterator")?;

        Ok(OcrPage::new(extract_ocr_words(&iterator)?))
    }
}

/// Extract OCR words and their properties.
///
/// Uses `kreuzberg-tesseract`'s high-level iterator helper instead of local
/// low-level Tesseract bindings.
fn extract_ocr_words(iterator: &ResultIterator) -> Result<Vec<OcrWord>> {
    let words = iterator
        .extract_all_words()
        .context("Failed to extract OCR words from Tesseract result iterator")?;

    Ok(words
        .into_iter()
        .filter_map(|word| {
            let text = word.text.trim().to_string();
            if text.is_empty() {
                return None;
            }

            let w = word.right - word.left;
            let h = word.bottom - word.top;
            if w <= 0 || h <= 0 {
                return None;
            }

            Some(OcrWord {
                text,
                vbox: OcrVBox {
                    x: word.left,
                    y: word.top,
                    w,
                    h,
                },
            })
        })
        .collect())
}
