use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use code_flow::{Analyzer, Project};
use serde::Serialize;

#[derive(Parser)]
#[command(
    name = "flow",
    version,
    about = "Explore callback flow in TypeScript/React source"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Parse, bind, lower, and resolve a configured project.
    Index {
        #[arg(long)]
        project: PathBuf,
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Audit callbacks created by a configured repository model.
    Audit {
        #[arg(long)]
        project: PathBuf,
        #[arg(long)]
        model: String,
        #[arg(long, value_enum, default_value_t = AuditScope::Reachable)]
        scope: AuditScope,
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
        #[arg(long)]
        fail_on_unresolved: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    Json,
    Text,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum AuditScope {
    Reachable,
}

#[derive(Serialize)]
struct IndexSummary<'a> {
    schema_version: u32,
    snapshot_id: &'a str,
    frontend_version: &'a str,
    files: usize,
    symbols: usize,
    functions: usize,
    cfg_blocks: usize,
    diagnostics: usize,
    resolved_imports: usize,
    unresolved_imports: usize,
    snapshot_path: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Index { project, format } => index(&project, format),
        Command::Audit {
            project,
            model,
            scope: _,
            format,
            fail_on_unresolved,
        } => audit(&project, &model, format, fail_on_unresolved),
    }
}

fn audit(
    config_path: &std::path::Path,
    model: &str,
    format: OutputFormat,
    fail_on_unresolved: bool,
) -> Result<()> {
    let analyzer = Analyzer::new(Project::load(config_path)?);
    let report = analyzer.audit(model)?;
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        OutputFormat::Text => {
            println!(
                "audit {} in snapshot {}",
                report.model_id, report.snapshot_id
            );
            for finding in &report.findings {
                println!(
                    "{} [{}] {}: {:?}",
                    finding.key, finding.choice, finding.finding_id, finding.conclusion
                );
                for registration in &finding.registrations {
                    println!(
                        "  registration {} @ file {} bytes {}..{}",
                        registration.summary,
                        registration.span.file_id.0,
                        registration.span.start,
                        registration.span.end
                    );
                }
                for invocation in &finding.invocations {
                    println!(
                        "  invocation {} @ file {} bytes {}..{}",
                        invocation.summary,
                        invocation.span.file_id.0,
                        invocation.span.start,
                        invocation.span.end
                    );
                }
                for unresolved in &finding.unresolved {
                    println!("  unresolved {}", unresolved.summary);
                }
            }
            println!(
                "coverage: {} ({} processed files)",
                if report.coverage.complete {
                    "complete for modeled scope"
                } else {
                    "incomplete"
                },
                report.coverage.processed_files
            );
        }
    }
    if fail_on_unresolved
        && report
            .findings
            .iter()
            .any(|finding| finding.conclusion == code_flow::queries::Conclusion::Unresolved)
    {
        anyhow::bail!("audit contains unresolved findings");
    }
    Ok(())
}

fn index(config_path: &std::path::Path, format: OutputFormat) -> Result<()> {
    let analyzer = Analyzer::new(Project::load(config_path)?);
    let snapshot = analyzer.index()?;
    let snapshot_path = analyzer.default_snapshot_path();
    snapshot.write(&snapshot_path)?;
    let unresolved_imports = snapshot
        .resolutions
        .iter()
        .filter(|resolution| resolution.status == code_flow::link::ResolutionStatus::Unresolved)
        .count();
    let summary = IndexSummary {
        schema_version: snapshot.schema_version,
        snapshot_id: &snapshot.snapshot_id,
        frontend_version: &snapshot.frontend_version,
        files: snapshot.files.len(),
        symbols: snapshot.files.iter().map(|file| file.symbols.len()).sum(),
        functions: snapshot.files.iter().map(|file| file.functions.len()).sum(),
        cfg_blocks: snapshot.files.iter().map(|file| file.blocks.len()).sum(),
        diagnostics: snapshot
            .files
            .iter()
            .map(|file| file.diagnostics.len())
            .sum(),
        resolved_imports: snapshot.resolutions.len() - unresolved_imports,
        unresolved_imports,
        snapshot_path: snapshot_path.display().to_string(),
    };
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&summary)?),
        OutputFormat::Text => {
            println!("snapshot {}", summary.snapshot_id);
            println!(
                "{} files, {} symbols, {} functions, {} CFG blocks",
                summary.files, summary.symbols, summary.functions, summary.cfg_blocks
            );
            println!(
                "{} resolved imports, {} unresolved imports, {} diagnostics",
                summary.resolved_imports, summary.unresolved_imports, summary.diagnostics
            );
            println!("wrote {}", summary.snapshot_path);
        }
    }
    Ok(())
}
