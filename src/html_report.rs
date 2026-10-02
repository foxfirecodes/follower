use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};

use crate::query::QueryReport;

static NEXT_REPORT: AtomicU64 = AtomicU64::new(0);

/// Write a self-contained report to a new, owner-readable file in /tmp.
pub fn write_query_report_html(report: &QueryReport) -> Result<PathBuf> {
    #[cfg(unix)]
    let directory = Path::new("/tmp");
    #[cfg(not(unix))]
    let temp_directory = std::env::temp_dir();
    #[cfg(not(unix))]
    let directory = temp_directory.as_path();

    let mut file_path = None;
    for _ in 0..32 {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_nanos();
        let sequence = NEXT_REPORT.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            "follower-query-{}-{nonce}-{sequence}.html",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => {
                file_path = Some((path, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("failed to create HTML report"),
        }
    }
    let Some((path, file)) = file_path else {
        bail!("failed to allocate a unique HTML report path");
    };
    if let Err(error) = write_report(file, report) {
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    Ok(path)
}

fn write_report(file: File, report: &QueryReport) -> Result<()> {
    let mut out = BufWriter::new(file);
    let json = serde_json::to_string(report)?.replace('<', "\\u003c");
    out.write_all(b"<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; base-uri 'none'; form-action 'none'\"><title>Follower query report</title><style>")?;
    out.write_all(include_str!("report.css").as_bytes())?;
    out.write_all(b"</style></head><body><div id=\"app\"></div><script id=\"report-data\" type=\"application/json\">")?;
    out.write_all(json.as_bytes())?;
    out.write_all(b"</script><script>")?;
    out.write_all(include_str!("report.js").as_bytes())?;
    out.write_all(b"</script></body></html>\n")?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        queries::Coverage,
        query::{QueryCallsiteInventory, QueryKind, QueryReport, QueryScope},
    };

    #[test]
    fn report_is_private_and_embedded_json_cannot_close_script() {
        let report = QueryReport {
            schema_version: 10,
            snapshot_id: "snapshot".into(),
            config_hash: "config".into(),
            query_hash: "query".into(),
            query_id: "</script><script>alert(1)</script>".into(),
            kind: QueryKind::FactoryReturnInvocations,
            scope: QueryScope::Reachable,
            creations: Vec::new(),
            callsite_inventory: QueryCallsiteInventory {
                configured_files: 0,
                candidate_files: 0,
                skipped_candidate_files: 0,
                round_limit_hit: false,
                callsites: Vec::new(),
            },
            evidence: Vec::new(),
            gaps: Vec::new(),
            component_boundaries: Vec::new(),
            unreached_callsites: Vec::new(),
            callsites: Vec::new(),
            coverage: Coverage {
                scope: "test".into(),
                roots: Vec::new(),
                processed_files: 0,
                complete: true,
                gaps: Vec::new(),
            },
            diagnostics: Vec::new(),
        };
        let path = write_query_report_html(&report).unwrap();
        let html = std::fs::read_to_string(&path).unwrap();
        assert!(html.contains("\\u003c/script>"));
        assert!(!html.contains("</script><script>alert(1)</script>"));
        assert!(!html.contains("https://"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert!(path.starts_with("/tmp"));
        }
        std::fs::remove_file(path).unwrap();
    }
}
