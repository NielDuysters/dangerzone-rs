//! Components and logic to handle OCR

use std::path::PathBuf;

use crate::PageData;
use kreuzberg_tesseract::{Pix, TessPageIteratorLevel, TesseractAPI};
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};

/// DPI used by container
pub const DEFAULT_DPI: i32 = 150;

/// Writing direction detected by the OCR backend.
///
/// This intentionally mirrors the writing directions used by Tesseract's
/// `ResultIterator`. The PDF writer uses it later to choose the text matrix:
/// left-to-right text can use the normal PDF coordinate system, while
/// right-to-left text needs a horizontally reflected matrix. We keep this enum
/// in our own OCR module instead of exposing Tesseract's enum directly so that
/// future backends, such as Apple Vision or Windows OCR, can map their own
/// direction concepts into the same internal representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OcrWritingDirection {
    LeftToRight,
    RightToLeft,
    TopToBottom,
}

impl OcrWritingDirection {
    /// Convert Tesseract's C API integer into our backend-neutral enum.
    ///
    /// Tesseract currently uses:
    /// - `0` for left-to-right
    /// - `1` for right-to-left
    /// - `2` for top-to-bottom
    ///
    /// Unknown values are treated as left-to-right because that is the safest
    /// fallback for the English-only PoC and matches the common case for our
    /// current sample documents.
    fn from_tesseract(value: c_int) -> Self {
        match value {
            1 => Self::RightToLeft,
            2 => Self::TopToBottom,
            _ => Self::LeftToRight,
        }
    }
}

/// Baseline reported by OCR in source-image pixel coordinates.
///
/// Tesseract's PDF renderer positions text on baselines rather than simply at
/// the bottom of word bounding boxes. That matters for text selection: PDF
/// viewers derive selectable text geometry from the text matrix and font
/// metrics, so using baseline placement gives more stable selection than
/// placing every word at an arbitrary rectangle corner.
///
/// Coordinates here are still in the image coordinate space returned by
/// Tesseract: origin at the top-left, units in source pixels. The PDF writer
/// later converts them to PDF points and flips the Y axis.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OcrBaseline {
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
}

impl OcrBaseline {
    pub(crate) fn new(x1: i32, y1: i32, x2: i32, y2: i32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    pub(crate) fn x1(&self) -> i32 {
        self.x1
    }

    pub(crate) fn y1(&self) -> i32 {
        self.y1
    }

    pub(crate) fn x2(&self) -> i32 {
        self.x2
    }

    pub(crate) fn y2(&self) -> i32 {
        self.y2
    }
}

/// Object for each OCR text run on a page.
///
/// For this PoC we keep OCR at word-level granularity. That matches
/// Tesseract's PDF renderer closely enough for English text while avoiding the
/// instability we saw when every character was emitted independently. The
/// fields are intentionally richer than `{ text, bbox }` because the PDF text
/// layer needs more than a rectangle to behave well:
///
/// - `block_id` / `line_id` preserve Tesseract's reading order.
/// - `baseline` and `line_baseline` let the writer place text the same way
///   Tesseract's PDF renderer does.
/// - `font_size` lets us avoid estimating text size from a display font.
/// - `writing_direction` gives the writer enough information to build the
///   correct PDF text matrix.
///
/// This type is still crate-private. External API shape can be revisited once
/// OCR support is no longer a PoC.
#[derive(Debug)]
pub(crate) struct OcrText {
    /// Text content recognized by the OCR backend for this word.
    text: String,
    /// Left edge of the OCR word box, in source-image pixels from the left.
    x: i32,
    /// Top edge of the OCR word box, in source-image pixels from the top.
    y: i32,
    /// Width of the OCR word box, in source-image pixels.
    w: i32,
    /// Height of the OCR word box, in source-image pixels.
    h: i32,
    /// Index of the text block this word belongs to.
    ///
    /// The value is generated while walking Tesseract's iterator. It is not
    /// exposed as a document-stable identifier; it is only used by the PDF
    /// writer to avoid mixing words from different blocks into one `BT` block.
    block_id: usize,
    /// Index of the text line this word belongs to.
    ///
    /// Tesseract already knows which words belong to the same text line. Using
    /// that information is more reliable than regrouping by Y coordinate in
    /// Rust, especially for slightly skewed scans.
    line_id: usize,
    /// Baseline for this word in source-image pixels.
    baseline: OcrBaseline,
    /// Baseline for the containing line in source-image pixels.
    line_baseline: OcrBaseline,
    /// Font size reported by Tesseract, in points when available.
    ///
    /// Tesseract can return zero for some scripts or low-confidence words. The
    /// PDF writer treats zero as "unknown" and falls back to a size derived
    /// from the OCR box height.
    font_size: i32,
    /// Writing direction reported by Tesseract.
    writing_direction: OcrWritingDirection,
    /// Whether this word is the final word in its OCR text line.
    ///
    /// This controls whether the PDF writer appends a synthetic trailing space
    /// after the word. Tesseract's PDF renderer appends a space when advancing
    /// to another word, but not at the end of a text line.
    final_in_line: bool,
}

impl OcrText {
    /// Return the recognized text for this OCR item.
    ///
    /// The string is kept exactly as the backend returned it after our
    /// extraction step has trimmed surrounding whitespace. The PDF writer adds
    /// explicit spaces between words when needed, so callers should not assume
    /// this already contains the visual spacing from the source page.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Return the left edge of the OCR word box in source-image pixels.
    pub(crate) fn x(&self) -> i32 {
        self.x
    }

    /// Return the top edge of the OCR word box in source-image pixels.
    ///
    /// This is not a PDF Y coordinate. Tesseract and the container image use a
    /// top-left origin, while PDF content streams use a bottom-left origin.
    /// That conversion happens later in `write_pdf`.
    pub(crate) fn y(&self) -> i32 {
        self.y
    }

    /// Return the OCR word-box width in source-image pixels.
    pub(crate) fn w(&self) -> i32 {
        self.w
    }

    /// Return the OCR word-box height in source-image pixels.
    pub(crate) fn h(&self) -> i32 {
        self.h
    }

    /// Return the Tesseract block index assigned during iterator traversal.
    ///
    /// The index is only meaningful relative to other `OcrText` items from the
    /// same page. It lets the PDF writer preserve Tesseract's block grouping
    /// without exposing Tesseract's raw iterator objects outside this module.
    pub(crate) fn block_id(&self) -> usize {
        self.block_id
    }

    /// Return the Tesseract text-line index assigned during traversal.
    ///
    /// The PDF writer groups words by `(block_id, line_id)`. That is more
    /// stable than guessing line membership from bounding boxes after the fact,
    /// especially when the image contains skewed or slightly rotated text.
    pub(crate) fn line_id(&self) -> usize {
        self.line_id
    }

    /// Return the baseline for this individual word in source-image pixels.
    ///
    /// Tesseract sometimes reports a word baseline that starts closer to the
    /// actual text than the containing line's baseline. We use it to calculate
    /// each word's position along the line baseline.
    pub(crate) fn baseline(&self) -> OcrBaseline {
        self.baseline
    }

    /// Return the baseline for the containing OCR text line.
    ///
    /// This is the reference line used to build the PDF text matrix. Keeping
    /// the whole line baseline allows the generated hidden text to follow
    /// skewed text instead of forcing every word onto a flat horizontal line.
    pub(crate) fn line_baseline(&self) -> OcrBaseline {
        self.line_baseline
    }

    /// Return the font size reported by Tesseract.
    ///
    /// This value is used as the PDF font size for invisible OCR text. The
    /// glyphless font is then horizontally scaled with `Tz`, so the exact
    /// display font does not need to match the scanned document.
    pub(crate) fn font_size(&self) -> i32 {
        self.font_size
    }

    /// Return the writing direction reported by Tesseract.
    ///
    /// Direction is part of the OCR result instead of a PDF-writer heuristic so
    /// future OCR backends can provide the same semantic value without forcing
    /// the writer to know which engine produced the text.
    pub(crate) fn writing_direction(&self) -> OcrWritingDirection {
        self.writing_direction
    }

    /// Return whether Tesseract says this is the final word in the line.
    ///
    /// The hidden text stream appends a space after most words so copy/paste
    /// produces natural text. We skip that synthetic space for the final word
    /// in a line to avoid copying extra trailing characters.
    pub(crate) fn is_final_in_line(&self) -> bool {
        self.final_in_line
    }
}

/// Object for each page in a document
///
/// An `OcrPage` contains its `OcrText` items. Together they
/// form the whole document.
pub(crate) struct OcrPage {
    /// OCR text items present on this page, in Tesseract iterator order.
    words: Vec<OcrText>,
}

impl OcrPage {
    fn new(words: Vec<OcrText>) -> Self {
        Self { words }
    }

    pub(crate) fn words(&self) -> &[OcrText] {
        &self.words
    }

    #[cfg(test)]
    pub(crate) fn from_test_words(words: Vec<(&str, i32, i32, i32, i32)>) -> Self {
        let word_count = words.len();
        Self::new(
            words
                .into_iter()
                .enumerate()
                .map(|(idx, (text, x, y, w, h))| {
                    let baseline = OcrBaseline::new(x, y + h, x + w, y + h);
                    OcrText {
                        text: text.to_string(),
                        x,
                        y,
                        w,
                        h,
                        block_id: 0,
                        line_id: 0,
                        baseline,
                        line_baseline: baseline,
                        font_size: h.max(1),
                        writing_direction: OcrWritingDirection::LeftToRight,
                        final_in_line: idx + 1 == word_count,
                    }
                })
                .collect(),
        )
    }
}

/// Trait implemented by OCR backends.
///
/// This is the small backend boundary for the integrated OCR path. The PDF
/// writer does not know whether text came from Tesseract, Apple Vision, or a
/// future Windows backend; it only consumes `OcrPage` / `OcrText` data. That is
/// why Tesseract-specific information is normalized into our own structs before
/// leaving `src/ocr.rs`.
pub(crate) trait OcrBackend {
    /// Detect text on a single rendered page.
    ///
    /// `pixels` must contain `width * height * 3` bytes in RGB order. The
    /// container gives us one page at a time in exactly that format, so the OCR
    /// backend can run before the final PDF is assembled and without writing an
    /// intermediate PDF to disk.
    fn ocr_page(&self, pixels: &[u8], width: u16, height: u16) -> OcrPage;
}

/// Run OCR for every page with the selected OCR backend.
///
/// This deliberately accepts the backend as an argument. It keeps the call site
/// explicit for the Linux PoC and leaves a clean place to choose another
/// backend later with conditional compilation.
pub(crate) fn ocr_pages<B: OcrBackend>(pages: &[PageData], backend: &B) -> Vec<OcrPage> {
    pages
        .iter()
        .map(|page| backend.ocr_page(&page.pixels, page.width, page.height))
        .collect()
}

/// Linux OCR backend powered by `kreuzberg-tesseract`.
///
/// This backend owns all Tesseract-specific setup and raw iterator extraction.
/// The rest of the crate should not need to know about Leptonica `Pix`
/// pointers, tessdata lookup, or Tesseract C API functions.
pub(crate) struct KreuzbergTesseractOcr;

impl KreuzbergTesseractOcr {
    /// Resolve the tessdata directory used to initialize Tesseract.
    ///
    /// `TESSDATA_PREFIX` has priority when set. Otherwise we use the tessdata
    /// bundled by `kreuzberg-tesseract`.
    ///
    /// The extra system paths are Debian/Ubuntu-friendly fallbacks. Tesseract
    /// packages commonly install language data under
    /// `/usr/share/tesseract-ocr/5/tessdata`, while some distributions or older
    /// installations use `/usr/share/tesseract-ocr/tessdata`.
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
        // Convert the container's raw RGB page buffer into a Leptonica `Pix`.
        //
        // Tesseract works with Leptonica images internally. Using
        // `Pix::from_raw_rgb` avoids the earlier BMP round-trip: we do not need
        // to encode a temporary image format just so Tesseract can decode it
        // again. The `Pix` wrapper also owns the Leptonica allocation, so Rust
        // will clean it up when this method returns.
        let mut pix = match Pix::from_raw_rgb(pixels, width.into(), height.into()) {
            Ok(pix) => pix,
            Err(_) => return OcrPage::new(Vec::new()),
        };

        // The Dangerzone container renders pages at 150 DPI. Store that
        // resolution on the image metadata so Tesseract can interpret text size
        // and baselines in the same scale the PDF writer will later use.
        let _ = pix.set_resolution(DEFAULT_DPI, DEFAULT_DPI);

        // Initialize a Tesseract engine for this page.
        //
        // This is intentionally simple for the PoC. Reusing one engine across
        // pages may be faster, but the current backend boundary is easier to
        // reason about: each `ocr_page` call owns its image, recognition state,
        // and iterator lifetime.
        // TODO: Find a way to re-use same instance for all pages.
        let api = match TesseractAPI::new() {
            Ok(api) => api,
            Err(_) => return OcrPage::new(Vec::new()),
        };

        // Seed Tesseract with trained language data.
        //
        // The PoC uses English because the current Linux integration is being
        // developed against the sample English documents. Additional languages
        // need more careful handling because text extraction, glyph mapping,
        // and writing direction have visible copy/paste consequences.
        // TODO: Currently we only support English. Support other languages too.
        // TODO: Check if we can seed the trained data for the whole PDF instead of per-page.
        let tessdata_dir = match Self::tessdata_dir() {
            Some(path) => path,
            None => return OcrPage::new(Vec::new()),
        };
        if api.init(&tessdata_dir, "eng").is_err() {
            return OcrPage::new(Vec::new());
        }

        // Give Tesseract the Leptonica image.
        //
        // `set_image_2` borrows the raw Pix pointer; it does not take ownership
        // of the Rust wrapper. Keep `pix` alive until recognition and iterator
        // extraction are complete.
        if api.set_image_2(pix.as_ptr()).is_err() {
            return OcrPage::new(Vec::new());
        }

        // Also set the source resolution on the Tesseract API. Some OCR paths
        // read DPI from the engine state rather than from the Pix metadata.
        let _ = api.set_source_resolution(DEFAULT_DPI);

        // Run recognition before requesting the iterator. The iterator is a
        // view into Tesseract's recognized page structure; without this call
        // there may be no word, line, baseline, or font metadata to extract.
        if api.recognize().is_err() {
            return OcrPage::new(Vec::new());
        }

        let iterator = match api.get_iterator() {
            Ok(iterator) => iterator,
            Err(_) => return OcrPage::new(Vec::new()),
        };

        OcrPage::new(extract_pdf_text_items(&iterator))
    }
}

fn extract_pdf_text_items(iterator: &kreuzberg_tesseract::ResultIterator) -> Vec<OcrText> {
    // `kreuzberg-tesseract` exposes useful high-level helpers for simple word
    // extraction, but Tesseract's own PDF renderer uses lower-level iterator
    // data: line starts, block starts, baselines, writing direction, font size,
    // and final-word markers. We need those pieces to generate a hidden text
    // layer that behaves like Tesseract's `pdfrenderer.cpp`, so this function
    // reads the raw C iterator through FFI and immediately copies the data into
    // safe Rust structs.
    let Ok(handle) = iterator.handle.lock() else {
        return Vec::new();
    };
    let raw = *handle;
    let mut texts = Vec::new();
    let mut block_id: usize = 0;
    let mut line_id: usize = 0;
    let mut current_line_baseline = OcrBaseline::new(0, 0, 0, 0);
    let mut current_writing_direction = OcrWritingDirection::LeftToRight;

    // Reset the page iterator to the first result and then advance by words.
    // At each word we still query the surrounding block and text-line state.
    // This is the same hierarchy Tesseract's PDF renderer walks, and it gives
    // the PDF writer enough structure to preserve reading order and line-level
    // positioning.
    unsafe { TessPageIteratorBegin(raw) };
    loop {
        // A new block means Tesseract has moved to another visual/semantic
        // grouping. Keeping the block id prevents the PDF writer from joining
        // text that Tesseract considered separate.
        if unsafe {
            TessPageIteratorIsAtBeginningOf(raw, TessPageIteratorLevel::RIL_BLOCK as c_int)
        } != 0
        {
            block_id += 1;
        }

        // Cache the text-line baseline when the iterator enters a new line.
        // Individual words may have their own baselines, but the line baseline
        // is the stable reference for rotation/skew and relative word movement
        // inside the PDF `BT`/`ET` text object.
        if unsafe {
            TessPageIteratorIsAtBeginningOf(raw, TessPageIteratorLevel::RIL_TEXTLINE as c_int)
        } != 0
        {
            line_id += 1;
            current_line_baseline = baseline(raw, TessPageIteratorLevel::RIL_TEXTLINE)
                .unwrap_or_else(|| fallback_baseline(raw, TessPageIteratorLevel::RIL_TEXTLINE));
        }

        // Extract text at word granularity. Returning `None` here usually means
        // Tesseract has no text for this iterator position, so we skip it and
        // continue advancing instead of failing the whole OCR page.
        let Some(text) = utf8_text(raw, TessPageIteratorLevel::RIL_WORD) else {
            if unsafe { TessResultIteratorNext(raw, TessPageIteratorLevel::RIL_WORD as c_int) } == 0
            {
                break;
            }
            continue;
        };
        let text = text.trim().to_string();
        if text.is_empty() {
            if unsafe { TessResultIteratorNext(raw, TessPageIteratorLevel::RIL_WORD as c_int) } == 0
            {
                break;
            }
            continue;
        }

        // A word without a bounding box cannot be positioned in the PDF text
        // layer, so it is ignored. This keeps bad OCR metadata local to one
        // word instead of poisoning the whole page.
        let Some((left, top, right, bottom)) = bounding_box(raw, TessPageIteratorLevel::RIL_WORD)
        else {
            if unsafe { TessResultIteratorNext(raw, TessPageIteratorLevel::RIL_WORD as c_int) } == 0
            {
                break;
            }
            continue;
        };

        // Prefer Tesseract's word baseline. If it is missing, fall back to a
        // horizontal line at the bottom of the word box; that is imperfect but
        // still gives the writer a sane position and width.
        let word_baseline = baseline(raw, TessPageIteratorLevel::RIL_WORD)
            .unwrap_or_else(|| OcrBaseline::new(left, bottom, right, bottom));
        // Orientation is queried from the iterator because writing direction is
        // not a property of the UTF-8 text alone. The current value is cached so
        // words on the same line keep the most recent direction Tesseract
        // reported.
        if let Some(direction) = orientation(raw) {
            current_writing_direction = direction;
        }
        // Tesseract can estimate the word font size from recognition results.
        // The PDF layer uses this with a glyphless font and horizontal scaling,
        // avoiding our earlier fragile Helvetica-width estimation.
        let font_size = word_font_size(raw).unwrap_or(0);
        // This mirrors Tesseract's renderer behavior for synthetic spaces:
        // words get a trailing space only when another word follows on the same
        // OCR text line.
        let final_in_line = unsafe {
            TessPageIteratorIsAtFinalElement(
                raw,
                TessPageIteratorLevel::RIL_TEXTLINE as c_int,
                TessPageIteratorLevel::RIL_WORD as c_int,
            )
        } != 0;

        texts.push(OcrText {
            text,
            x: left,
            y: top,
            w: right - left,
            h: bottom - top,
            block_id: block_id.saturating_sub(1),
            line_id: line_id.saturating_sub(1),
            baseline: word_baseline,
            line_baseline: current_line_baseline,
            font_size,
            writing_direction: current_writing_direction,
            final_in_line,
        });

        if unsafe { TessResultIteratorNext(raw, TessPageIteratorLevel::RIL_WORD as c_int) } == 0 {
            break;
        }
    }

    texts
}

fn utf8_text(raw: *mut c_void, level: TessPageIteratorLevel) -> Option<String> {
    // Tesseract allocates the returned C string and transfers ownership to the
    // caller. Convert it immediately, then free it with `TessDeleteText` on all
    // successful pointer paths.
    let text_ptr = unsafe { TessResultIteratorGetUTF8Text(raw, level as c_int) };
    if text_ptr.is_null() {
        return None;
    }
    let text = unsafe { CStr::from_ptr(text_ptr) }
        .to_str()
        .ok()
        .map(str::to_string);
    unsafe { TessDeleteText(text_ptr) };
    text
}

fn bounding_box(raw: *mut c_void, level: TessPageIteratorLevel) -> Option<(i32, i32, i32, i32)> {
    // Bounding boxes come back as left/top/right/bottom in image pixels. The
    // top-left origin is preserved here; coordinate conversion is centralized
    // in the PDF writer.
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
    (ok != 0).then_some((left, top, right, bottom))
}

fn baseline(raw: *mut c_void, level: TessPageIteratorLevel) -> Option<OcrBaseline> {
    // Baselines are returned as two points in image pixels. They may be angled
    // if Tesseract detected skew or rotated text.
    let mut x1 = 0;
    let mut y1 = 0;
    let mut x2 = 0;
    let mut y2 = 0;
    let ok = unsafe {
        TessPageIteratorBaseline(raw, level as c_int, &mut x1, &mut y1, &mut x2, &mut y2)
    };
    (ok != 0).then_some(OcrBaseline::new(x1, y1, x2, y2))
}

fn fallback_baseline(raw: *mut c_void, level: TessPageIteratorLevel) -> OcrBaseline {
    // When Tesseract cannot provide a baseline, use the bottom edge of the
    // bounding box. This keeps the OCR item usable while making the fallback
    // obvious and contained.
    bounding_box(raw, level)
        .map(|(left, _top, right, bottom)| OcrBaseline::new(left, bottom, right, bottom))
        .unwrap_or_else(|| OcrBaseline::new(0, 0, 0, 0))
}

fn orientation(raw: *mut c_void) -> Option<OcrWritingDirection> {
    // `TessPageIteratorOrientation` returns several pieces of orientation
    // metadata. The PDF text layer currently only needs writing direction, but
    // the other output parameters still have to be provided for the C API call.
    let mut orientation = 0;
    let mut writing_direction = 0;
    let mut textline_order = 0;
    let mut deskew_angle = 0.0;
    unsafe {
        TessPageIteratorOrientation(
            raw,
            &mut orientation,
            &mut writing_direction,
            &mut textline_order,
            &mut deskew_angle,
        )
    };
    Some(OcrWritingDirection::from_tesseract(writing_direction))
}

fn word_font_size(raw: *mut c_void) -> Option<i32> {
    // The C API returns a group of font-style flags plus point size and font id.
    // For hidden OCR text we only need point size: style is irrelevant because
    // rendering mode 3 makes the text invisible, and the glyphless font is used
    // solely to provide stable selectable/copyable geometry.
    let mut is_bold = 0;
    let mut is_italic = 0;
    let mut is_underlined = 0;
    let mut is_monospace = 0;
    let mut is_serif = 0;
    let mut is_smallcaps = 0;
    let mut pointsize = 0;
    let mut font_id = 0;
    let ok = unsafe {
        TessResultIteratorWordFontAttributes(
            raw,
            &mut is_bold,
            &mut is_italic,
            &mut is_underlined,
            &mut is_monospace,
            &mut is_serif,
            &mut is_smallcaps,
            &mut pointsize,
            &mut font_id,
        )
    };
    (ok != 0 && pointsize > 0).then_some(pointsize)
}

// Raw Tesseract C API calls that are not currently surfaced by
// `kreuzberg-tesseract`'s safe Rust API.
//
// Keep these declarations narrow. They exist only to reproduce the pieces of
// Tesseract's PDF renderer that affect hidden text geometry: iterator
// traversal, bounding boxes, baselines, writing direction, final-word detection,
// and word font size.
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
            let baseline = OcrBaseline::new(1, 6, 4, 6);
            OcrPage::new(vec![OcrText {
                text: format!("{width}x{height}"),
                x: 1,
                y: 2,
                w: 3,
                h: 4,
                block_id: 0,
                line_id: 0,
                baseline,
                line_baseline: baseline,
                font_size: 4,
                writing_direction: OcrWritingDirection::LeftToRight,
                final_in_line: true,
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
