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
// Tesseract's PDF renderer embeds a tiny TrueType font named "GlyphLessFont"
// for OCR text layers. It intentionally does not contain visible glyph shapes
// for each character. Instead, every CID maps to the same blank glyph and a
// ToUnicode CMap tells PDF viewers which Unicode text each CID represents.
//
// That is exactly what we want for Dangerzone's OCR layer: the rasterized page
// image stays the only visible page content, while the hidden text remains
// searchable, selectable, and copyable. Using the same `pdf.ttf` as Tesseract
// also avoids depending on viewer-specific fallback behavior for an unembedded
// font.
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

/// Convert pixel data to a PDF file and add the provided OCR text layer.
///
/// The OCR step happens before this function is called. That means this writer
/// receives two aligned slices:
///
/// - `pages`: the raster page images produced by the untrusted container;
/// - `ocr_pages`: the trusted-side OCR metadata extracted from those images.
///
/// Keeping OCR data in memory lets the Linux path avoid writing an intermediate
/// PDF and then reopening it with an external OCR tool. The final PDF is built
/// once, with each page image and its matching hidden text stream emitted
/// together.
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

    // On Linux, use the integrated OCR backend directly on the RGB page buffers
    // we just received from the container. The old platform-neutral fallback
    // writes a temporary PDF and asks an external tool to add OCR later; this
    // path keeps the pipeline closer to Dangerzone's Python implementation:
    // render pages to pixels, OCR those pixels, then construct one final PDF.
    //
    // The backend selection is intentionally still small and explicit. When
    // Apple Vision or Windows OCR backends exist, this is the point where
    // conditional compilation can choose the right implementation without
    // changing the PDF writer's `OcrPage` input format.
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

fn to_pdf_utf16be_hex(text: &str) -> String {
    // The OCR font uses Identity-H encoding and the ToUnicode map below maps
    // 16-bit CIDs directly back to Unicode code points. Emitting text as
    // UTF-16BE hex therefore gives PDF viewers enough information to copy the
    // original text, including characters that are awkward or unsafe in PDF
    // literal strings such as parentheses, backslashes, and newlines.
    let mut out = String::with_capacity(text.len() * 4);
    for unit in text.encode_utf16() {
        out.push_str(&format!("{unit:04X}"));
    }
    out
}

struct OcrTextLine<'a> {
    // Borrowed OCR words that Tesseract reported as belonging to one text line.
    // We keep references instead of copying `OcrText` because the line object
    // is only a temporary view used while building the page content stream.
    words: Vec<&'a ocr::OcrText>,
    // Union bounding box for the line in source-image pixels. These values are
    // mainly useful for diagnostics and future layout work; the current PDF
    // writer positions text from Tesseract baselines instead.
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

fn ocr_text_lines(words: &[ocr::OcrText]) -> Vec<OcrTextLine<'_>> {
    // Tesseract has already done the hard work of assigning each word to a
    // block and text line. Preserve that grouping instead of recomputing lines
    // from Y coordinates. Re-grouping by geometry was one of the earlier causes
    // of unstable selection because nearby lines or columns could be merged
    // accidentally.
    let mut lines = Vec::new();
    let mut current: Option<OcrTextLine<'_>> = None;

    for word in words.iter().filter(|word| word.w() > 0 && word.h() > 0) {
        match &mut current {
            Some(line)
                if line.words.last().is_some_and(|last| {
                    last.block_id() == word.block_id() && last.line_id() == word.line_id()
                }) =>
            {
                // Extend the line's union box so tests and future callers have
                // a cheap summary of the visual extent of this OCR line.
                let right = line.x + line.w;
                let bottom = line.y + line.h;
                let word_right = word.x() + word.w();
                let word_bottom = word.y() + word.h();

                line.x = line.x.min(word.x());
                line.y = line.y.min(word.y());
                line.w = word_right.max(right) - line.x;
                line.h = word_bottom.max(bottom) - line.y;
                line.words.push(word);
            }
            Some(line) => {
                sort_ocr_line_words(&mut line.words);
                lines.push(current.take().expect("line exists"));
                current = Some(OcrTextLine {
                    words: vec![word],
                    x: word.x(),
                    y: word.y(),
                    w: word.w(),
                    h: word.h(),
                });
            }
            None => {
                current = Some(OcrTextLine {
                    words: vec![word],
                    x: word.x(),
                    y: word.y(),
                    w: word.w(),
                    h: word.h(),
                });
            }
        }
    }

    if let Some(mut line) = current {
        sort_ocr_line_words(&mut line.words);
        lines.push(line);
    }

    lines
}

fn sort_ocr_line_words(words: &mut [&ocr::OcrText]) {
    // Tesseract's iterator order is usually already usable, but sorting inside
    // a line makes the emitted PDF text stream deterministic. The sort key
    // follows writing direction: left-to-right lines advance from low X to high
    // X, while right-to-left lines advance from high X to low X.
    if words
        .first()
        .is_some_and(|word| word.writing_direction() == ocr::OcrWritingDirection::RightToLeft)
    {
        words.sort_by_key(|word| std::cmp::Reverse(word.x()));
    } else {
        words.sort_by_key(|word| word.x());
    }
}

fn dist2(x1: i32, y1: i32, x2: i32, y2: i32) -> f32 {
    // Squared distance is enough for comparisons and avoids an unnecessary
    // square root at call sites that only need relative length.
    let dx = (x2 - x1) as f32;
    let dy = (y2 - y1) as f32;
    dx * dx + dy * dy
}

fn clip_baseline(baseline: ocr::OcrBaseline) -> ocr::OcrBaseline {
    // This mirrors a small correction in Tesseract's PDF renderer. When the
    // baseline is nearly horizontal and long enough, flatten it to one average
    // Y value. That avoids tiny OCR baseline noise turning into a visibly
    // rotated PDF text matrix, which can make viewer selection feel jumpy.
    let mut y1 = baseline.y1();
    let mut y2 = baseline.y2();
    let rise = (y2 - y1).abs() as f32 * 72.0;
    let run = (baseline.x2() - baseline.x1()).abs() as f32 * 72.0;

    if rise < 2.0 * DPI && 2.0 * DPI < run {
        let y = (y1 + y2) / 2;
        y1 = y;
        y2 = y;
    }

    ocr::OcrBaseline::new(baseline.x1(), y1, baseline.x2(), y2)
}

fn word_baseline_position(
    word: &ocr::OcrText,
    line_baseline: ocr::OcrBaseline,
    page_height_pts: f32,
) -> (f32, f32, f32) {
    // Calculate where this word starts on the line baseline.
    //
    // Tesseract's PDF renderer does not place each word at the top-left of its
    // bounding box. Instead, it projects the word baseline onto the containing
    // line baseline and uses that projected point as the PDF text position. The
    // result is much closer to the geometry PDF viewers expect for text
    // selection, especially when a page is slightly skewed.
    let mut word_baseline = word.baseline();
    if word.writing_direction() == ocr::OcrWritingDirection::RightToLeft {
        // For right-to-left text, the start of the word is visually on the
        // opposite end of the baseline. Flip the endpoints before projection so
        // the returned position matches the direction-specific text matrix.
        word_baseline = ocr::OcrBaseline::new(
            word_baseline.x2(),
            word_baseline.y2(),
            word_baseline.x1(),
            word_baseline.y1(),
        );
    }

    let line_length_squared = dist2(
        line_baseline.x1(),
        line_baseline.y1(),
        line_baseline.x2(),
        line_baseline.y2(),
    );
    let (x, y) = if line_length_squared == 0.0 {
        // Degenerate baseline: fall back to the first baseline point. This is
        // rare, but it avoids division by zero and still emits usable text.
        (line_baseline.x1() as f32, line_baseline.y1() as f32)
    } else {
        // Project the word's starting point onto the line baseline. The formula
        // is written in the same "from line end backwards" shape as Tesseract's
        // renderer so future comparisons to `pdfrenderer.cpp` stay readable.
        let t = ((word_baseline.x1() - line_baseline.x2()) as f32
            * (line_baseline.x2() - line_baseline.x1()) as f32
            + (word_baseline.y1() - line_baseline.y2()) as f32
                * (line_baseline.y2() - line_baseline.y1()) as f32)
            / line_length_squared;
        (
            line_baseline.x2() as f32 + t * (line_baseline.x2() - line_baseline.x1()) as f32,
            line_baseline.y2() as f32 + t * (line_baseline.y2() - line_baseline.y1()) as f32,
        )
    };

    // Use the OCR word baseline length as the target visual advance for the
    // hidden word. The later `Tz` operator stretches the glyphless text to this
    // length, so we do not need a Helvetica/Arial width estimate.
    let word_length = dist2(
        word_baseline.x1(),
        word_baseline.y1(),
        word_baseline.x2(),
        word_baseline.y2(),
    )
    .sqrt()
        * 72.0
        / DPI;

    (
        // Convert image pixels to PDF points. The source image is 150 DPI and
        // PDF points are 72 per inch.
        x * 72.0 / DPI,
        // Flip Y from image coordinates (top-left origin) to PDF coordinates
        // (bottom-left origin).
        page_height_pts - (y * 72.0 / DPI),
        word_length,
    )
}

fn affine_matrix(
    direction: ocr::OcrWritingDirection,
    line_baseline: ocr::OcrBaseline,
) -> (f32, f32, f32, f32) {
    // Build the 2x2 part of the PDF text matrix from the OCR line baseline.
    // This rotates hidden text onto the same angle Tesseract detected in the
    // raster image. For normal English text this is effectively identity, but
    // the matrix matters for scanned pages with small skew.
    let theta = ((line_baseline.y1() - line_baseline.y2()) as f32)
        .atan2((line_baseline.x2() - line_baseline.x1()) as f32);
    let mut a = theta.cos();
    let mut b = theta.sin();
    let c = -theta.sin();
    let d = theta.cos();

    if direction == ocr::OcrWritingDirection::RightToLeft {
        // Right-to-left text advances in the opposite horizontal direction.
        // Reflecting the matrix is the same approach used by Tesseract's PDF
        // renderer for directional text.
        a = -a;
        b = -b;
    }

    (a, b, c, d)
}

fn append_glyphless_ocr_font_objects(
    pdf_data: &mut Vec<u8>,
    object_offsets: &mut Vec<usize>,
) -> Result<()> {
    // The OCR font objects are shared by all pages and are only emitted when an
    // OCR layer is requested. Object numbers 3 through 8 are reserved for them;
    // page objects therefore start at object 9 in OCR PDFs.
    //
    // The structure below intentionally follows Tesseract's
    // `TessPDFRenderer::BeginDocumentHandler` closely:
    //
    // 3: Type0 composite font used from page resources as `/Focr`
    // 4: CIDFontType2 descendant font
    // 5: CIDToGIDMap stream mapping every CID to the same blank glyph
    // 6: ToUnicode CMap mapping CIDs back to Unicode for copy/paste/search
    // 7: FontDescriptor
    // 8: FontFile2 stream containing Tesseract's glyphless `pdf.ttf`

    // Object 3: Type0 composite font.
    //
    // `/Encoding /Identity-H` means the bytes in each text object are treated
    // as two-byte character IDs. `/ToUnicode 6 0 R` is what makes those IDs
    // copy back to real Unicode text instead of arbitrary glyph ids.
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

    // Object 4: CIDFontType2 descendant font.
    //
    // `/DW 500` declares a default glyph width of 500 font units. Because the
    // PDF text layer uses rendering mode 3, this width affects selection
    // geometry but never paints visible letters over the sanitized page image.
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

    // Object 5: CIDToGIDMap.
    //
    // A CIDFont maps character IDs to glyph IDs. Tesseract's glyphless font has
    // a single useful blank glyph, so every possible 16-bit CID maps to GID 1
    // (`00 01`). The full map is 2 * 65536 bytes and is compressed before being
    // embedded.
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

    // Object 6: ToUnicode CMap.
    //
    // This is the key piece for copy/paste and search. It tells the PDF viewer
    // that CID `<0041>` means Unicode U+0041, CID `<0065>` means U+0065, and so
    // on across the 16-bit range. Without this map, viewers can select hidden
    // text but may paste meaningless glyph ids.
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

    // Object 7: FontDescriptor.
    //
    // The metrics are intentionally simple and match the glyphless font. The
    // descriptor also points at object 8 via `/FontFile2`, which embeds the
    // actual TrueType program instead of relying on a system font.
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

    // Object 8: FontFile2.
    //
    // `/FontFile2` is the PDF name for an embedded TrueType font stream. The
    // bytes come from Tesseract's `pdf.ttf`, a tiny glyphless font designed
    // specifically for hidden OCR text layers.
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

/// Write a minimal PDF file with embedded RGB pixel data
fn write_pdf<W: Write>(
    writer: &mut W,
    pages: &[PageData],
    ocr_pages: Option<&[ocr::OcrPage]>,
) -> Result<()> {
    // `ocr_pages` is optional because this function is the single PDF writer
    // for both paths:
    //
    // - without OCR: write only the raster page images;
    // - with OCR: write the same raster page images plus an invisible text
    //   layer in each page content stream.
    //
    // Keeping one writer avoids two almost-identical PDF generation paths that
    // could drift apart. When OCR is provided, it must have exactly one
    // `OcrPage` per raster page so page N's hidden text is never accidentally
    // written onto page M.
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

    let has_ocr = ocr_pages.is_some();
    // In PDFs without OCR, object 3 is the first page object. In PDFs with OCR,
    // object numbers 3-8 are reserved for the shared glyphless font objects, so
    // the first page object moves to 9. The rest of the page/image/content
    // numbering is derived from this one value.
    let first_page_obj_num = if has_ocr { 9 } else { 3 };

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
        if ocr_pages.is_some() {
            // Hidden OCR text font points to the shared Type0 glyphless font
            // object. Page content streams refer to it as `/Focr`.
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
            // Tesseract's glyphless font has a nominal width of 500 units.
            // At PDF text scale, 500 units is 0.5em, so the unscaled advance is
            // roughly `font_size * 0.5` points per character. The equation
            // below uses the reciprocal as a multiplier and then applies `Tz`
            // so each invisible word spans Tesseract's measured baseline
            // length. This replaces the earlier real-font width estimate and
            // is the important part of making selection geometry stable.
            const GLYPHLESS_CHAR_WIDTH: f32 = 2.0;

            for line in ocr_text_lines(ocr_page.words()) {
                // Remove blank OCR items before creating a PDF text object.
                // This keeps the text stream compact and prevents empty `TJ`
                // arrays from affecting viewer selection behavior.
                let words = line
                    .words
                    .iter()
                    .filter(|word| !word.text().trim().is_empty())
                    .collect::<Vec<_>>();

                if words.is_empty() {
                    continue;
                }

                // Start one PDF text object per OCR line.
                //
                // `BT`/`ET` bracket text operations. `3 Tr` sets rendering
                // mode 3, which means "do not paint glyphs". The text still
                // exists for search, selection, accessibility extraction, and
                // copy/paste, but it does not visually alter the sanitized page
                // image.
                content.push_str("BT\n3 Tr\n");

                // Track the previous word's PDF-space position. Tesseract's
                // renderer emits the first word with `Tm` and later words with
                // relative `Td` moves. Doing the same keeps all words in one
                // line-level text object, which gives PDF viewers a much more
                // natural selection order than many independent text objects.
                let mut old_x = 0.0;
                let mut old_y = 0.0;
                let mut old_direction = None;
                let mut first_word = true;
                for (word_idx, word) in words.iter().enumerate() {
                    let text = word.text().trim();
                    let char_count = text.chars().count();
                    if char_count == 0 {
                        continue;
                    }

                    // Use the OCR line baseline, not the bounding box corner,
                    // to position the hidden text. This is the same principle
                    // Tesseract's `GetPDFTextObjects` uses and is what fixed
                    // the selection jumping caused by approximate font metrics.
                    let line_baseline = clip_baseline(word.line_baseline());
                    let (x_pts, y_pts, word_length_pts) =
                        word_baseline_position(word, line_baseline, height_pts);
                    // Prefer Tesseract's point size. If it is unavailable,
                    // derive a conservative size from the OCR word height so
                    // the text still has usable geometry.
                    let font_size = if word.font_size() > 0 {
                        word.font_size() as f32
                    } else {
                        (word.h() as f32 * 72.0 / DPI * 0.75).max(1.0)
                    };
                    // `Tz` is horizontal scaling in percent. The glyphless
                    // font gives each character a simple nominal advance; this
                    // scaling stretches or shrinks the word so the PDF viewer's
                    // selectable area lines up with Tesseract's word baseline.
                    // The clamp protects against pathological OCR boxes or
                    // font sizes producing unusably tiny/huge text matrices.
                    let horizontal_scale =
                        (GLYPHLESS_CHAR_WIDTH * 100.0 * word_length_pts.max(1.0)
                            / (font_size * char_count as f32))
                            .clamp(5.0, 300.0);
                    // Tesseract emits a space after words that are followed by
                    // another word on the same text line. We replicate that
                    // behavior explicitly because the OCR backend returns words
                    // without inter-word whitespace.
                    let pdf_word = if !word.is_final_in_line() && word_idx + 1 < words.len() {
                        format!("{text} ")
                    } else {
                        text.to_string()
                    };
                    // Hex UTF-16BE avoids PDF literal-string escaping and works
                    // with the Identity-H / ToUnicode font objects above.
                    let text_hex = to_pdf_utf16be_hex(&pdf_word);
                    // The text matrix contains rotation/skew and direction.
                    // Its translation is supplied by the baseline projection.
                    let (a, b, c, d) = affine_matrix(word.writing_direction(), line_baseline);

                    if first_word || old_direction != Some(word.writing_direction()) {
                        // `Tm` sets the full text matrix. Use it for the first
                        // word in a line and whenever writing direction changes,
                        // because a relative `Td` move cannot change direction.
                        content.push_str(&format!(
                            "{a:.3} {b:.3} {c:.3} {d:.3} {x_pts:.2} {y_pts:.2} Tm\n/Focr {font_size:.2} Tf\n"
                        ));
                        first_word = false;
                    } else {
                        // Later words in the same direction can be positioned
                        // with `Td`, a relative move in text space. Convert the
                        // PDF-space delta back through the 2x2 matrix so the
                        // relative move remains correct for skewed baselines.
                        let dx = x_pts - old_x;
                        let dy = y_pts - old_y;
                        let text_dx = dx * a + dy * b;
                        let text_dy = dx * c + dy * d;
                        content.push_str(&format!(
                            "{text_dx:.2} {text_dy:.2} Td\n/Focr {font_size:.2} Tf\n"
                        ));
                    }

                    // `TJ` shows a text array. We use a single hex string per
                    // word, matching Tesseract's renderer shape closely enough
                    // for smooth search, selection, and copy/paste in viewers.
                    content.push_str(&format!("{horizontal_scale:.2} Tz\n[ <{text_hex}> ] TJ\n"));
                    old_x = x_pts;
                    old_y = y_pts;
                    old_direction = Some(word.writing_direction());
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
            pdf_data.contains("/FontFile2 8 0 R"),
            "PDF should embed the glyphless TrueType font"
        );
        assert!(
            pdf_data.contains("3 Tr"),
            "PDF should use invisible text rendering mode"
        );
        assert!(
            pdf_data.contains("[ <00680065006C006C006F002000280070006400660029> ] TJ"),
            "PDF should contain UTF-16BE hex OCR text"
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
