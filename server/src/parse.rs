//! Local document → Markdown, never refusing on images (parsers lab: xberg without OCR scored
//! 17/17 recall on PDF/DOCX/PPTX/XLSX incl. a PDF with a scanned page; anydoc refused that PDF,
//! anydoc+pdf-inspector returned nothing for it). Images are simply skipped.

use std::path::Path;

const PLAIN: &[&str] = &["txt", "md", "markdown", "csv", "tsv", "json", "yaml", "yml", "toml", "log", "rs", "py", "js", "ts", "html", "htm", "xml", "ini", "sh"];

pub async fn to_markdown(path: &Path) -> anyhow::Result<String> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if PLAIN.contains(&ext.as_str()) {
        let bytes = tokio::fs::read(path).await?;
        return Ok(String::from_utf8_lossy(&bytes).into_owned());
    }
    let input = xberg::ExtractInput::from_uri(path.display().to_string());
    let r = xberg::extract(input, &xberg::ExtractionConfig::default()).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    anyhow::ensure!(!r.results.is_empty(), "could not parse {}: {:?}", path.display(), r.errors.first().map(|e| format!("{e:?}")));
    Ok(crate::browser::extract::clean_markdown(&r.results.iter().map(|d| d.content.as_str()).collect::<Vec<_>>().join("\n\n")))
}
