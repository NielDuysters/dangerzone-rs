//! PDF rendering helpers for invisible OCR text layers.

use anyhow::{Context, Result};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::Write;

use super::{OcrPage, OcrVBaseline, OcrWord, OcrWritingDirection};
use crate::DPI;

const GLYPHLESS_PDF_TTF: &[u8] = include_bytes!("../../assets/pdf.ttf");
const GLYPHLESS_CHAR_WIDTH: f32 = 2.0;

/// Encode OCR text for the glyphless PDF font.
///
/// The OCR font uses `/Encoding /Identity-H`, so each character is written as a
/// 16-bit big-endian code unit inside a PDF hex string. This avoids PDF literal
/// string escaping and lets the `/ToUnicode` CMap map selected text back to
/// normal Unicode for copy/paste and search.
fn to_pdf_utf16be_hex(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 4);
    for unit in text.encode_utf16() {
        out.push_str(&format!("{unit:04X}"));
    }
    out
}

/// Append shared glyphless OCR font objects to the PDF.
///
/// These objects are emitted once per OCR PDF and referenced by every page as
/// `/Focr`. The font is intentionally glyphless: rendering mode `3 Tr` keeps OCR
/// text invisible, while the font metrics and `/ToUnicode` map give PDF viewers
/// enough structure for selection, search, and copy/paste.
pub(crate) fn append_glyphless_ocr_font_objects(
    pdf_data: &mut Vec<u8>,
    object_offsets: &mut Vec<usize>,
) -> Result<()> {
    // Object 3 is the Type0 composite font used by page resources. It does not
    // contain glyph metrics directly; it points to the CID descendant font and
    // the `/ToUnicode` CMap below.
    object_offsets.push(pdf_data.len());
    pdf_data.extend_from_slice(b"3 0 obj\n");
    pdf_data.extend_from_slice(b"<<\n");
    pdf_data.extend_from_slice(b" /BaseFont /GlyphLessFont\n");
    pdf_data.extend_from_slice(b" /DescendantFonts [ 4 0 R ]\n");
    pdf_data.extend_from_slice(b" /Encoding /Identity-H\n");
    pdf_data.extend_from_slice(b" /Subtype /Type0\n");
    pdf_data.extend_from_slice(b" /ToUnicode 6 0 R\n");
    pdf_data.extend_from_slice(b" /Type /Font\n");
    pdf_data.extend_from_slice(b">>\n");
    pdf_data.extend_from_slice(b"endobj\n");

    // Object 4 is the CIDFontType2 descendant. `/DW 500` declares the default
    // glyph advance used by PDF viewers when computing text geometry. The text
    // is invisible, but this advance still matters for selection rectangles.
    object_offsets.push(pdf_data.len());
    pdf_data.extend_from_slice(b"4 0 obj\n");
    pdf_data.extend_from_slice(b"<<\n");
    pdf_data.extend_from_slice(b" /BaseFont /GlyphLessFont\n");
    pdf_data.extend_from_slice(b" /CIDToGIDMap 5 0 R\n");
    pdf_data.extend_from_slice(
        b" /CIDSystemInfo << /Ordering (Identity) /Registry (Adobe) /Supplement 0 >>\n",
    );
    pdf_data.extend_from_slice(b" /FontDescriptor 7 0 R\n");
    pdf_data.extend_from_slice(b" /Subtype /CIDFontType2\n");
    pdf_data.extend_from_slice(b" /Type /Font\n");
    pdf_data.extend_from_slice(b" /DW 500\n");
    pdf_data.extend_from_slice(b">>\n");
    pdf_data.extend_from_slice(b"endobj\n");

    // Object 5 maps every possible 16-bit CID to glyph id 1. Tesseract's
    // `pdf.ttf` provides that glyph as a blank glyph, so the OCR text has stable
    // font geometry without drawing visible letters over the sanitized image.
    let mut cid_to_gid_map = vec![0u8; 2 * (1 << 16)];
    for i in 0..(1 << 16) {
        cid_to_gid_map[i * 2 + 1] = 1;
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&cid_to_gid_map)
        .context("Failed to compress CIDToGIDMap")?;
    let compressed_map = encoder
        .finish()
        .context("Failed to finish CIDToGIDMap compression")?;

    object_offsets.push(pdf_data.len());
    pdf_data.extend_from_slice(b"5 0 obj\n");
    pdf_data.extend_from_slice(b"<<\n");
    pdf_data.extend_from_slice(format!(" /Length {}\n", compressed_map.len()).as_bytes());
    pdf_data.extend_from_slice(b" /Filter /FlateDecode\n");
    pdf_data.extend_from_slice(b">>\n");
    pdf_data.extend_from_slice(b"stream\n");
    pdf_data.extend_from_slice(&compressed_map);
    pdf_data.extend_from_slice(b"\nendstream\n");
    pdf_data.extend_from_slice(b"endobj\n");

    // Object 6 maps character ids back to Unicode. Because we emit UTF-16BE hex
    // values, an identity CMap is enough: CID `<0068>` maps to Unicode U+0068,
    // CID `<00E9>` maps to U+00E9, and so on.
    let to_unicode = b"/CIDInit /ProcSet findresource begin\n\
12 dict begin\n\
begincmap\n\
/CIDSystemInfo\n\
<< /Registry (Adobe)\n\
/Ordering (UCS)\n\
/Supplement 0\n\
>> def\n\
/CMapName /Adobe-Identity-UCS def\n\
/CMapType 2 def\n\
1 begincodespacerange\n\
<0000> <FFFF>\n\
endcodespacerange\n\
1 beginbfrange\n\
<0000> <FFFF> <0000>\n\
endbfrange\n\
endcmap\n\
CMapName currentdict /CMap defineresource pop\n\
end\n\
end\n";
    object_offsets.push(pdf_data.len());
    pdf_data.extend_from_slice(b"6 0 obj\n");
    pdf_data.extend_from_slice(format!("<< /Length {} >>\n", to_unicode.len()).as_bytes());
    pdf_data.extend_from_slice(b"stream\n");
    pdf_data.extend_from_slice(to_unicode);
    pdf_data.extend_from_slice(b"endstream\n");
    pdf_data.extend_from_slice(b"endobj\n");

    // Object 7 describes the embedded font program and basic metrics. PDF
    // viewers use this descriptor even though rendering mode 3 prevents actual
    // painting.
    object_offsets.push(pdf_data.len());
    pdf_data.extend_from_slice(b"7 0 obj\n");
    pdf_data.extend_from_slice(b"<<\n");
    pdf_data.extend_from_slice(b" /Ascent 1000\n");
    pdf_data.extend_from_slice(b" /CapHeight 1000\n");
    pdf_data.extend_from_slice(b" /Descent -1\n");
    pdf_data.extend_from_slice(b" /Flags 5\n");
    pdf_data.extend_from_slice(b" /FontBBox [ 0 0 500 1000 ]\n");
    pdf_data.extend_from_slice(b" /FontFile2 8 0 R\n");
    pdf_data.extend_from_slice(b" /FontName /GlyphLessFont\n");
    pdf_data.extend_from_slice(b" /ItalicAngle 0\n");
    pdf_data.extend_from_slice(b" /StemV 80\n");
    pdf_data.extend_from_slice(b" /Type /FontDescriptor\n");
    pdf_data.extend_from_slice(b">>\n");
    pdf_data.extend_from_slice(b"endobj\n");

    // Object 8 embeds Tesseract's small glyphless TrueType font program.
    object_offsets.push(pdf_data.len());
    pdf_data.extend_from_slice(b"8 0 obj\n");
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

struct OcrTextLine<'a> {
    words: Vec<&'a OcrWord>,
}

/// Group OCR words into the text lines reported by the OCR backend.
///
/// The PDF writer should not infer lines from Y coordinates here: OCR backends
/// already know block and line membership, and using that metadata avoids merging
/// nearby columns or skewed lines by accident.
fn ocr_text_lines(words: &[OcrWord]) -> Vec<OcrTextLine<'_>> {
    let mut lines = Vec::new();
    let mut current: Option<OcrTextLine<'_>> = None;

    // Ignore degenerate word boxes. They cannot produce useful PDF text geometry
    // and would make later baseline and scaling calculations less predictable.
    for word in words
        .iter()
        .filter(|word| word.vbox.w > 0 && word.vbox.h > 0)
    {
        match &mut current {
            Some(line)
                if line.words.last().is_some_and(|last| {
                    last.block_id == word.block_id && last.line_id == word.line_id
                }) =>
            {
                // Same OCR block and line as the previous word, so this belongs
                // to the current PDF text object.
                line.words.push(word);
            }
            Some(line) => {
                // The OCR iterator moved to another visual line. Sort the line
                // before storing it so PDF emission is deterministic.
                sort_ocr_line_words(&mut line.words);
                lines.push(current.take().expect("line exists"));
                current = Some(OcrTextLine { words: vec![word] });
            }
            None => {
                current = Some(OcrTextLine { words: vec![word] });
            }
        }
    }

    if let Some(mut line) = current {
        sort_ocr_line_words(&mut line.words);
        lines.push(line);
    }

    lines
}

/// Sort words in visual reading order for one OCR line.
///
/// Tesseract usually returns a useful iterator order, but sorting makes the PDF
/// content deterministic and handles left-to-right versus right-to-left lines
/// explicitly.
fn sort_ocr_line_words(words: &mut [&OcrWord]) {
    if words
        .first()
        .is_some_and(|word| word.writing_direction == OcrWritingDirection::Rtl)
    {
        words.sort_by_key(|word| std::cmp::Reverse(word.vbox.x));
    } else {
        words.sort_by_key(|word| word.vbox.x);
    }
}

/// Return squared distance between two image-space points.
///
/// Several calculations only need relative line length, so avoiding the square
/// root keeps those call sites simple. Callers that need a real length can take
/// `.sqrt()` themselves.
fn dist2(x1: i32, y1: i32, x2: i32, y2: i32) -> f32 {
    let dx = (x2 - x1) as f32;
    let dy = (y2 - y1) as f32;
    dx * dx + dy * dy
}

/// Flatten tiny baseline noise for mostly-horizontal OCR lines.
///
/// Tesseract can report baselines with one or two pixels of vertical variation.
/// If we turn that noise directly into a PDF text matrix, selection rectangles
/// can look slightly rotated or jittery. For long, nearly-horizontal baselines,
/// use the average Y value instead.
fn clip_baseline(baseline: OcrVBaseline) -> OcrVBaseline {
    let mut y1 = baseline.y1;
    let mut y2 = baseline.y2;
    let rise = (y2 - y1).abs() as f32 * 72.0;
    let run = (baseline.x2 - baseline.x1).abs() as f32 * 72.0;

    if rise < 2.0 * DPI && 2.0 * DPI < run {
        let y = (y1 + y2) / 2;
        y1 = y;
        y2 = y;
    }

    OcrVBaseline::new(baseline.x1, y1, baseline.x2, y2)
}

/// Project a word baseline onto its containing line baseline.
///
/// The returned tuple is `(x_pts, y_pts, word_length_pts)`: the PDF-space start
/// point for the word and the measured word length used later for `Tz`
/// horizontal scaling.
fn word_baseline_position(
    word: &OcrWord,
    line_baseline: OcrVBaseline,
    page_height_pts: f32,
) -> (f32, f32, f32) {
    let mut word_baseline = word.vbaseline;
    if word.writing_direction == OcrWritingDirection::Rtl {
        // For Rtl text the visual start of the word is the opposite baseline
        // endpoint. Flip before projection so the text matrix starts on the
        // side where the PDF text should begin advancing.
        word_baseline = OcrVBaseline::new(
            word_baseline.x2,
            word_baseline.y2,
            word_baseline.x1,
            word_baseline.y1,
        );
    }

    // Project the word's starting point onto the containing line baseline.
    // This follows Tesseract's PDF renderer: text is positioned from baselines,
    // not from bounding-box corners.
    let line_length_squared = dist2(
        line_baseline.x1,
        line_baseline.y1,
        line_baseline.x2,
        line_baseline.y2,
    );
    let (x, y) = if line_length_squared == 0.0 {
        // Degenerate baseline. Use the first line-baseline point rather than
        // dividing by zero; this still emits selectable text near the OCR line.
        (line_baseline.x1 as f32, line_baseline.y1 as f32)
    } else {
        // Parameter of the projected word-start point on the line-baseline
        // vector. The formula is written from the line end backwards to match
        // the shape of Tesseract's renderer.
        let t = ((word_baseline.x1 - line_baseline.x2) as f32
            * (line_baseline.x2 - line_baseline.x1) as f32
            + (word_baseline.y1 - line_baseline.y2) as f32
                * (line_baseline.y2 - line_baseline.y1) as f32)
            / line_length_squared;
        (
            line_baseline.x2 as f32 + t * (line_baseline.x2 - line_baseline.x1) as f32,
            line_baseline.y2 as f32 + t * (line_baseline.y2 - line_baseline.y1) as f32,
        )
    };

    // Use the OCR word baseline length as the target selection advance. The
    // content stream later scales the glyphless font horizontally to this length.
    let word_length = dist2(
        word_baseline.x1,
        word_baseline.y1,
        word_baseline.x2,
        word_baseline.y2,
    )
    .sqrt()
        * 72.0
        / DPI;

    (
        // Convert image pixels at `DPI` to PDF points.
        x * 72.0 / DPI,
        // Flip Y from image coordinates (top-left origin) to PDF coordinates
        // (bottom-left origin).
        page_height_pts - (y * 72.0 / DPI),
        word_length,
    )
}

/// Calculate the 2x2 part of the PDF text matrix from an OCR line baseline.
///
/// The matrix rotates hidden text onto the same angle Tesseract detected in the
/// raster image. For Rtl text, the horizontal advance is reflected.
fn affine_matrix(
    direction: OcrWritingDirection,
    line_baseline: OcrVBaseline,
) -> (f32, f32, f32, f32) {
    // `theta` is the angle of the baseline in image coordinates. The Y
    // difference is inverted here because the PDF text matrix lives in a
    // bottom-left coordinate system while OCR coordinates are top-left based.
    let theta = ((line_baseline.y1 - line_baseline.y2) as f32)
        .atan2((line_baseline.x2 - line_baseline.x1) as f32);

    // Standard 2D rotation matrix:
    //
    //   [ cos(theta)  -sin(theta) ]
    //   [ sin(theta)   cos(theta) ]
    //
    // PDF writes this as `a b c d x y Tm`.
    let mut a = theta.cos();
    let mut b = theta.sin();
    let c = -theta.sin();
    let d = theta.cos();

    if direction == OcrWritingDirection::Rtl {
        // Reflect the text advance for right-to-left lines while preserving the
        // baseline angle.
        a = -a;
        b = -b;
    }

    (a, b, c, d)
}

/// Append invisible OCR text operations to a page content stream.
pub(crate) fn append_page_text_layer(content: &mut String, ocr_page: &OcrPage, height_pts: f32) {
    // The glyphless font declares a default width of 500 units. In PDF text
    // space that is 0.5em, so this reciprocal factor is used when calculating
    // `Tz` to stretch each invisible word to its OCR baseline length.
    for line in ocr_text_lines(ocr_page.words()) {
        let words = line
            .words
            .iter()
            .filter(|word| !word.text.trim().is_empty())
            .collect::<Vec<_>>();

        if words.is_empty() {
            continue;
        }

        // One PDF text object per OCR line gives viewers a natural selection
        // order while still allowing per-word positioning.
        content.push_str("BT\n3 Tr\n");

        // Track the previous word's PDF-space position so later words can use
        // relative `Td` moves. This mirrors Tesseract and avoids resetting the
        // full matrix for every word.
        let mut old_x = 0.0;
        let mut old_y = 0.0;
        let mut old_direction = None;
        let mut first_word = true;

        for (word_idx, word) in words.iter().enumerate() {
            let text = word.text.trim();
            let char_count = text.chars().count();
            if char_count == 0 {
                continue;
            }

            // Use the line baseline as the coordinate system for all words on
            // the line, then project each word's own baseline onto it to find
            // the text start point.
            let line_baseline = clip_baseline(word.line_vbaseline);
            let (x_pts, y_pts, word_length_pts) =
                word_baseline_position(word, line_baseline, height_pts);

            // Prefer the OCR-reported point size. Some words do not have one, so
            // fall back to a conservative value derived from the word box height.
            let font_size = if word.font_size > 0 {
                word.font_size as f32
            } else {
                (word.vbox.h as f32 * 72.0 / DPI * 0.75).max(1.0)
            };

            // `Tz` is horizontal scaling in percent. Scale the glyphless text so
            // the selectable area spans the measured OCR baseline length instead
            // of the font's nominal character width.
            let horizontal_scale = (GLYPHLESS_CHAR_WIDTH * 100.0 * word_length_pts.max(1.0)
                / (font_size * char_count as f32))
                .clamp(5.0, 300.0);

            // Tesseract inserts spaces between words but not after the final word
            // of a line. The OCR backend returns trimmed words, so the PDF layer
            // has to add that spacing explicitly.
            let pdf_word = if !word.last_in_line && word_idx + 1 < words.len() {
                format!("{text} ")
            } else {
                text.to_string()
            };
            let text_hex = to_pdf_utf16be_hex(&pdf_word);
            let (a, b, c, d) = affine_matrix(word.writing_direction, line_baseline);

            if first_word || old_direction != Some(word.writing_direction) {
                // `Tm` sets the full text matrix. Use it for the first word and
                // when direction changes because `Td` can only move within the
                // current text coordinate system.
                content.push_str(&format!(
                    "{a:.3} {b:.3} {c:.3} {d:.3} {x_pts:.2} {y_pts:.2} Tm\n/Focr {font_size:.2} Tf\n"
                ));
                first_word = false;
            } else {
                // Convert the PDF-space movement back into the current text-space
                // basis before emitting `Td`. This keeps relative moves correct
                // for skewed/rotated baselines.
                let dx = x_pts - old_x;
                let dy = y_pts - old_y;
                let text_dx = dx * a + dy * b;
                let text_dy = dx * c + dy * d;
                content.push_str(&format!(
                    "{text_dx:.2} {text_dy:.2} Td\n/Focr {font_size:.2} Tf\n"
                ));
            }

            // `TJ` accepts a text array. We use one hex string per word; the
            // preceding `Tz` makes that word's selection geometry match the OCR
            // baseline length.
            content.push_str(&format!("{horizontal_scale:.2} Tz\n[ <{text_hex}> ] TJ\n"));
            old_x = x_pts;
            old_y = y_pts;
            old_direction = Some(word.writing_direction);
        }

        content.push_str("ET\n");
    }
}
