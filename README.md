# dangerzone.rs

A command-line implementation of Dangerzone in Rust.

## Overview

This is a simple Rust implementation of Dangerzone that converts potentially dangerous documents (PDF, Office documents, etc.) into safe PDFs by rendering them to pixels and reconstructing a clean PDF.

## Features

- Uses the official Dangerzone Docker images from `ghcr.io/freedomofpress/dangerzone/v1`
- Uses podman for container runtime
- Streams documents through the conversion process
- Two-phase conversion: document → pixels → safe PDF
- Parses the binary pixel stream protocol
- Reconstructs PDF from pixel data using Rust PDF libraries

## Prerequisites

- Rust (for building)
- Podman installed and running
- The Dangerzone container image pulled:
  ```bash
  podman pull ghcr.io/freedomofpress/dangerzone/v1
  ```

## Building

```bash
cargo build --release
```

The binary will be available at `target/release/dangerzone-rs`.

## Usage

Basic usage:
```bash
dangerzone-rs --input unsafe.pdf --output safe.pdf
```

With OCR enabled:
```bash
dangerzone-rs --input unsafe.pdf --output safe.pdf --ocr
```

Or using cargo run:
```bash
cargo run -- --input unsafe.pdf --output safe.pdf
```

## OCR Support

The `--ocr` flag enables OCR (Optical Character Recognition) to add a searchable text layer to the output PDF. This requires `ocrmypdf` to be installed:

```bash
pip install ocrmypdf
```

If `ocrmypdf` is not available, the conversion will continue without OCR and produce a PDF without text layers.

## Supported Document Formats

The implementation supports all formats supported by the Dangerzone container:
- PDF documents (.pdf)
- Microsoft Office documents (.docx, .xlsx, .pptx, .doc, .xls, .ppt)
- OpenDocument formats (.odt, .ods, .odp, .odg)
- Image files (.jpg, .png, .gif, .bmp, .tiff, .svg)
- E-books (.epub)
- And more...

## Testing

Unit tests:
```bash
cargo test
```

Integration tests (requires podman, dangerzone image, and optionally pdftoppm):
```bash
# Pull the container image first
podman pull ghcr.io/freedomofpress/dangerzone/v1

# Install pdftoppm for pixel-by-pixel comparison (optional, falls back to size comparison)
# On Ubuntu/Debian: sudo apt-get install poppler-utils
# On macOS: brew install poppler

# Run all integration tests (tests all files in tests/ directory automatically)
cargo test --test integration_test -- --ignored

# Run single test
cargo test --test integration_test test_single_docx -- --ignored

# Or test a specific conversion
cargo run -- --input tests/sample-docx.docx --output /tmp/output.pdf
```

### Test Features

The integration tests automatically:
- Discover all test files in the `tests/` directory
- Determine expected behavior based on filename (`sample_bad_*` files are expected to fail)
- Compare converted PDFs with reference outputs using pixel-by-pixel comparison (requires `pdftoppm`)
- Fall back to file size comparison if `pdftoppm` is not available
- Provide detailed pass/fail reporting for each test case

Test naming conventions:
- `sample-*.ext`: Expected to convert successfully
- `sample_bad_*.ext`: Expected to fail conversion

## How it works

1. **Document to Pixels**: The input document is streamed to stdin of a sandboxed podman container that converts it to pixel data
2. **Parse Pixel Stream**: The binary output stream is parsed according to the Dangerzone protocol:
   - Page count (2 bytes, big-endian)
   - For each page: width (2 bytes), height (2 bytes), RGB pixel data
3. **Pixels to PDF**: The pixel data is converted to a safe PDF using Rust PDF libraries

All conversions happen with strict security settings following the Dangerzone security model.

## Security Features

The implementation uses the same security flags as the official Dangerzone:
- `--security-opt no-new-privileges`: Prevents privilege escalation
- `--cap-drop all --cap-add SYS_CHROOT`: Minimal capabilities
- `--network=none`: No network access
- `-u dangerzone`: Run as unprivileged user
- `--rm`: Automatically remove containers after use
- `--log-driver none`: Don't log container output

## Implementation Details

This is a minimal implementation that demonstrates the core Dangerzone workflow:
- Uses the container for the untrusted document-to-pixels conversion
- Implements the binary I/O protocol for receiving pixel data
- Converts pixels back to PDF using the `printpdf` crate

## References

- [Dangerzone Project](https://github.com/freedomofpress/dangerzone)
- [Container Security Flags](https://github.com/freedomofpress/dangerzone/blob/main/dangerzone/isolation_provider/container.py)
- [Binary Protocol](https://github.com/freedomofpress/dangerzone/blob/main/dangerzone/conversion/common.py)