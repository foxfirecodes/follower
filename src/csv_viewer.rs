//! A self-contained page for browsing a query CSV: search, filters, and grouping by item,
//! callsite, call file, or argument, and the item outcomes of `follower items`. The page holds
//! only the CSV and what is derived from it, so it shows exactly what an agent reading the CSV
//! sees.

use std::{
    fs::OpenOptions,
    io::{BufWriter, Write},
    path::Path,
};

use anyhow::{Context, Result};

/// Writes the viewer for a CSV to `output`, readable only by its owner, since the CSV holds
/// source paths and values.
pub fn write_csv_viewer(csv: &str, title: &str, output: &Path) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(output)
        .with_context(|| format!("failed to create {}", output.display()))?;
    let mut out = BufWriter::new(file);
    out.write_all(page(csv, title)?.as_bytes())?;
    out.flush()?;
    Ok(())
}

fn page(csv: &str, title: &str) -> Result<String> {
    // JSON strings with `<` escaped cannot close the script elements that hold them.
    let embed = |value: &str| -> Result<String> {
        Ok(serde_json::to_string(value)?.replace('<', "\\u003c"))
    };
    Ok(format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'\"><title>Follower callsite table</title><style>{}</style></head><body><div id=\"app\"></div><script id=\"csv-title\" type=\"application/json\">{}</script><script id=\"csv-data\" type=\"application/json\">{}</script><script id=\"csv-items\" type=\"application/json\">{}</script><script>{}</script></body></html>\n",
        include_str!("viewer.css"),
        embed(title)?,
        embed(csv)?,
        embed(&crate::csv_report::item_outcomes(csv))?,
        include_str!("viewer.js"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_csv_cannot_close_its_script_and_the_page_is_private() {
        let csv = "row_kind,note\r\ncall,\"</script><script>alert(1)</script>\"\r\n";
        let directory =
            std::env::temp_dir().join(format!("follower-viewer-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("report.html");
        write_csv_viewer(csv, "</script>title", &path).unwrap();
        let html = std::fs::read_to_string(&path).unwrap();
        assert!(!html.contains("</script><script>alert(1)"));
        assert!(!html.contains("</script>title"));
        assert!(html.contains("\\u003c/script>"));
        assert!(!html.contains("https://"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}
