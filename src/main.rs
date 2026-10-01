use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use code_flow::{
    Analyzer, Project,
    html_report::write_query_report_html,
    query::{QueryReport, QueryValue, load_query},
};
use serde::Serialize;

#[derive(Parser)]
#[command(
    name = "follower",
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
    /// Run a generic creation-to-invocation flow query.
    Query {
        #[arg(long)]
        project: PathBuf,
        #[arg(long)]
        query: PathBuf,
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
        #[arg(long)]
        fail_on_unresolved: bool,
        /// Write a self-contained, private HTML report to /tmp.
        #[arg(long)]
        html_report: bool,
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
        Command::Query {
            project,
            query,
            format,
            fail_on_unresolved,
            html_report,
        } => run_query(&project, &query, format, fail_on_unresolved, html_report),
    }
}

fn run_query(
    config_path: &std::path::Path,
    query_path: &std::path::Path,
    format: OutputFormat,
    fail_on_unresolved: bool,
    html_report: bool,
) -> Result<()> {
    let analyzer = Analyzer::new(Project::load(config_path)?);
    let (query, query_hash) = load_query(query_path)?;
    let report = analyzer.query(&query, &query_hash)?;
    if html_report || query.report.html {
        let path = write_query_report_html(&report)?;
        eprintln!("HTML report: {}", path.display());
    }
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        OutputFormat::Text => print_query_report(&report),
    }
    if fail_on_unresolved
        && (!report.coverage.complete
            || report
                .creations
                .iter()
                .any(|creation| creation.unresolved_count > 0))
    {
        anyhow::bail!("query has incomplete coverage or unresolved escapes");
    }
    Ok(())
}

fn print_query_report(report: &QueryReport) {
    println!(
        "query {} in snapshot {} ({:?})",
        report.query_id, report.snapshot_id, report.scope
    );
    for creation in &report.creations {
        println!(
            "{} [{}] {:?} @ {}",
            creation.creation_id,
            creation.choice,
            creation.conclusion,
            render_location(
                creation.factory_location.as_ref(),
                &creation.factory_callsite
            )
        );
        for (label, value) in &creation.factory_arguments {
            println!("  factory {label} = {}", render_query_value(value));
        }
        for invocation in &creation.invocations {
            println!(
                "  invocation {} @ {}",
                invocation.evidence_id,
                render_location(invocation.location.as_ref(), &invocation.callsite)
            );
            for (label, value) in &invocation.arguments {
                println!("    {label} = {}", render_query_value(value));
            }
        }
        for unresolved in &creation.unresolved {
            println!("  unresolved {}", unresolved.summary);
        }
        if creation.unresolved_count > creation.unresolved.len() {
            println!(
                "  {} unresolved details omitted by query report options",
                creation.unresolved_count - creation.unresolved.len()
            );
        }
    }
    println!(
        "coverage: {} ({} creations, {} processed files)",
        if report.coverage.complete {
            "complete for modeled scope"
        } else {
            "incomplete"
        },
        report.creations.len(),
        report.coverage.processed_files
    );
    let statuses = &report.callsite_inventory.callsites;
    println!(
        "callsite inventory: {} analyzed, {} filtered, {} unresolved, {} skipped ({} candidate files of {} configured, {} candidates over budget, round limit hit: {})",
        statuses
            .iter()
            .filter(|site| site.status == code_flow::query::QueryCallsiteStatus::Analyzed)
            .count(),
        statuses
            .iter()
            .filter(|site| site.status == code_flow::query::QueryCallsiteStatus::Filtered)
            .count(),
        statuses
            .iter()
            .filter(|site| site.status == code_flow::query::QueryCallsiteStatus::Unresolved)
            .count(),
        statuses
            .iter()
            .filter(|site| site.status == code_flow::query::QueryCallsiteStatus::Skipped)
            .count(),
        report.callsite_inventory.candidate_files,
        report.callsite_inventory.configured_files,
        report.callsite_inventory.skipped_candidate_files,
        report.callsite_inventory.round_limit_hit,
    );
}

fn render_location(
    location: Option<&code_flow::query::QueryLocation>,
    span: &code_flow::ir::SourceSpan,
) -> String {
    location.map_or_else(
        || format!("file {} bytes {}..{}", span.file_id.0, span.start, span.end),
        |location| {
            format!(
                "{}:{}:{}",
                location.path, location.start_line, location.start_column
            )
        },
    )
}

fn render_query_value(value: &QueryValue) -> String {
    match value {
        QueryValue::Null => "null".to_owned(),
        QueryValue::String { value } => format!("{value:?}"),
        QueryValue::Number { value } => value.to_string(),
        QueryValue::EnumMember {
            enum_name,
            member_name,
            value,
        } => format!("{enum_name}.{member_name} ({value})"),
        QueryValue::Boolean { value } => value.to_string(),
        QueryValue::Alternatives { values } => format!(
            "({})",
            values
                .iter()
                .map(render_query_value)
                .collect::<Vec<_>>()
                .join(" | ")
        ),
        QueryValue::Array { elements } => format!(
            "[{}]",
            elements
                .iter()
                .map(render_query_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        QueryValue::Undefined => "undefined".to_owned(),
        QueryValue::Unknown { reason } => format!("<unknown: {reason}>"),
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
        && (!report.coverage.complete
            || report
                .findings
                .iter()
                .any(|finding| !finding.unresolved.is_empty()))
    {
        anyhow::bail!("audit has incomplete coverage or unresolved escapes");
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
