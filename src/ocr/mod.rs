//! Components and logic to handle OCR.

use crate::PageData;

pub(crate) mod kreuzberg_tesseract;
pub(crate) use kreuzberg_tesseract::KreuzbergTesseractOcr;

/// DPI used by container.
pub const DEFAULT_DPI: i32 = 150;

/// Writing direction used to do OCR.
///
/// Used to decide the text matrix to calculate the coordinates
/// of objects in the PDF.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OcrWritingDirection {
    /// Left-to-right
    LTR,
    /// Right-to-left
    RTL,
}

/// Object holding coordinates and size data of OCR object.
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

/// Baseline reported by OCR in source image pixel.
///
/// Tesseract use baselines instead of only relying on word boxes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OcrVBaseline {
    /// Top-left X-coordinate
    pub x1: i32,
    /// Top-left Y-coordinate
    pub y1: i32,
    /// Bottom-right X-coordinate
    pub x2: i32,
    /// Bottom-right Y-coordinate
    pub y2: i32,
}

impl OcrVBaseline {
    /// Helper method to construct.
    pub fn new(x1: i32, y1: i32, x2: i32, y2: i32) -> Self {
        Self { x1, y1, x2, y2 }
    }
}

/// Object for each word on a page.
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
    /// Index of text-block this word belongs to
    ///
    /// Used to avoid mixing words from different blocks into one
    pub block_id: usize,
    /// Index of the line this word belongs to
    pub line_id: usize,
    /// Baseline of this word in source image pixel
    pub vbaseline: OcrVBaseline,
    /// Baseline of the wrapping line this word belongs to
    ///
    /// We duplicate/denormalize this data over the multiple words from a line
    /// to allow easier handling.
    pub line_vbaseline: OcrVBaseline,
    /// Reported font-size
    pub font_size: i32,
    /// Reported writing direction
    pub writing_direction: OcrWritingDirection,
    /// Flag determining if this word is the last in the line
    pub last_in_line: bool,
}

/// Object for each page in a document.
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
                    block_id: 0,
                    line_id: 0,
                    vbaseline: OcrVBaseline::new(x, y + h, x + w, y + h),
                    line_vbaseline: OcrVBaseline::new(x, y + h, x + w, y + h),
                    font_size: h.max(1),
                    writing_direction: OcrWritingDirection::LTR,
                    last_in_line: true,
                })
                .collect(),
        )
    }
}

/// Trait implemented by OCR backends.
///
/// This trait provides a generic contract for doing OCR on a page which
/// the different OCR backends will follow. This way we keep our OCR
/// implementation modular.
pub(crate) trait OcrBackend {
    /// Detect words on a single page.
    ///
    /// `pixels` must contain `width * height * 3` bytes in RGB order.
    fn ocr_page(&self, pixels: &[u8], width: u16, height: u16) -> OcrPage;
}

/// Run OCR for multiple pages with specified OCR-backend.
pub(crate) fn ocr_pages<B: OcrBackend>(pages: &[PageData], backend: &B) -> Vec<OcrPage> {
    pages
        .iter()
        .map(|page| backend.ocr_page(&page.pixels, page.width, page.height))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeOcrBackend;

    impl OcrBackend for FakeOcrBackend {
        fn ocr_page(&self, _pixels: &[u8], width: u16, height: u16) -> OcrPage {
            OcrPage::new(vec![OcrWord {
                text: format!("{width}x{height}"),
                vbox: OcrVBox {
                    x: 1,
                    y: 2,
                    w: 3,
                    h: 4,
                },
                block_id: 0,
                line_id: 0,
                vbaseline: OcrVBaseline::new(1, 6, 4, 6),
                line_vbaseline: OcrVBaseline::new(1, 6, 4, 6),
                font_size: 4,
                writing_direction: OcrWritingDirection::LTR,
                last_in_line: true,
            }])
        }
    }

    #[test]
    fn ocr_pages_runs_backend_for_each_page() {
        let pages = vec![
            PageData::new(10, 20, vec![255; 10 * 20 * 3]),
            PageData::new(30, 40, vec![255; 30 * 40 * 3]),
        ];

        let result = ocr_pages(&pages, &FakeOcrBackend);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].words[0].text, "10x20");
        assert_eq!(result[1].words[0].text, "30x40");
    }
}
