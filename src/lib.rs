use anyhow::{Context, Result};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::fs::File;
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::process::{Command, Stdio};
use util::replace_control_chars;

mod ocr;
mod util;

pub const IMAGE_NAME: &str = "ghcr.io/freedomofpress/dangerzone/v1";
pub const INT_BYTES: usize = 2;
pub const DPI: f32 = 150.0;
const MAX_SANITIZED_CHUNK_BYTES: u64 = 64 * 1024;
const GLYPHLESS_PDF_TTF: &[u8] = include_bytes!("../assets/pdf.ttf");

fn get_security_args() -> Vec<String> {
    vec![
        "--log-driver".to_string(),
        "none".to_string(),
        "--security-opt".to_string(),
        "no-new-privileges".to_string(),
        "--cap-drop".to_string(),
        "all".to_string(),
        "--cap-add".to_string(),
        "SYS_CHROOT".to_string(),
        "--security-opt".to_string(),
        "label=type:container_engine_t".to_string(),
        "--network=none".to_string(),
        "-u".to_string(),
        "dangerzone".to_string(),
    ]
}

fn read_u16_be(data: &[u8]) -> Result<u16> {
    if data.len() < INT_BYTES {
        anyhow::bail!("Not enough bytes to read u16");
    }
    Ok(u16::from_be_bytes([data[0], data[1]]))
}

/// Page data structure representing a single page's pixel information
#[derive(Clone)]
pub struct PageData {
    pub width: u16,
    pub height: u16,
    pub pixels: Vec<u8>,
}

impl PageData {
    pub fn new(width: u16, height: u16, pixels: Vec<u8>) -> Self {
        PageData {
            width,
            height,
            pixels,
        }
    }
}

/// Parse binary pixel data stream from the container
/// Returns a list of (width, height, pixel_data) tuples for each page
pub fn parse_pixel_data(data: Vec<u8>) -> Result<Vec<PageData>> {
    let mut pos = 0;

    // Read page count
    if data.len() < INT_BYTES {
        anyhow::bail!("Insufficient data for page count");
    }
    let page_count = read_u16_be(&data[pos..pos + INT_BYTES])?;
    pos += INT_BYTES;

    eprintln!("Document has {page_count} page(s)");

    let mut pages = Vec::new();

    for page_num in 0..page_count {
        // Read width
        if pos + INT_BYTES > data.len() {
            anyhow::bail!("Insufficient data for page {} width", page_num + 1);
        }
        let width = read_u16_be(&data[pos..pos + INT_BYTES])?;
        pos += INT_BYTES;

        // Read height
        if pos + INT_BYTES > data.len() {
            anyhow::bail!("Insufficient data for page {} height", page_num + 1);
        }
        let height = read_u16_be(&data[pos..pos + INT_BYTES])?;
        pos += INT_BYTES;

        eprintln!("Page {}: {}x{} pixels", page_num + 1, width, height);

        // Read pixel data (RGB, 3 bytes per pixel)
        let num_bytes = (width as usize) * (height as usize) * 3;
        if pos + num_bytes > data.len() {
            anyhow::bail!(
                "Insufficient data for page {} pixels (expected {} bytes)",
                page_num + 1,
                num_bytes
            );
        }

        let pixels = data[pos..pos + num_bytes].to_vec();
        pos += num_bytes;

        pages.push(PageData {
            width,
            height,
            pixels,
        });
    }

    Ok(pages)
}

/// Read from a source (mostly locked stderr/stdout) and write sanitized
/// text to given output. Output is marked as untrusted
fn forward_sanitized_text<R: BufRead, W: Write + IsTerminal>(
    mut reader: R,
    mut out: W,
) -> Result<()> {
    const ANSI_GRAY: &str = "\x1b[90m";
    const ANSI_RESET: &str = "\x1b[0m";
    const UNTRUSTED_PREFIX: &str = "UNTRUSTED> ";

    let mut line_buf = Vec::new();
    loop {
        line_buf.clear();
        let n = reader
            .by_ref()
            .take(MAX_SANITIZED_CHUNK_BYTES)
            .read_until(b'\n', &mut line_buf)
            .context("Failed to read output for sanitizing")?;
        if n == 0 {
            break;
        }

        let s = String::from_utf8_lossy(&line_buf);
        let mut sanitized: String = replace_control_chars(&s, true);
        if !sanitized.ends_with('\n') {
            sanitized.push('\n');
        }
        let sanitized_untrusted_prefix = if out.is_terminal() {
            format!("{ANSI_GRAY}{UNTRUSTED_PREFIX}{sanitized}{ANSI_RESET}")
        } else {
            format!("{UNTRUSTED_PREFIX}{sanitized}")
        };

        out.write_all(sanitized_untrusted_prefix.as_bytes())
            .context("Failed to write sanitized output")?;
        out.flush().context("Failed to flush sanitized output")?;
    }

    Ok(())
}

/// Convert a document to raw RGB pixel data using the Dangerzone container
pub fn convert_doc_to_pixels(input_path: String) -> Result<Vec<u8>> {
    eprintln!("Converting document to pixels...");

    let mut args = vec!["run".to_string()];
    args.extend(get_security_args());
    args.extend(vec![
        "--rm".to_string(),
        "-i".to_string(),
        IMAGE_NAME.to_string(),
        "/usr/bin/python3".to_string(),
        "-m".to_string(),
        "dangerzone.conversion.doc_to_pixels".to_string(),
    ]);

    let mut child = Command::new("podman")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context(format!(
            "Failed to spawn container. Make sure podman is installed and the image '{IMAGE_NAME}' is pulled."
        ))?;

    // Take ownership of child stderr pipe and output sanitized text to parent stderr
    let stderr = child
        .stderr
        .take()
        .context("Failed to take ownership of stderr")?;
    let stderr_thread = std::thread::spawn(move || -> Result<()> {
        forward_sanitized_text(BufReader::new(stderr), std::io::stderr().lock())
    });

    // Read the input document
    let mut input_file = File::open(&input_path).context(format!(
        "Failed to open input file '{input_path_sanitized}'",
        input_path_sanitized = replace_control_chars(&input_path, false)
    ))?;
    let mut input_data = Vec::new();
    input_file
        .read_to_end(&mut input_data)
        .context("Failed to read input file")?;

    // Write the document to the container's stdin
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&input_data)
            .context("Failed to write to container stdin")?;
    }

    // Read the output from the container
    let output = child
        .wait_with_output()
        .context("Failed to wait for container")?;

    // Read stderr from the container
    match stderr_thread.join() {
        Err(_) => {
            eprintln!("Warning: stderr_thread panicked while forwarding container stderr");
        }
        Ok(Err(e)) => {
            eprintln!(
                "Warning: Failed to forward container stderr: {err_sanitized}",
                err_sanitized = replace_control_chars(&e.to_string(), true)
            );
        }
        Ok(Ok(_)) => {}
    }

    if !output.status.success() {
        anyhow::bail!(
            "Container failed with status: {}. The document format may be unsupported or corrupted.",
            output.status
        );
    }

    eprintln!("Document converted to pixels successfully");
    Ok(output.stdout)
}

/// Convert pixel data to a PDF file
pub fn pixels_to_pdf(pages: Vec<PageData>, output_path: String) -> Result<()> {
    eprintln!("Converting pixels to safe PDF...");

    if pages.is_empty() {
        anyhow::bail!("No pages to convert");
    }

    let mut file = File::create(&output_path).context(format!(
        "Failed to create output file '{output_path_sanitized}'",
        output_path_sanitized = replace_control_chars(&output_path, false)
    ))?;
    write_pdf(&mut file, &pages, None).context("Failed to write PDF")?;

    eprintln!(
        "Safe PDF created successfully at: {output_path_sanitized}",
        output_path_sanitized = replace_control_chars(&output_path, false)
    );
    Ok(())
}

/// Convert pixel data to a PDF file and add the provided OCR text layer
fn pixels_to_pdf_with_ocr(
    pages: &[PageData],
    ocr_pages: &[ocr::OcrPage],
    output_path: &str,
) -> Result<()> {
    eprintln!("Converting pixels to safe PDF with OCR text layer...");

    if pages.is_empty() {
        anyhow::bail!("No pages to convert");
    }

    let mut file = File::create(output_path).context(format!(
        "Failed to create output file '{output_path_sanitized}'",
        output_path_sanitized = replace_control_chars(output_path, false)
    ))?;
    write_pdf(&mut file, pages, Some(ocr_pages)).context("Failed to write PDF with OCR")?;

    eprintln!(
        "Safe PDF with OCR created successfully at: {output_path_sanitized}",
        output_path_sanitized = replace_control_chars(output_path, false)
    );
    Ok(())
}

/// Convert a document to a safe PDF in one call
pub fn convert_document(input_path: String, output_path: String, apply_ocr: bool) -> Result<()> {
    let pixels_data = convert_doc_to_pixels(input_path)?;
    let pages = parse_pixel_data(pixels_data)?;

    // TODO: When having the implementations for Apple Vision and windows.media.ocr I want to use
    // conditional compilation flags to dynamically set the OCR backend.
    #[cfg(target_os = "linux")]
    if apply_ocr {
        eprintln!("Applying OCR with integrated Linux backend...");

        let backend = ocr::KreuzbergTesseractOcr;
        let ocr_pages = ocr::ocr_pages(&pages, &backend);
        return pixels_to_pdf_with_ocr(&pages, &ocr_pages, &output_path)
            .context("Failed to convert pixels to OCR PDF");
    }

    let temp_output = if apply_ocr {
        format!("{output_path}.temp.pdf")
    } else {
        output_path.clone()
    };

    pixels_to_pdf(pages.clone(), temp_output.clone()).context("Failed to convert pixels to PDF")?;

    if apply_ocr {
        apply_ocr_fn(temp_output.clone(), output_path.clone())?;
        std::fs::remove_file(&temp_output).context("Failed to remove temporary file")?;
    }

    Ok(())
}

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
fn append_glyphless_ocr_font_objects(
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
    words: Vec<&'a ocr::OcrWord>,
}

/// Group OCR words into the text lines reported by the OCR backend.
///
/// The PDF writer should not infer lines from Y coordinates here: OCR backends
/// already know block and line membership, and using that metadata avoids merging
/// nearby columns or skewed lines by accident.
fn ocr_text_lines(words: &[ocr::OcrWord]) -> Vec<OcrTextLine<'_>> {
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
fn sort_ocr_line_words(words: &mut [&ocr::OcrWord]) {
    if words
        .first()
        .is_some_and(|word| word.writing_direction == ocr::OcrWritingDirection::RTL)
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
fn clip_baseline(baseline: ocr::OcrVBaseline) -> ocr::OcrVBaseline {
    let mut y1 = baseline.y1;
    let mut y2 = baseline.y2;
    let rise = (y2 - y1).abs() as f32 * 72.0;
    let run = (baseline.x2 - baseline.x1).abs() as f32 * 72.0;

    if rise < 2.0 * DPI && 2.0 * DPI < run {
        let y = (y1 + y2) / 2;
        y1 = y;
        y2 = y;
    }

    ocr::OcrVBaseline::new(baseline.x1, y1, baseline.x2, y2)
}

/// Project a word baseline onto its containing line baseline.
///
/// The returned tuple is `(x_pts, y_pts, word_length_pts)`: the PDF-space start
/// point for the word and the measured word length used later for `Tz`
/// horizontal scaling.
fn word_baseline_position(
    word: &ocr::OcrWord,
    line_baseline: ocr::OcrVBaseline,
    page_height_pts: f32,
) -> (f32, f32, f32) {
    let mut word_baseline = word.vbaseline;
    if word.writing_direction == ocr::OcrWritingDirection::RTL {
        // For RTL text the visual start of the word is the opposite baseline
        // endpoint. Flip before projection so the text matrix starts on the
        // side where the PDF text should begin advancing.
        word_baseline = ocr::OcrVBaseline::new(
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
/// raster image. For RTL text, the horizontal advance is reflected.
fn affine_matrix(
    direction: ocr::OcrWritingDirection,
    line_baseline: ocr::OcrVBaseline,
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

    if direction == ocr::OcrWritingDirection::RTL {
        // Reflect the text advance for right-to-left lines while preserving the
        // baseline angle.
        a = -a;
        b = -b;
    }

    (a, b, c, d)
}

/// Write a minimal PDF file with embedded RGB pixel data.
///
/// When OCR data is provided, this writes the same raster image PDF plus a
/// hidden text layer. The OCR path uses shared glyphless font objects and
/// Tesseract-style baseline placement so PDF selection follows the visible
/// raster text.
fn write_pdf<W: Write>(
    writer: &mut W,
    pages: &[PageData],
    ocr_pages: Option<&[ocr::OcrPage]>,
) -> Result<()> {
    if let Some(ocr_pages) = ocr_pages {
        if ocr_pages.len() != pages.len() {
            anyhow::bail!(
                "OCR page count ({}) does not match PDF page count ({})",
                ocr_pages.len(),
                pages.len()
            );
        }
    }

    let mut pdf_data = Vec::new();
    let mut object_offsets = Vec::new();
    let has_ocr = ocr_pages.is_some();
    // OCR PDFs reserve objects 3-8 for the shared glyphless font. Image-only
    // PDFs keep the original compact numbering and start page objects at 3.
    let first_page_obj_num = if has_ocr { 9 } else { 3 };

    // PDF Header
    pdf_data.extend_from_slice(b"%PDF-1.4\n");
    pdf_data.extend_from_slice(b"%\xE2\xE3\xCF\xD3\n");

    // Object 1: Catalog
    object_offsets.push(pdf_data.len());
    pdf_data.extend_from_slice(b"1 0 obj\n");
    pdf_data.extend_from_slice(b"<<\n");
    pdf_data.extend_from_slice(b"/Type /Catalog\n");
    pdf_data.extend_from_slice(b"/Pages 2 0 R\n");
    pdf_data.extend_from_slice(b">>\n");
    pdf_data.extend_from_slice(b"endobj\n");

    // Object 2: Pages (parent)
    object_offsets.push(pdf_data.len());
    pdf_data.extend_from_slice(b"2 0 obj\n");
    pdf_data.extend_from_slice(b"<<\n");
    pdf_data.extend_from_slice(b"/Type /Pages\n");

    // Build kids array
    let mut kids = String::from("/Kids [");
    for i in 0..pages.len() {
        kids.push_str(&format!("{} 0 R ", first_page_obj_num + i * 2));
    }
    kids.push_str("]\n");
    pdf_data.extend_from_slice(kids.as_bytes());

    pdf_data.extend_from_slice(format!("/Count {}\n", pages.len()).as_bytes());
    pdf_data.extend_from_slice(b">>\n");
    pdf_data.extend_from_slice(b"endobj\n");
    if has_ocr {
        // Add shared font objects before page objects so every page resource can
        // reference `/Focr 3 0 R`.
        append_glyphless_ocr_font_objects(&mut pdf_data, &mut object_offsets)?;
    }

    // For each page, create a Page object and an Image XObject
    for (page_idx, page) in pages.iter().enumerate() {
        eprintln!("Adding page {} to PDF...", page_idx + 1);

        // Convert pixels to points (1 point = 1/72 inch)
        let width_pts = (page.width as f32) / DPI * 72.0;
        let height_pts = (page.height as f32) / DPI * 72.0;

        // Page object
        let page_obj_num = first_page_obj_num + page_idx * 2;
        let image_obj_num = page_obj_num + 1;

        object_offsets.push(pdf_data.len());
        pdf_data.extend_from_slice(format!("{page_obj_num} 0 obj\n").as_bytes());
        pdf_data.extend_from_slice(b"<<\n");
        pdf_data.extend_from_slice(b"/Type /Page\n");
        pdf_data.extend_from_slice(b"/Parent 2 0 R\n");
        pdf_data.extend_from_slice(
            format!("/MediaBox [0 0 {width_pts:.2} {height_pts:.2}]\n").as_bytes(),
        );
        pdf_data.extend_from_slice(b"/Resources <<\n");
        pdf_data.extend_from_slice(
            format!("  /XObject << /Im{page_idx} {image_obj_num} 0 R >>\n").as_bytes(),
        );
        if has_ocr {
            // `/Focr` is the font name used inside OCR content streams.
            pdf_data.extend_from_slice(b"  /Font << /Focr 3 0 R >>\n");
        }
        pdf_data.extend_from_slice(b">>\n");

        // Reference to content stream object
        let first_content_obj_num = first_page_obj_num + pages.len() * 2;
        pdf_data.extend_from_slice(
            format!("/Contents {} 0 R\n", first_content_obj_num + page_idx).as_bytes(),
        );
        pdf_data.extend_from_slice(b">>\n");
        pdf_data.extend_from_slice(b"endobj\n");

        // Image XObject
        object_offsets.push(pdf_data.len());
        pdf_data.extend_from_slice(format!("{image_obj_num} 0 obj\n").as_bytes());
        pdf_data.extend_from_slice(b"<<\n");
        pdf_data.extend_from_slice(b"/Type /XObject\n");
        pdf_data.extend_from_slice(b"/Subtype /Image\n");
        pdf_data.extend_from_slice(format!("/Width {}\n", page.width).as_bytes());
        pdf_data.extend_from_slice(format!("/Height {}\n", page.height).as_bytes());
        pdf_data.extend_from_slice(b"/ColorSpace /DeviceRGB\n");
        pdf_data.extend_from_slice(b"/BitsPerComponent 8\n");

        // Compress pixel data using Flate compression
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(&page.pixels)
            .context("Failed to compress pixel data")?;
        let compressed_pixels = encoder.finish().context("Failed to finish compression")?;

        pdf_data.extend_from_slice(b"/Filter /FlateDecode\n");
        pdf_data.extend_from_slice(format!("/Length {}\n", compressed_pixels.len()).as_bytes());
        pdf_data.extend_from_slice(b">>\n");
        pdf_data.extend_from_slice(b"stream\n");
        pdf_data.extend_from_slice(&compressed_pixels);
        pdf_data.extend_from_slice(b"\nendstream\n");
        pdf_data.extend_from_slice(b"endobj\n");
    }

    // Content stream objects for each page
    for (page_idx, page) in pages.iter().enumerate() {
        let width_pts = (page.width as f32) / DPI * 72.0;
        let height_pts = (page.height as f32) / DPI * 72.0;
        let mut content =
            format!("q\n{width_pts:.2} 0 0 {height_pts:.2} 0 0 cm\n/Im{page_idx} Do\nQ\n");

        if let Some(ocr_page) = ocr_pages.and_then(|pages| pages.get(page_idx)) {
            // The glyphless font declares a default width of 500 units. In PDF
            // text space that is 0.5em, so this reciprocal factor is used when
            // calculating `Tz` to stretch each invisible word to its OCR baseline
            // length.
            const GLYPHLESS_CHAR_WIDTH: f32 = 2.0;

            for line in ocr_text_lines(ocr_page.words()) {
                let words = line
                    .words
                    .iter()
                    .filter(|word| !word.text.trim().is_empty())
                    .collect::<Vec<_>>();

                if words.is_empty() {
                    continue;
                }

                // One PDF text object per OCR line gives viewers a natural
                // selection order while still allowing per-word positioning.
                content.push_str("BT\n3 Tr\n");

                // Track the previous word's PDF-space position so later words
                // can use relative `Td` moves. This mirrors Tesseract and avoids
                // resetting the full matrix for every word.
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

                    // Use the line baseline as the coordinate system for all
                    // words on the line, then project each word's own baseline
                    // onto it to find the text start point.
                    let line_baseline = clip_baseline(word.line_vbaseline);
                    let (x_pts, y_pts, word_length_pts) =
                        word_baseline_position(word, line_baseline, height_pts);

                    // Prefer the OCR-reported point size. Some words do not have
                    // one, so fall back to a conservative value derived from the
                    // word box height.
                    let font_size = if word.font_size > 0 {
                        word.font_size as f32
                    } else {
                        (word.vbox.h as f32 * 72.0 / DPI * 0.75).max(1.0)
                    };

                    // `Tz` is horizontal scaling in percent. Scale the glyphless
                    // text so the selectable area spans the measured OCR baseline
                    // length instead of the font's nominal character width.
                    let horizontal_scale =
                        (GLYPHLESS_CHAR_WIDTH * 100.0 * word_length_pts.max(1.0)
                            / (font_size * char_count as f32))
                            .clamp(5.0, 300.0);

                    // Tesseract inserts spaces between words but not after the
                    // final word of a line. The OCR backend returns trimmed words,
                    // so the PDF layer has to add that spacing explicitly.
                    let pdf_word = if !word.last_in_line && word_idx + 1 < words.len() {
                        format!("{text} ")
                    } else {
                        text.to_string()
                    };
                    let text_hex = to_pdf_utf16be_hex(&pdf_word);
                    let (a, b, c, d) = affine_matrix(word.writing_direction, line_baseline);

                    if first_word || old_direction != Some(word.writing_direction) {
                        // `Tm` sets the full text matrix. Use it for the first
                        // word and when direction changes because `Td` can only
                        // move within the current text coordinate system.
                        content.push_str(&format!(
                            "{a:.3} {b:.3} {c:.3} {d:.3} {x_pts:.2} {y_pts:.2} Tm\n/Focr {font_size:.2} Tf\n"
                        ));
                        first_word = false;
                    } else {
                        // Convert the PDF-space movement back into the current
                        // text-space basis before emitting `Td`. This keeps
                        // relative moves correct for skewed/rotated baselines.
                        let dx = x_pts - old_x;
                        let dy = y_pts - old_y;
                        let text_dx = dx * a + dy * b;
                        let text_dy = dx * c + dy * d;
                        content.push_str(&format!(
                            "{text_dx:.2} {text_dy:.2} Td\n/Focr {font_size:.2} Tf\n"
                        ));
                    }

                    // `TJ` accepts a text array. We use one hex string per word;
                    // the preceding `Tz` makes that word's selection geometry
                    // match the OCR baseline length.
                    content.push_str(&format!("{horizontal_scale:.2} Tz\n[ <{text_hex}> ] TJ\n"));
                    old_x = x_pts;
                    old_y = y_pts;
                    old_direction = Some(word.writing_direction);
                }

                content.push_str("ET\n");
            }
        }

        let first_content_obj_num = first_page_obj_num + pages.len() * 2;
        let content_obj_num = first_content_obj_num + page_idx;
        object_offsets.push(pdf_data.len());
        pdf_data.extend_from_slice(format!("{content_obj_num} 0 obj\n").as_bytes());
        pdf_data.extend_from_slice(b"<<\n");
        pdf_data.extend_from_slice(format!("/Length {}\n", content.len()).as_bytes());
        pdf_data.extend_from_slice(b">>\n");
        pdf_data.extend_from_slice(b"stream\n");
        pdf_data.extend_from_slice(content.as_bytes());
        pdf_data.extend_from_slice(b"\nendstream\n");
        pdf_data.extend_from_slice(b"endobj\n");
    }

    // Cross-reference table
    let xref_offset = pdf_data.len();
    let num_objects = object_offsets.len();
    pdf_data.extend_from_slice(b"xref\n");
    pdf_data.extend_from_slice(format!("0 {}\n", num_objects + 1).as_bytes());
    pdf_data.extend_from_slice(b"0000000000 65535 f \n");
    for offset in &object_offsets {
        pdf_data.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }

    // Trailer
    pdf_data.extend_from_slice(b"trailer\n");
    pdf_data.extend_from_slice(b"<<\n");
    pdf_data.extend_from_slice(format!("/Size {}\n", num_objects + 1).as_bytes());
    pdf_data.extend_from_slice(b"/Root 1 0 R\n");
    pdf_data.extend_from_slice(b">>\n");
    pdf_data.extend_from_slice(b"startxref\n");
    pdf_data.extend_from_slice(format!("{xref_offset}\n").as_bytes());
    pdf_data.extend_from_slice(b"%%EOF\n");

    writer
        .write_all(&pdf_data)
        .context("Failed to write PDF data")?;
    Ok(())
}

/// Apply OCR to add text layer to PDF (platform-aware)
pub fn apply_ocr_fn(input_pdf: String, output_pdf: String) -> Result<()> {
    eprintln!("Applying OCR to PDF...");

    // On macOS, try using PDFKit's saveTextFromOCROption first
    #[cfg(target_os = "macos")]
    {
        match apply_ocr_macos(&input_pdf, &output_pdf) {
            Ok(()) => return Ok(()),
            Err(e) => {
                eprintln!(
                    "Warning: macOS PDFKit OCR failed: {stderr_sanitized}",
                    stderr_sanitized = replace_control_chars(&e.to_string(), true)
                );
                eprintln!("Falling back to ocrmypdf...");
            }
        }
    }

    // Fall back to ocrmypdf (for non-macOS or if PDFKit fails)
    let output = Command::new("ocrmypdf")
        .args([&input_pdf, &output_pdf])
        .output();

    match output {
        Ok(result) if result.status.success() => {
            eprintln!("OCR applied successfully");
            Ok(())
        }
        Ok(result) => {
            let stderr = String::from_utf8_lossy(&result.stderr);
            eprintln!(
                "Warning: OCR failed: {stderr_sanitized}",
                stderr_sanitized = replace_control_chars(&stderr, true)
            );
            eprintln!("Falling back to PDF without OCR");
            std::fs::copy(&input_pdf, &output_pdf).context("Failed to copy PDF")?;
            Ok(())
        }
        Err(e) => {
            eprintln!("Warning: ocrmypdf not found or failed: {e}");
            eprintln!("Falling back to PDF without OCR");
            eprintln!("To enable OCR, install ocrmypdf: pip install ocrmypdf");
            std::fs::copy(&input_pdf, &output_pdf).context("Failed to copy PDF")?;
            Ok(())
        }
    }
}

#[cfg(target_os = "macos")]
fn apply_ocr_macos(input_pdf: &str, output_pdf: &str) -> Result<()> {
    eprintln!("Using macOS PDFKit for OCR...");

    let script_path = if let Ok(exe_path) = std::env::current_exe() {
        let mut path = exe_path.parent().unwrap().to_path_buf();
        path.push("macos_ocr.swift");
        if path.exists() {
            path
        } else {
            std::path::PathBuf::from("src/macos_ocr.swift")
        }
    } else {
        std::path::PathBuf::from("src/macos_ocr.swift")
    };

    if !script_path.exists() {
        anyhow::bail!("macOS OCR script not found at {:?}", script_path);
    }

    let input_absolute = std::fs::canonicalize(input_pdf).with_context(|| {
        format!(
            "Failed to get absolute path for input: {input_pdf_sanitized}",
            input_pdf_sanitized = replace_control_chars(input_pdf, false)
        )
    })?;
    let output_absolute = std::path::Path::new(output_pdf)
        .canonicalize()
        .unwrap_or_else(|_| {
            let output_path = std::path::Path::new(output_pdf);
            if output_path.is_absolute() {
                output_path.to_path_buf()
            } else {
                std::env::current_dir().unwrap().join(output_path)
            }
        });

    let output = Command::new("swift")
        .arg(&script_path)
        .arg(&input_absolute)
        .arg(&output_absolute)
        .output()
        .context("Failed to execute Swift OCR script")?;

    if output.status.success() {
        eprintln!("OCR applied successfully using macOS PDFKit");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Swift OCR script failed: {stderr_sanitized}",
            stderr_sanitized = replace_control_chars(&stderr, true)
        )
    }
}

/// Python bindings module
/// Re-exports from the python module to make them available to PyO3
#[cfg(feature = "python")]
pub mod python;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_page_size_calculation() {
        let width_pixels = 1500u16;
        let height_pixels = 2000u16;
        let dpi = 150.0f32;

        let width_mm = (width_pixels as f32) / dpi * 25.4;
        let height_mm = (height_pixels as f32) / dpi * 25.4;

        assert_eq!(width_mm, 254.0);
        assert_eq!(height_mm, 338.66666);
    }

    #[test]
    fn test_pixel_data_parsing() {
        let mut data = Vec::new();

        let page_count: u16 = 1;
        data.extend_from_slice(&page_count.to_be_bytes());

        let width: u16 = 100;
        let height: u16 = 50;
        data.extend_from_slice(&width.to_be_bytes());
        data.extend_from_slice(&height.to_be_bytes());

        let num_pixels = (width as usize) * (height as usize) * 3;
        data.extend(vec![128u8; num_pixels]);

        let result = parse_pixel_data(data);
        assert!(result.is_ok());

        let pages = result.unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].width, width);
        assert_eq!(pages[0].height, height);
        assert_eq!(pages[0].pixels.len(), num_pixels);
    }

    #[test]
    fn test_pdf_generation() {
        use std::io::Cursor;

        let width = 10u16;
        let height = 10u16;
        let mut pixels = Vec::new();

        for _ in 0..(width * height) {
            pixels.push(255);
            pixels.push(0);
            pixels.push(0);
        }

        let page = PageData {
            width,
            height,
            pixels,
        };
        let pages = vec![page];

        let mut buffer = Cursor::new(Vec::new());
        let result = write_pdf(buffer.get_mut(), &pages, None);
        assert!(result.is_ok(), "PDF generation should succeed");

        let pdf_data = buffer.into_inner();
        assert!(!pdf_data.is_empty(), "PDF should have data");

        let header = String::from_utf8_lossy(&pdf_data[0..9]);
        assert!(
            header.starts_with("%PDF-1.4"),
            "PDF should have correct header"
        );

        let trailer = String::from_utf8_lossy(&pdf_data);
        assert!(trailer.contains("%%EOF"), "PDF should have EOF marker");
        assert!(
            trailer.contains("/Type /Catalog"),
            "PDF should have catalog"
        );
        assert!(trailer.contains("/Type /Pages"), "PDF should have pages");
        assert!(
            trailer.contains("/Type /Page"),
            "PDF should have page object"
        );
        assert!(
            trailer.contains("/Type /XObject"),
            "PDF should have image object"
        );

        assert!(
            trailer.contains("/Filter /FlateDecode"),
            "PDF should use Flate compression for images"
        );
    }

    #[test]
    fn test_pdf_compression_reduces_size() {
        use std::io::Cursor;

        let width = 100u16;
        let height = 100u16;
        let mut pixels = Vec::new();

        for _ in 0..(width * height) {
            pixels.push(255);
            pixels.push(0);
            pixels.push(0);
        }

        let page = PageData {
            width,
            height,
            pixels: pixels.clone(),
        };
        let pages = vec![page];

        let mut buffer = Cursor::new(Vec::new());
        let result = write_pdf(buffer.get_mut(), &pages, None);
        assert!(result.is_ok(), "PDF generation should succeed");

        let pdf_data = buffer.into_inner();

        let uncompressed_pixel_size = pixels.len();
        assert_eq!(uncompressed_pixel_size, 30000);

        let estimated_uncompressed_pdf_size = uncompressed_pixel_size + 1000;

        eprintln!("PDF size with compression: {} bytes", pdf_data.len());
        eprintln!("Estimated uncompressed size: {estimated_uncompressed_pdf_size} bytes");
        eprintln!(
            "Compression ratio: {:.2}%",
            (pdf_data.len() as f32 / estimated_uncompressed_pdf_size as f32) * 100.0
        );

        assert!(
            pdf_data.len() < estimated_uncompressed_pdf_size / 2,
            "PDF with compression should be significantly smaller than uncompressed"
        );
    }

    #[test]
    fn test_pdf_generation_with_hidden_ocr_text() {
        use std::io::Cursor;

        let width = 10u16;
        let height = 10u16;
        let page = PageData {
            width,
            height,
            pixels: vec![255; width as usize * height as usize * 3],
        };
        let pages = vec![page];
        let ocr_pages = vec![ocr::OcrPage::from_test_words(vec![(
            "hello (pdf)",
            1,
            2,
            3,
            4,
        )])];

        let mut buffer = Cursor::new(Vec::new());
        let result = write_pdf(buffer.get_mut(), &pages, Some(&ocr_pages));
        assert!(result.is_ok(), "PDF generation with OCR should succeed");

        let pdf_data = String::from_utf8_lossy(&buffer.into_inner()).into_owned();
        assert!(
            pdf_data.contains("/Font << /Focr"),
            "PDF should include OCR font resource"
        );
        assert!(
            pdf_data.contains("/GlyphLessFont"),
            "PDF should include the glyphless OCR font"
        );
        assert!(
            pdf_data.contains("/FontFile2 8 0 R"),
            "PDF should embed the glyphless TrueType font"
        );
        assert!(
            pdf_data.contains("/Encoding /Identity-H"),
            "PDF should use Identity-H encoding for OCR text"
        );
        assert!(
            pdf_data.contains("3 Tr"),
            "PDF should use invisible text rendering mode"
        );
        assert!(
            pdf_data.contains("Tz"),
            "PDF should horizontally scale OCR text"
        );
        assert!(
            pdf_data.contains("[ <00680065006C006C006F002000280070006400660029> ] TJ"),
            "PDF should contain UTF-16BE hex OCR text in a TJ array"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_macos_ocr_function_compiles() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut temp_input = NamedTempFile::new().unwrap();
        temp_input.write_all(b"%PDF-1.4\n%%EOF\n").unwrap();
        let input_path = temp_input.path().to_str().unwrap();

        let temp_output = NamedTempFile::new().unwrap();
        let output_path = temp_output.path().to_str().unwrap();

        let result = apply_ocr_macos(input_path, output_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_forward_sanitized_text() {
        let input = concat!(
            "plain ✓ café 😀\n",
            "\x1b[31mANSI escaped\n",
            "red text.\x1b[0m\n",
            "\ttab\rline\n",
            "a\u{200E}b\u{E000}c\u{0378}d\u{2028}e\u{2029}f\n",
            "x\n",
            "\u{2028}\u{2029}y\n",
            "ok line\n",
            "\x1b[31mred\x1b[0m\n",
            "end",
        );
        let expected_output = concat!(
            "UNTRUSTED> plain ✓ café 😀\n",
            "UNTRUSTED> \u{FFFD}[31mANSI escaped\n",
            "UNTRUSTED> red text.\u{FFFD}[0m\n",
            "UNTRUSTED> \u{FFFD}tab\u{FFFD}line\n",
            "UNTRUSTED> a\u{FFFD}b\u{FFFD}c\u{FFFD}d\u{FFFD}e\u{FFFD}f\n",
            "UNTRUSTED> x\n",
            "UNTRUSTED> \u{FFFD}\u{FFFD}y\n",
            "UNTRUSTED> ok line\n",
            "UNTRUSTED> \u{FFFD}[31mred\u{FFFD}[0m\n",
            "UNTRUSTED> end",
        );

        let reader = BufReader::new(std::io::Cursor::new(input.as_bytes()));
        let out = tempfile::NamedTempFile::new().unwrap();
        let out_path = out.path().to_path_buf();
        let out_file = out.reopen().unwrap();

        forward_sanitized_text(reader, out_file).unwrap();

        let output_bytes = std::fs::read(out_path).unwrap();
        let output = String::from_utf8(output_bytes).unwrap();
        assert_eq!(
            output, expected_output,
            "forward_sanitized_text failed for input: {input:?}",
        );
    }
}
