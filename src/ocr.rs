//! Components and logic to handle OCR

use std::path::PathBuf;

use anyhow::{Context, Result};
use crate::PageData;
use crate::GLYPHLESS_PDF_TTF;
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

/// Object for each line in the OCR PDF containing words
///
/// We use this to make the OCR placement line-aware instead
/// of word-by-word individually. This to make RTL behavior more consistent.
#[derive(Debug)]
pub(crate) struct OcrTextLine<'a> {
    /// Words in this line. We borrow these words and don't own them.
    /// We use a lifetime param to let Rust know this line is only valid as long as the referenced
    /// words are alive.
    words: Vec<&'a OcrWord>,
}

/// Group individual OCR words into text lines reported by the OCR backend.
fn merge_ocr_words_into_ocr_text_line(
    // This argument is a borrowed slice of `OcrWords`. Due to this borrowed slice we need a
    // specified lifetime for `OcrTextLine`.
    // The alternative to avoid lifetimes would be to make words a Vec copying the words, but this
    // would result in poor performance.
    words: &[OcrWord]
) -> Vec<OcrTextLine<'_>> {

    // Lines we will return as result.
    let mut lines : Vec<OcrTextLine<'_>> = Vec::new();
    // Current line we are processing.
    let mut curr_line : Option<OcrTextLine<'_>> = None;

    // Helper method returning if current word is in
    // the currently processed line.
    fn word_in_curr_line(line: &OcrTextLine<'_>, word: &OcrWord) -> bool {
        line.words.last().is_some_and(|last| {
            last.block_id == word.block_id && last.line_id == word.line_id
        })
    }

    // Loop over words.
    for word in words
        .iter()
        // Only use non-corrupt word boxes.
        .filter(|word| word.vbox.w > 0 && word.vbox.h > 0)
    {
        // Check state of current line.
        match &mut curr_line {
            // We are handling a line and the current word
            // is part of `curr_line`
            Some(line) if word_in_curr_line(line, word) => {
                // Just push word to current line since it's part of it.
                line.words.push(word);
            }
            // We are handling a line put should move to another visual line since current word is
            // not considered a part of `curr_line`.
            Some(line) => {
                // Sort words in line by x-coordinate.
                line.words.sort_by_key(|word| word.vbox.x);
                // Push current line to lines.
                // .take() takes ownership of curr_line and resets to None.
                lines.push(curr_line.take().expect("curr_line should exist"));
                // Move currently handled word to a next visual line.
                curr_line = Some(OcrTextLine { words: vec![word] });

            }
            // First line encountered: Initiate a new line with current word as first.
            None => {
                curr_line = Some(OcrTextLine { words: vec![word] });
            }
        }
    }

    // Flush latest remaining line into lines.
    if let Some(mut line) = curr_line {
        // Sort words in line by x-coordinate.
        line.words.sort_by_key(|word| word.vbox.x);
        lines.push(line);
    }

    lines
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

            // Set direction. We cache this since orientation is
            // not a property specific to one word alone, but all words
            // on the same line need the same orientation. We remember
            // the last orientation Tesseract reported.
            if let Some(direction) = orientation(raw) {
                curr_writing_direction = direction;
            } 

            // Set font size.
            let font_size = word_font_size(raw).unwrap_or(0);

            // Set flag determining if word is last in line.
            // This flag avoids setting trailing spaces which would be
            // required when there would be a next word. Since it's
            // the last word no trailing space is required.
            let last_in_line = unsafe {
                TessPageIteratorIsAtFinalElement(
                    raw,
                    TessPageIteratorLevel::RIL_TEXTLINE as c_int,
                    TessPageIteratorLevel::RIL_WORD as c_int,
                )
            } != 0;

            // Put extracted properties in `OcrWord` object and
            // push to result list.
            ocr_words.push(OcrWord {
                text,
                vbox,
                block_id,
                line_id,
                vbaseline: word_baseline,
                line_vbaseline: curr_line_baseline,
                font_size,
                writing_direction: curr_writing_direction,
                last_in_line,
            });

            // Exit looping over words if no new word is found on page.
            if unsafe {
                TessResultIteratorNext(raw, TessPageIteratorLevel::RIL_WORD as c_int)
            } == 0 {
                break;
            }
        }

        ocr_words
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

        OcrPage::new(KreuzbergTesseractOcr::extract_pdf_words(&iterator))
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

/// Get orientation metadata from tesseract
fn orientation(raw: *mut c_void) -> Option<OcrWritingDirection> {
    let mut orientation = 0;
    let mut _writing_direction = 0;
    let mut _textline_order = 0;
    let mut _deskew_angle = 0.0;
    unsafe {
        TessPageIteratorOrientation(
            raw,
            &mut orientation,
            &mut _writing_direction,
            &mut _textline_order,
            &mut _deskew_angle,
        )
    };
    
    Some(match orientation {
        1 => OcrWritingDirection::RTL,
        _ => OcrWritingDirection::LTR,
    })
}

/// Get pointsize from tesseract.
fn word_font_size(raw: *mut c_void) -> Option<i32> {
    let mut _is_bold = 0;
    let mut _is_italic = 0;
    let mut _is_underlined = 0;
    let mut _is_monospace = 0;
    let mut _is_serif = 0;
    let mut _is_smallcaps = 0;
    let mut pointsize = 0;
    let mut _font_id = 0;
    let ok = unsafe {
        TessResultIteratorWordFontAttributes(
            raw,
            &mut _is_bold,
            &mut _is_italic,
            &mut _is_underlined,
            &mut _is_monospace,
            &mut _is_serif,
            &mut _is_smallcaps,
            &mut pointsize,
            &mut _font_id,
        )
    };
    (ok != 0 && pointsize > 0).then_some(pointsize)
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

/// Embed glyphless OCR font objects in PDF
///
/// These objects are embedded once into the PDF and referenced
/// by `/OcrFont`.
pub(crate) fn embed_ocr_font(
    pdf_data: &mut Vec<u8>, // Raw PDF data
    object_offsets: &mut Vec<usize>, // Byte positions of each new object. Used to write the PDF xref table.
) -> Result<()> {
    // Object 3: Type0 font. Wrapper for character text to route
    // character codes to correct descendant font.
    object_offsets.push(pdf_data.len());
    // Start of object 3
    pdf_data.extend_from_slice(b"3 0 obj\n");
    // Start of PDF dictionary.
    pdf_data.extend_from_slice(b"<<\n");
    // GlyphlessFont as Basefont.
    pdf_data.extend_from_slice(b" /BaseFont /GlyphLessFont\n");
    // Reference to object 4 containing actual descendant font.
    pdf_data.extend_from_slice(b" /DescendantFonts [ 4 0 R ]\n"); 
    // Use identity mapping for character codes.
    pdf_data.extend_from_slice(b" /Encoding /Identity-H\n");
    // Declare font subtype. Type0 is the composite font containing multiple CID fonts. This way we
    // can support a wide range of Unicode characters.
    pdf_data.extend_from_slice(b" /Subtype /Type0\n");
    // Declare this object as a Font.
    pdf_data.extend_from_slice(b" /Type /Font\n");
    // End PDF dictionary.
    pdf_data.extend_from_slice(b">>\n");
    // End object.
    pdf_data.extend_from_slice(b"endobj\n");

    // Object 4: The actual CID font implementation.
    // A CID font is a Character Identifier which is actually an integer identifying the character.
    // Not necessarily the glyph or unicode itself.
    object_offsets.push(pdf_data.len());
    // Start of object 4.
    pdf_data.extend_from_slice(b"4 0 obj\n");
    // Start PDF dictionary
    pdf_data.extend_from_slice(b"<<\n");
    // Also the actual CID font needs to be identified as base font
    pdf_data.extend_from_slice(b" /BaseFont /GlyphLessFont\n");
    // Define CIDSystemInfo. This defines the CID character collection identity. A CID is just an
    // integer, we need the correct info about what system we use to convert these integers to the
    // actual characters.
    pdf_data.extend_from_slice(
        b" /CIDSystemInfo << /Ordering (Identity) /Registry (Adobe) /Supplement 0 >>\n",
    );
    // Per character advance widths.
    pdf_data.extend_from_slice(b" /DW 500 \n");
    // Link to object 5 containing font metadata.
    pdf_data.extend_from_slice(b" /FontDescriptor 5 0 R\n");
    // Subtype saying this is a CID font using TrueType outlines.
    pdf_data.extend_from_slice(b" /Subtype /CIDFontType2\n");
    // Declare object 4 as font.
    pdf_data.extend_from_slice(b" /Type /Font\n");
    // End dictionary.
    pdf_data.extend_from_slice(b">>\n");
    // End object.
    pdf_data.extend_from_slice(b"endobj\n");

    // Object 5: Font metadata and descriptors like width, metrics, characteristics,...
    // Without this data selection rectangles, cursor geometry,... wouldn't be possible.
    object_offsets.push(pdf_data.len());
    // Start object 5.
    pdf_data.extend_from_slice(b"5 0 obj\n");
    // Start PFD dictionary.
    pdf_data.extend_from_slice(b"<<\n");
    // Height above baseline. 1000 is 1em and basic config.
    pdf_data.extend_from_slice(b" /Ascent 1000\n");
    // Height of an uppercase char.
    pdf_data.extend_from_slice(b" /CapHeight 1000\n");
    // Bitfield 1 to only support basic monospace for our PoC.
    // TODO: Also support symbolic later.
    pdf_data.extend_from_slice(b" /Flags 1\n");
    // Bounding box for glyph matching DW 500 and Ascent 1000.
    pdf_data.extend_from_slice(b" /FontBBox [ 0 0 500 1000 ]\n");
    // Reference object 6 containing embedded TrueFont font.
    pdf_data.extend_from_slice(b" /FontFile2 6 0 R\n");
    // Internal font name matching Type0 and CID.
    pdf_data.extend_from_slice(b" /FontName /GlyphLessFont\n");
    // Declare this object as Font descriptir.
    pdf_data.extend_from_slice(b" /Type /FontDescriptor\n");
    // End dictionary.
    pdf_data.extend_from_slice(b">>\n");
    // End object.
    pdf_data.extend_from_slice(b"endobj\n");

    // Object 6: Embed our included Tesseract TrueType font file/program.
    object_offsets.push(pdf_data.len());
    pdf_data.extend_from_slice(b"6 0 obj\n");
    pdf_data.extend_from_slice(b"<<\n");
    pdf_data.extend_from_slice(format!(" /Length {}\n", GLYPHLESS_PDF_TTF.len()).as_bytes());
    pdf_data.extend_from_slice(format!(" /Length1 {}\n", GLYPHLESS_PDF_TTF.len()).as_bytes());
    pdf_data.extend_from_slice(b">>\n");
    pdf_data.extend_from_slice(b"stream\n");
    pdf_data.extend_from_slice(GLYPHLESS_PDF_TTF);
    pdf_data.extend_from_slice(b"\nendstream\n");
    pdf_data.extend_from_slice(b"endobj\n");

    Ok(())
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
