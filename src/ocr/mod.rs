//! Components and logic to handle OCR

use crate::PageData;

pub(crate) mod kreuzberg_tesseract;
pub(crate) mod pdf_renderer;
pub(crate) use self::kreuzberg_tesseract::KreuzbergTesseractOcr;

/// DPI used by container
pub const DEFAULT_DPI: i32 = 150;

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
    /// Index of text-block this word belongs to
    ///
    /// Used to avoid mixing words from different blocks into one
    pub block_id: usize,
    /// Index of the line this word belongs to
    pub line_id: usize,
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
    pub words: Vec<&'a OcrWord>,
}

/// Group individual OCR words into text lines reported by the OCR backend.
pub(crate) fn merge_ocr_words_into_ocr_text_line(
    // This argument is a borrowed slice of `OcrWords`. Due to this borrowed slice we need a
    // specified lifetime for `OcrTextLine`.
    // The alternative to avoid lifetimes would be to make words a Vec copying the words, but this
    // would result in poor performance.
    words: &[OcrWord],
) -> Vec<OcrTextLine<'_>> {
    // Lines we will return as result.
    let mut lines: Vec<OcrTextLine<'_>> = Vec::new();
    // Current line we are processing.
    let mut curr_line: Option<OcrTextLine<'_>> = None;

    // Helper method returning if current word is in
    // the currently processed line.
    fn word_in_curr_line(line: &OcrTextLine<'_>, word: &OcrWord) -> bool {
        line.words
            .last()
            .is_some_and(|last| last.block_id == word.block_id && last.line_id == word.line_id)
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
                    vbox: OcrVBox { x, y, w, h },
                    block_id: 0,
                    line_id: 0,
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

    /// Run OCR for multiple pages.
    fn ocr_pages(&self, pages: &[PageData]) -> Vec<OcrPage> {
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
            }])
        }
    }

    #[test]
    fn ocr_pages_runs_backend_for_each_page() {
        let pages = vec![
            PageData::new(10, 20, vec![255; 10 * 20 * 3]),
            PageData::new(30, 40, vec![255; 30 * 40 * 3]),
        ];

        let result = FakeOcrBackend.ocr_pages(&pages);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].words[0].text, "10x20");
        assert_eq!(result[1].words[0].text, "30x40");
    }

    #[test]
    fn merge_ocr_words_groups_and_sorts_words_by_line() {
        let words = vec![
            OcrWord {
                text: "line1-right".to_string(),
                vbox: OcrVBox {
                    x: 30,
                    y: 0,
                    w: 10,
                    h: 10,
                },
                block_id: 0,
                line_id: 0,
            },
            OcrWord {
                text: "line1-left".to_string(),
                vbox: OcrVBox {
                    x: 10,
                    y: 0,
                    w: 10,
                    h: 10,
                },
                block_id: 0,
                line_id: 0,
            },
            OcrWord {
                text: "line2".to_string(),
                vbox: OcrVBox {
                    x: 20,
                    y: 20,
                    w: 10,
                    h: 10,
                },
                block_id: 0,
                line_id: 1,
            },
        ];

        let lines = merge_ocr_words_into_ocr_text_line(&words);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].words.len(), 2);
        assert_eq!(lines[0].words[0].text, "line1-left");
        assert_eq!(lines[0].words[1].text, "line1-right");
        assert_eq!(lines[1].words.len(), 1);
        assert_eq!(lines[1].words[0].text, "line2");
    }
}
