//! Components and logic to handle OCR

use anyhow::Result;

use crate::PageData;

pub(crate) mod kreuzberg_tesseract;
pub(crate) mod pdf_renderer;
pub(crate) use self::kreuzberg_tesseract::KreuzbergTesseractOcr;

/// Object holding coordinates and size data of OCR object
#[derive(Clone, Copy, Debug)]
pub(crate) struct OcrVBox {
    /// X-coordinate
    pub x: i32,
    /// Y-coordinate
    pub y: i32,
    /// Width
    pub w: i32,
    /// Height
    pub h: i32,
}

/// Object for each word on a page
///
/// We use word-level granularity for OCR.
/// The fields in this struct are richer then storing only
/// the text and coordinates + sizing properties since that isn't
/// sufficient to do precise OCR.
#[derive(Debug)]
pub(crate) struct OcrWord {
    /// Text recognized by the OCR
    pub text: String,
    /// Coordinates + sizing properties
    pub vbox: OcrVBox,
}

/// Object for each page in a document
///
/// An `OcrPage` contains it's `OcrWord`'s. Together they
/// form the whole document.
pub(crate) struct OcrPage {
    /// OCR word-boxes present on this page
    words: Vec<OcrWord>,
}

impl OcrPage {
    fn new(words: Vec<OcrWord>) -> Self {
        Self { words }
    }

    pub(crate) fn words(&self) -> &[OcrWord] {
        &self.words
    }

    #[cfg(test)]
    pub(crate) fn from_test_words(words: Vec<(&str, i32, i32, i32, i32)>) -> Self {
        Self::new(
            words
                .into_iter()
                .map(|(text, x, y, w, h)| OcrWord {
                    text: text.to_string(),
                    vbox: OcrVBox { x, y, w, h },
                })
                .collect(),
        )
    }
}

/// Trait implemented by OCR backends
///
/// This trait provides a generic contract for doing OCR on a page which
/// the different OCR backends will follow. This way we keep our OCR
/// implementation modular.
pub(crate) trait OcrBackend {
    /// Detect words on a single page
    ///
    /// `pixels` must contain `width * height * 3` bytes in RGB order.
    fn ocr_page(&self, pixels: &[u8], width: u16, height: u16) -> Result<OcrPage>;

    /// Run OCR for multiple pages.
    fn ocr_pages(&self, pages: &[PageData]) -> Result<Vec<OcrPage>> {
        pages
            .iter()
            .map(|page| self.ocr_page(&page.pixels, page.width, page.height))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeOcrBackend;

    impl OcrBackend for FakeOcrBackend {
        fn ocr_page(&self, _pixels: &[u8], width: u16, height: u16) -> Result<OcrPage> {
            Ok(OcrPage::new(vec![OcrWord {
                text: format!("{width}x{height}"),
                vbox: OcrVBox {
                    x: 1,
                    y: 2,
                    w: 3,
                    h: 4,
                },
            }]))
        }
    }

    #[test]
    fn ocr_pages_runs_backend_for_each_page() -> Result<()> {
        let pages = vec![
            PageData::new(10, 20, vec![255; 10 * 20 * 3]),
            PageData::new(30, 40, vec![255; 30 * 40 * 3]),
        ];

        let result = FakeOcrBackend.ocr_pages(&pages)?;

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].words[0].text, "10x20");
        assert_eq!(result[1].words[0].text, "30x40");
        Ok(())
    }
}
