use std::fs;
use std::path::Path;
use std::process::Command;

fn run_conversion(input: &str, output: &str) -> Result<(), Box<dyn std::error::Error>> {
    let status = Command::new("cargo")
        .args(&["run", "--", "--input", input, "--output", output])
        .status()?;

    if !status.success() {
        return Err(format!("Conversion failed for {}", input).into());
    }

    Ok(())
}

fn compare_pdf_size(generated: &str, reference: &str) -> Result<(), Box<dyn std::error::Error>> {
    let gen_metadata = fs::metadata(generated)?;
    let ref_metadata = fs::metadata(reference)?;

    let gen_size = gen_metadata.len();
    let ref_size = ref_metadata.len();

    let size_diff_percent = ((gen_size as f64 - ref_size as f64) / ref_size as f64).abs() * 100.0;

    if size_diff_percent > 50.0 {
        return Err(format!(
            "PDF size differs too much: generated={}, reference={}, diff={}%",
            gen_size, ref_size, size_diff_percent
        )
        .into());
    }

    Ok(())
}

#[test]
#[ignore] // Requires podman and dangerzone image
fn test_docx_conversion() -> Result<(), Box<dyn std::error::Error>> {
    let input = "tests/sample-docx.docx";
    let output = "/tmp/test-output-docx.pdf";
    let reference = "tests/reference/sample-docx.pdf";

    if !Path::new(input).exists() {
        return Err(format!("Test file not found: {}", input).into());
    }

    run_conversion(input, output)?;

    assert!(Path::new(output).exists(), "Output PDF was not created");

    if Path::new(reference).exists() {
        compare_pdf_size(output, reference)?;
    }

    fs::remove_file(output)?;
    Ok(())
}

#[test]
#[ignore] // Requires podman and dangerzone image
fn test_pdf_conversion() -> Result<(), Box<dyn std::error::Error>> {
    let input = "tests/sample-pdf.pdf";
    let output = "/tmp/test-output-pdf.pdf";
    let reference = "tests/reference/sample-pdf.pdf";

    if !Path::new(input).exists() {
        return Err(format!("Test file not found: {}", input).into());
    }

    run_conversion(input, output)?;

    assert!(Path::new(output).exists(), "Output PDF was not created");

    if Path::new(reference).exists() {
        compare_pdf_size(output, reference)?;
    }

    fs::remove_file(output)?;
    Ok(())
}

#[test]
#[ignore] // Requires podman and dangerzone image
fn test_jpg_conversion() -> Result<(), Box<dyn std::error::Error>> {
    let input = "tests/sample-jpg.jpg";
    let output = "/tmp/test-output-jpg.pdf";
    let reference = "tests/reference/sample-jpg.pdf";

    if !Path::new(input).exists() {
        return Err(format!("Test file not found: {}", input).into());
    }

    run_conversion(input, output)?;

    assert!(Path::new(output).exists(), "Output PDF was not created");

    if Path::new(reference).exists() {
        compare_pdf_size(output, reference)?;
    }

    fs::remove_file(output)?;
    Ok(())
}
