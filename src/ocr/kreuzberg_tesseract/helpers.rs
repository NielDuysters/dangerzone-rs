//! Helper methods for tesseract.

use std::ffi::CStr;
use std::os::raw::{c_int, c_void};

use kreuzberg_tesseract::TessPageIteratorLevel;

use super::bindings;
use crate::ocr::{OcrVBox, OcrWord};

/// Extract OCR words and their properties.
///
/// Required to construct OcrWord's. We use Tesseract's low-level iterator since it provides
/// more details.
pub(super) fn extract_ocr_words(iterator: &::kreuzberg_tesseract::ResultIterator) -> Vec<OcrWord> {
    // Get raw handle
    let Ok(handle) = iterator.handle.lock() else {
        return Vec::new();
    };
    let raw = *handle;

    // Vector containing results we will return
    let mut ocr_words: Vec<OcrWord> = Vec::new();

    // Track line_id to set property on `OcrWord`. This property can later be used to group words
    // into lines.
    let mut line_id: usize = 0;

    // Reset iterator to first word on page
    unsafe { bindings::TessPageIteratorBegin(raw) };

    // Loop over words on page
    loop {
        if unsafe {
            bindings::TessPageIteratorIsAtBeginningOf(
                raw,
                TessPageIteratorLevel::RIL_TEXTLINE as c_int,
            )
        } != 0
        {
            line_id += 1;
        }

        // Extract text with word-level granularity.
        let Some(text) = utf8_text(raw, TessPageIteratorLevel::RIL_WORD) else {
            // Manually move iterator to next word
            if unsafe {
                bindings::TessResultIteratorNext(raw, TessPageIteratorLevel::RIL_WORD as c_int)
            } == 0
            {
                // No next word found on page. Break loop.
                break;
            }
            // No text found for current word. But continue scanning next words.
            continue;
        };

        // Trim text, and if it's empty try continuing to
        // next word or end loop if no next is found.
        let text = text.trim().to_string();
        if text.is_empty() {
            if unsafe {
                bindings::TessResultIteratorNext(raw, TessPageIteratorLevel::RIL_WORD as c_int)
            } == 0
            {
                break;
            }

            continue;
        }

        // Check if word has a bounding_box. Ignore if it doesn't to avoid poisoning whole OCR
        // result.
        let Some(vbox) = bounding_box(raw, TessPageIteratorLevel::RIL_WORD) else {
            if unsafe {
                bindings::TessResultIteratorNext(raw, TessPageIteratorLevel::RIL_WORD as c_int)
            } == 0
            {
                break;
            }

            continue;
        };

        // Put extracted properties in `OcrWord` object and
        // push to result list.
        ocr_words.push(OcrWord {
            text,
            vbox,
            line_id,
        });

        // Exit looping over words if no new word is found on page.
        if unsafe {
            bindings::TessResultIteratorNext(raw, TessPageIteratorLevel::RIL_WORD as c_int)
        } == 0
        {
            break;
        }
    }

    ocr_words
}

/// Bounding boxes come back as top, left, right, bottom.
/// We convert it to our OcrVBox object.
fn bounding_box(raw: *mut c_void, level: TessPageIteratorLevel) -> Option<OcrVBox> {
    let mut left = 0;
    let mut top = 0;
    let mut right = 0;
    let mut bottom = 0;
    let ok = unsafe {
        bindings::TessPageIteratorBoundingBox(
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

/// Get text returned by tesseract.
fn utf8_text(raw: *mut c_void, level: TessPageIteratorLevel) -> Option<String> {
    // Retrieve text
    let text_ptr = unsafe { bindings::TessResultIteratorGetUTF8Text(raw, level as c_int) };
    if text_ptr.is_null() {
        return None;
    }
    // Transfer ownership to caller.
    let text = unsafe { CStr::from_ptr(text_ptr) }
        .to_str()
        .ok()
        .map(str::to_string);

    // Free pointer.
    unsafe { bindings::TessDeleteText(text_ptr) };
    text
}
