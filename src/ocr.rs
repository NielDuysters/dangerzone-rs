//! Components and logic to handle OCR

use std::path::PathBuf;

use crate::PageData;
use kreuzberg_tesseract::{Pix, TesseractAPI, TessPageIteratorLevel};
use std::os::raw::{c_char, c_int, c_void};
use std::ffi::CStr;

/// DPI used by container
pub const DEFAULT_DPI: i32 = 150;

/// Writing direction used to do OCR
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

/// Baseline reported by OCR in source image pixel
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
    /// Helper method to construct
    pub fn new(x1: i32, y1: i32, x2: i32, y2: i32) -> Self {
        Self {
            x1, y1, x2, y2
        }
    }
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
                    x,
                    y,
                    w,
                    h,
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
    fn ocr_page(&self, pixels: &[u8], width: u16, height: u16) -> OcrPage;
}

/// Run OCR for multiple pages with specified OCR-backend
pub(crate) fn ocr_pages<B: OcrBackend>(pages: &[PageData], backend: &B) -> Vec<OcrPage> {
    pages
        .iter()
        .map(|page| backend.ocr_page(&page.pixels, page.width, page.height))
        .collect()
}

/// OCR backend powered by the `kreuzberg-tesseract` used for Linux
pub(crate) struct KreuzbergTesseractOcr;

impl KreuzbergTesseractOcr {
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
    
    /// Extract PDF words and their properties.
    ///
    /// Required to construct OcrWord's. We use Tesseract's low-level iterator since it provides
    /// more details.
    pub(crate) fn extract_pdf_words(iterator: &kreuzberg_tesseract::ResultIterator) -> Vec<OcrWord> {

        // Get raw handle
        let Ok(handle) = iterator.handle.lock() else {
            return Vec::new();
        };
        let raw = *handle;

        // Vector containing results we will return
        let mut ocr_words : Vec<OcrWord> = Vec::new();

        // Helper properties used when looping over iterator
        let mut block_id: usize = 0;
        let mut line_id: usize = 0;
        let mut curr_line_baseline = OcrVBaseline::new(0,0,0,0);
        let mut curr_writing_direction = OcrWritingDirection::LTR;

        // Reset iterator to first word on page
        unsafe { TessPageIteratorBegin(raw) };

        // Loop over words on page
        loop {
            // Tesseract has moved to a new visual element
            //
            // Update block_id to prevent the PDF writer to join/mix
            // text that should remain separated.
            if unsafe {
                TessPageIteratorIsAtBeginningOf(raw, TessPageIteratorLevel::RIL_BLOCK as c_int)
            } != 0 {
                block_id += 1;
            }

            // Store text-level baseline when the iterator goes into next line.
            // A line-level baseline is used as reference for rotated/skewed text.
            if unsafe {
                TessPageIteratorIsAtBeginningOf(raw, TessPageIteratorLevel::RIL_TEXTLINE as c_int)
            } != 0 {
                line_id += 1;
                curr_line_baseline = baseline(raw, TessPageIteratorLevel::RIL_TEXTLINE)
                .unwrap_or_else(|| fallback_baseline(raw, TessPageIteratorLevel::RIL_TEXTLINE));
            }

            // Extract text with word-level granularity.
            let Some(text) = utf8_text(raw, TessPageIteratorLevel::RIL_WORD) else {
                // Manually move iterator to next word
                if unsafe {
                    TessResultIteratorNext(raw, TessPageIteratorLevel::RIL_WORD as c_int)
                } == 0 {
                    // No next word found on page. Break loop.
                    break;
                }
                // No text found for current word. But continue scanning next words.
                continue;
            };
            
            // Trim text, and if it's empty try continuing to
            // next word of end loop if no next is found.
            let text = text.trim().to_string();
            if text.is_empty() {
                if unsafe {
                    TessResultIteratorNext(raw, TessPageIteratorLevel::RIL_WORD as c_int)
                } == 0 {
                    break;
                }

                continue;
            }

            // Check if word has a bounding_box. Ignore if it doesn't to avoid poisoning whole OCR
            // result.
            let Some(vbox) = bounding_box(raw, TessPageIteratorLevel::RIL_WORD) else {
                if unsafe {
                    TessResultIteratorNext(raw, TessPageIteratorLevel::RIL_WORD as c_int)
                } == 0 {
                    break;
                }
                
                continue;
            };

            // Set word_baseline. Fall back to horizontal line at
            // bottom of word-box is missing.
            let word_baseline = baseline(raw, TessPageIteratorLevel::RIL_WORD)
                .unwrap_or_else(|| OcrVBaseline::new(vbox.x, vbox.y + vbox.h, vbox.x + vbox.w, vbox.y + vbox.h));
        }

        unimplemented!()
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

        // Initialize tesseract engine for this page to do OCR.
        // TODO: Find a way to re-use same instance for all pages.
        let api = match TesseractAPI::new() {
            Ok(api) => api,
            Err(_) => return OcrPage::new(Vec::new()),
        };

        // Seed tesseract with trained language data.
        // TODO: Currently we only support English. Support other languages to.
        // TODO: Check if we can seed the trained data for the whole PDF instead of per-page.
        let tessdata_dir = match Self::tessdata_dir() {
            Some(path) => path,
            None => return OcrPage::new(Vec::new()),
        };
        if api.init(&tessdata_dir, "eng").is_err() {
            return OcrPage::new(Vec::new());
        }

        // Give Tesseract the Leptonica image. `set_image_2` borrows the Pix
        // pointer; keep `pix` alive for the rest of this method.
        if api.set_image_2(pix.as_ptr()).is_err() {
            return OcrPage::new(Vec::new());
        }

        // Also set the source resolution on the Tesseract API. Some OCR paths
        // read DPI from the engine state rather than from the Pix metadata.
        let _ = api.set_source_resolution(DEFAULT_DPI);

        if api.recognize().is_err() {
            return OcrPage::new(Vec::new());
        }

        let iterator = match api.get_iterator() {
            Ok(iterator) => iterator,
            Err(_) => return OcrPage::new(Vec::new()),
        };

        let words = match iterator.extract_all_words() {
            Ok(words) => words,
            Err(_) => return OcrPage::new(Vec::new()),
        };

        OcrPage::new(
            words
                .into_iter()
                .filter_map(|word| {
                    let text = word.text.trim().to_string();
                    if text.is_empty() {
                        None
                    } else {
                        Some(OcrWord {
                            text,
                            x: word.left,
                            y: word.top,
                            w: word.right - word.left,
                            h: word.bottom - word.top,
                        })
                    }
                })
                .collect(),
        )
    }

}


// TODO: We will revisit our project structure to put
// the following code in a separate module.

// Helper methods for tesseract.

/// Bounding boxes come back as top, left, right, bottom.
/// We convert it to our OcrVBox object.
fn bounding_box(raw: *mut c_void, level: TessPageIteratorLevel) -> Option<OcrVBox> {
    let mut left = 0;
    let mut top = 0;
    let mut right = 0;
    let mut bottom = 0;
    let ok = unsafe {
        TessPageIteratorBoundingBox(
            raw,
            level as c_int,
            &mut left,
            &mut top,
            &mut right,
            &mut bottom,
        )
    };
    (ok != 0).then_some(OcrVBox {
        x: left,
        y: top,
        w: right - left,
        h: bottom - top,
    })
}

/// Baselines are returned as two points in image pixels. They may be angled
/// if Tesseract detected skew or rotated text.
fn baseline(raw: *mut c_void, level: TessPageIteratorLevel) -> Option<OcrVBaseline> {
    let mut x1 = 0;
    let mut y1 = 0;
    let mut x2 = 0;
    let mut y2 = 0;
    let ok = unsafe {
        TessPageIteratorBaseline(raw, level as c_int, &mut x1, &mut y1, &mut x2, &mut y2)
    };
    (ok != 0).then_some(OcrVBaseline::new(x1, y1, x2, y2))
}

/// When Tesseract cannot provide a baseline, use the bottom edge of the
/// bounding box.
fn fallback_baseline(raw: *mut c_void, level: TessPageIteratorLevel) -> OcrVBaseline {
    bounding_box(raw, level)
        .map(| vbox| OcrVBaseline::new(vbox.x, vbox.y + vbox.h, vbox.x + vbox.w, vbox.y + vbox.h))
        .unwrap_or_else(|| OcrVBaseline::new(0, 0, 0, 0))
}

/// Get text returned by tesseract.
fn utf8_text(raw: *mut c_void, level: TessPageIteratorLevel) -> Option<String> {
    // Retrieve text
    let text_ptr = unsafe { TessResultIteratorGetUTF8Text(raw, level as c_int) };
    if text_ptr.is_null() {
        return None;
    }
    // Transfer ownership to caller.
    let text = unsafe { CStr::from_ptr(text_ptr) }
        .to_str()
        .ok()
        .map(str::to_string);

    // Free pointer.
    unsafe { TessDeleteText(text_ptr) };
    text
}

// Raw Tesseract C API calls that are not currently surfaced by
// `kreuzberg-tesseract`'s safe Rust API.
unsafe extern "C-unwind" {
    fn TessDeleteText(text: *mut c_char);
    fn TessPageIteratorBegin(handle: *mut c_void);
    fn TessPageIteratorIsAtBeginningOf(handle: *mut c_void, level: c_int) -> c_int;
    fn TessPageIteratorIsAtFinalElement(handle: *mut c_void, level: c_int, element: c_int)
        -> c_int;
    fn TessPageIteratorBoundingBox(
        handle: *mut c_void,
        level: c_int,
        left: *mut c_int,
        top: *mut c_int,
        right: *mut c_int,
        bottom: *mut c_int,
    ) -> c_int;
    fn TessPageIteratorBaseline(
        handle: *mut c_void,
        level: c_int,
        x1: *mut c_int,
        y1: *mut c_int,
        x2: *mut c_int,
        y2: *mut c_int,
    ) -> c_int;
    fn TessPageIteratorOrientation(
        handle: *mut c_void,
        orientation: *mut c_int,
        writing_direction: *mut c_int,
        textline_order: *mut c_int,
        deskew_angle: *mut f32,
    );
    fn TessResultIteratorGetUTF8Text(handle: *mut c_void, level: c_int) -> *mut c_char;
    fn TessResultIteratorNext(handle: *mut c_void, level: c_int) -> c_int;
    fn TessResultIteratorWordFontAttributes(
        handle: *mut c_void,
        is_bold: *mut c_int,
        is_italic: *mut c_int,
        is_underlined: *mut c_int,
        is_monospace: *mut c_int,
        is_serif: *mut c_int,
        is_smallcaps: *mut c_int,
        pointsize: *mut c_int,
        font_id: *mut c_int,
    ) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::*;


    struct FakeOcrBackend;

    impl OcrBackend for FakeOcrBackend {
        fn ocr_page(&self, _pixels: &[u8], width: u16, height: u16) -> OcrPage {
            OcrPage::new(vec![OcrWord {
                text: format!("{width}x{height}"),
                x: 1,
                y: 2,
                w: 3,
                h: 4,
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
