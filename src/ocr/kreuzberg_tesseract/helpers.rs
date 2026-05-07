//! Helper methods for extracting Tesseract OCR metadata.

use std::ffi::CStr;
use std::os::raw::{c_int, c_void};

use super::bindings;
use crate::ocr::{OcrVBaseline, OcrVBox, OcrWord, OcrWritingDirection};
use ::kreuzberg_tesseract::TessPageIteratorLevel;

/// Extract PDF words and their properties.
///
/// Required to construct OcrWord's. We use Tesseract's low-level iterator since it provides
/// more details.
pub(super) fn extract_pdf_words(iterator: &::kreuzberg_tesseract::ResultIterator) -> Vec<OcrWord> {
    // Get raw handle
    let Ok(handle) = iterator.handle.lock() else {
        return Vec::new();
    };
    let raw = *handle;

    // Vector containing results we will return
    let mut ocr_words: Vec<OcrWord> = Vec::new();

    // Helper properties used when looping over iterator
    let mut block_id: usize = 0;
    let mut line_id: usize = 0;
    let mut curr_line_baseline = OcrVBaseline::new(0, 0, 0, 0);
    let mut curr_writing_direction = OcrWritingDirection::LTR;

    // Reset iterator to first word on page
    unsafe { bindings::TessPageIteratorBegin(raw) };

    // Loop over words on page
    loop {
        // Tesseract has moved to a new visual element
        //
        // Update block_id to prevent the PDF writer to join/mix
        // text that should remain separated.
        if unsafe {
            bindings::TessPageIteratorIsAtBeginningOf(
                raw,
                TessPageIteratorLevel::RIL_BLOCK as c_int,
            )
        } != 0
        {
            block_id += 1;
        }

        // Store text-level baseline when the iterator goes into next line.
        // A line-level baseline is used as reference for rotated/skewed text.
        if unsafe {
            bindings::TessPageIteratorIsAtBeginningOf(
                raw,
                TessPageIteratorLevel::RIL_TEXTLINE as c_int,
            )
        } != 0
        {
            line_id += 1;
            curr_line_baseline = baseline(raw, TessPageIteratorLevel::RIL_TEXTLINE)
                .unwrap_or_else(|| fallback_baseline(raw, TessPageIteratorLevel::RIL_TEXTLINE));
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
        // next word of end loop if no next is found.
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

        // Set word_baseline. Fall back to horizontal line at
        // bottom of word-box is missing.
        let word_baseline = baseline(raw, TessPageIteratorLevel::RIL_WORD).unwrap_or_else(|| {
            OcrVBaseline::new(vbox.x, vbox.y + vbox.h, vbox.x + vbox.w, vbox.y + vbox.h)
        });

        // Set direction. We cache this since orientation is
        // not a property specific to one word alone, but all words
        // on the same line need the same orientation. We remember
        // the last orientation Tesseract reported.
        if let Some(direction) = writing_direction(raw) {
            curr_writing_direction = direction;
        }

        // Set font size.
        let font_size = word_font_size(raw).unwrap_or(0);

        // Set flag determining if word is last in line.
        // This flag avoids setting trailing spaces which would be
        // required when there would be a next word. Since it's
        // the last word no trailing space is required.
        let last_in_line = unsafe {
            bindings::TessPageIteratorIsAtFinalElement(
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

/// Baselines are returned as two points in image pixels. They may be angled
/// if Tesseract detected skew or rotated text.
fn baseline(raw: *mut c_void, level: TessPageIteratorLevel) -> Option<OcrVBaseline> {
    let mut x1 = 0;
    let mut y1 = 0;
    let mut x2 = 0;
    let mut y2 = 0;
    let ok = unsafe {
        bindings::TessPageIteratorBaseline(raw, level as c_int, &mut x1, &mut y1, &mut x2, &mut y2)
    };
    (ok != 0).then_some(OcrVBaseline::new(x1, y1, x2, y2))
}

/// When Tesseract cannot provide a baseline, use the bottom edge of the
/// bounding box.
fn fallback_baseline(raw: *mut c_void, level: TessPageIteratorLevel) -> OcrVBaseline {
    bounding_box(raw, level)
        .map(|vbox| OcrVBaseline::new(vbox.x, vbox.y + vbox.h, vbox.x + vbox.w, vbox.y + vbox.h))
        .unwrap_or_else(|| OcrVBaseline::new(0, 0, 0, 0))
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

/// Get writing direction metadata from Tesseract.
fn writing_direction(raw: *mut c_void) -> Option<OcrWritingDirection> {
    let mut _orientation = 0;
    let mut writing_direction = 0;
    let mut _textline_order = 0;
    let mut _deskew_angle = 0.0;
    unsafe {
        bindings::TessPageIteratorOrientation(
            raw,
            &mut _orientation,
            &mut writing_direction,
            &mut _textline_order,
            &mut _deskew_angle,
        )
    };

    Some(match writing_direction {
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
        bindings::TessResultIteratorWordFontAttributes(
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
