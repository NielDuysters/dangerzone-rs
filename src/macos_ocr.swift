#!/usr/bin/env swift

import Foundation
import PDFKit
import Quartz

// Usage: swift macos_ocr.swift <input_pdf> <output_pdf>

guard CommandLine.arguments.count == 3 else {
    print("Usage: \(CommandLine.arguments[0]) <input_pdf> <output_pdf>")
    exit(1)
}

let inputPath = CommandLine.arguments[1]
let outputPath = CommandLine.arguments[2]

// Load the PDF
guard let inputURL = URL(string: "file://\(inputPath)"),
      let document = PDFDocument(url: inputURL) else {
    print("Error: Failed to load PDF from \(inputPath)")
    exit(1)
}

// Check if saveTextFromOCR option is available (macOS 10.15+)
if #available(macOS 10.15, *) {
    // Use PDFKit's OCR capability
    let outputURL = URL(fileURLWithPath: outputPath)
    
    // Write the PDF with OCR text extraction option
    let success = document.write(to: outputURL, withOptions: [PDFDocumentWriteOption.saveTextFromOCROption: true])
    
    if success {
        print("OCR applied successfully using PDFKit")
        exit(0)
    } else {
        print("Error: Failed to write PDF with OCR to \(outputPath)")
        exit(1)
    }
} else {
    print("Error: saveTextFromOCROption requires macOS 10.15 or later")
    exit(1)
}
