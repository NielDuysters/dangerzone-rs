//! Helper methods for tesseract.

use crate::ocr::{OcrVBox, OcrWord};

/// Extract OCR words and their properties.
///
/// Uses the crate's high-level iterator helper and does not rely on low-level
/// Tesseract iterator bindings.
pub(super) fn extract_ocr_words(iterator: &::kreuzberg_tesseract::ResultIterator) -> Vec<OcrWord> {
    let Ok(words) = iterator.extract_all_words() else {
        return Vec::new();
    };

    words
        .into_iter()
        .enumerate()
        .filter_map(|(idx, word)| {
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
                // Keep each recognized word in its own line group.
                line_id: idx,
            })
        })
        .collect()
}
