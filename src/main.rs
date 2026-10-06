use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use code_flow::{
    Analyzer, Project,
    csv_report::{csv_records, query_report_csv},
    csv_viewer::write_csv_viewer,
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
        #[arg(long, value_enum, default_value_t = QueryFormat::Text)]
        format: QueryFormat,
        #[arg(long)]
        fail_on_unresolved: bool,
        /// Write a self-contained, private HTML report to /tmp.
        #[arg(long)]
        html_report: bool,
    },
    /// Write a browsable page for a query CSV from `query --format csv`.
    View {
        /// The CSVs to show, as one table; each must have the same columns. Written as
        /// `label=path`, a CSV's rows get the label in a leading `source` column.
        #[arg(required = true)]
        csv: Vec<String>,
        /// Where to write the page; defaults to the CSV path with an `.html` extension.
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    Json,
    Text,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum QueryFormat {
    Json,
    Text,
    /// One row per call and item it applies to, for agents and spreadsheets. `follower view`
    /// turns it into a browsable page.
    Csv,
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
        Command::View { csv, output } => view(&csv, output),
    }
}

fn view(arguments: &[String], output: Option<PathBuf>) -> Result<()> {
    // Several query CSVs with the same columns show as one table, as a hook query's calls and a
    // query of direct calls that answer the same question. A label says which CSV a row is from.
    let csvs = arguments
        .iter()
        .map(|argument| match argument.split_once('=') {
            Some((label, path)) if !label.is_empty() && !label.contains('/') => {
                (Some(label.to_owned()), PathBuf::from(path))
            }
            _ => (None, PathBuf::from(argument)),
        })
        .collect::<Vec<_>>();
    let labelled = csvs.iter().any(|(label, _)| label.is_some());
    let csv_label = |label: &str| {
        if label.contains([',', '"', '\n']) {
            format!("\"{}\"", label.replace('"', "\"\""))
        } else {
            label.to_owned()
        }
    };
    let mut text = String::new();
    let mut header: Option<String> = None;
    for (label, csv) in &csvs {
        let source = std::fs::read_to_string(csv)
            .with_context(|| format!("failed to read {}", csv.display()))?;
        let records = csv_records(&source);
        let Some((first, rows)) = records.split_first() else {
            bail!("{} is empty", csv.display());
        };
        match &header {
            None => {
                if labelled {
                    text.push_str("source,");
                }
                text.push_str(first);
                text.push('\n');
                header = Some((*first).to_owned());
            }
            Some(header) if header != first => bail!(
                "{} has different columns from {}",
                csv.display(),
                csvs[0].1.display()
            ),
            Some(_) => {}
        }
        let label = label
            .as_deref()
            .map_or_else(String::new, |label| format!("{},", csv_label(label)));
        for row in rows {
            if labelled {
                text.push_str(&label);
            }
            text.push_str(row);
            text.push('\n');
        }
    }
    let csvs = csvs.into_iter().map(|(_, csv)| csv).collect::<Vec<_>>();
    let first = &csvs[0];
    let output = output.unwrap_or_else(|| first.with_extension("html"));
    let title = csvs
        .iter()
        .map(|csv| {
            csv.file_name().map_or_else(
                || csv.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            )
        })
        .collect::<Vec<_>>()
        .join(" + ");
    write_csv_viewer(&text, &title, &output)?;
    eprintln!("Viewer: {}", output.display());
    Ok(())
}

fn run_query(
    config_path: &std::path::Path,
    query_path: &std::path::Path,
    format: QueryFormat,
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
        QueryFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        QueryFormat::Text => print_query_report(&report),
        QueryFormat::Csv => print!("{}", query_report_csv(&report, &query)),
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
    print_callsite_values(report);
    if !report.creations.is_empty() {
        println!("creations: {}", report.creations.len());
    }
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
    if !report.component_boundaries.is_empty() {
        println!(
            "component boundaries: {} (possible creations depend on these; check a component before adding a suggested contract)",
            report.component_boundaries.len()
        );
        for boundary in report.component_boundaries.iter().take(10) {
            let target = match (&boundary.module, &boundary.export) {
                (Some(module), Some(export)) => format!(" from {module}#{export}"),
                _ => String::new(),
            };
            println!(
                "  {} {:?} {}{target}: {} creations, {} only through it{} ({})",
                boundary.boundary_id,
                boundary.kind,
                boundary.component,
                boundary.affected_creations,
                boundary.sole_blocker_creations,
                if boundary.entered_from_reachable {
                    ", reached exactly"
                } else {
                    ""
                },
                boundary
                    .sites
                    .first()
                    .map_or_else(String::new, |site| format!(
                        "{}:{}",
                        site.path, site.start_line
                    )),
            );
            if let Some(contract) = &boundary.suggested_contract {
                for line in contract.lines() {
                    println!("    {line}");
                }
            }
        }
    }
    if !report.unreached_callsites.is_empty() {
        let mut reasons = std::collections::BTreeMap::<String, usize>::new();
        let mut blockers =
            std::collections::BTreeMap::<(String, String), (usize, String, Option<String>)>::new();
        for unreached in &report.unreached_callsites {
            let reason = format!("{:?}", unreached.reason);
            *reasons.entry(reason.clone()).or_default() += 1;
            let site = unreached.blocking_site.as_ref().map_or_else(
                || unreached.detail.clone(),
                |site| format!("{}:{}", site.path, site.start_line),
            );
            let entry = blockers.entry((reason, site)).or_insert_with(|| {
                (
                    0,
                    unreached.detail.clone(),
                    unreached.suggested_contract.clone(),
                )
            });
            entry.0 += 1;
        }
        println!(
            "unreached callsites: {} ({})",
            report.unreached_callsites.len(),
            reasons
                .iter()
                .map(|(reason, count)| format!("{count} {reason}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut blockers = blockers.into_iter().collect::<Vec<_>>();
        blockers.sort_by(|left, right| right.1.0.cmp(&left.1.0).then_with(|| left.0.cmp(&right.0)));
        for ((reason, site), (count, detail, contract)) in blockers.iter().take(10) {
            println!("  {count} behind {reason} at {site}: {detail}");
            if let Some(contract) = contract {
                for line in contract.lines() {
                    println!("    {line}");
                }
            }
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

/// Possible arrays shown one by one; more are summarized by their elements.
const SHOWN_ARRAYS: usize = 4;

/// The number of arrays a value may be.
fn array_count(value: &QueryValue) -> usize {
    match value {
        QueryValue::Array { .. } => 1,
        QueryValue::Alternatives { values } => values.iter().map(array_count).sum(),
        _ => 0,
    }
}

/// The values at each callsite and the calls made with its result, before the per-creation
/// detail.
fn print_callsite_values(report: &QueryReport) {
    if report.callsites.is_empty() {
        return;
    }
    let mut statuses = std::collections::BTreeMap::<String, usize>::new();
    for callsite in &report.callsites {
        *statuses
            .entry(format!("{:?}", callsite.capability.status))
            .or_default() += 1;
    }
    println!(
        "callsites: {} ({})",
        report.callsites.len(),
        statuses
            .iter()
            .map(|(status, count)| format!("{count} {status}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let place = |location: Option<&code_flow::query::QueryLocation>| {
        location.map_or_else(
            || "<unknown location>".to_owned(),
            |location| format!("{}:{}", location.path, location.start_line),
        )
    };
    let values = |values: &[QueryValue]| {
        values
            .iter()
            .map(render_query_value)
            .collect::<Vec<_>>()
            .join(" | ")
    };
    for callsite in &report.callsites {
        println!(
            "  {} in {} [{:?}, {} contexts{}]",
            place(callsite.location.as_ref()),
            callsite.enclosing.as_deref().unwrap_or("<module>"),
            callsite.reachability,
            callsite.contexts,
            callsite
                .unreached_reason
                .map(|reason| format!(", unreached: {reason:?}"))
                .unwrap_or_default()
        );
        for (label, value) in &callsite.factory_arguments {
            let arrays = value.iter().map(array_count).sum::<usize>();
            let elements = callsite.possible_elements.get(label);
            // Many possible arrays read better as the elements they are drawn from.
            match elements {
                Some(elements) if arrays > SHOWN_ARRAYS => println!(
                    "    factory {label} = one of {arrays} arrays of: {}",
                    elements
                        .iter()
                        .map(render_query_value)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                _ => println!("    factory {label} = {}", values(value)),
            }
            if let Some(elements) = elements
                && arrays <= 1
            {
                println!("    factory {label} may contain {}", values(elements));
            }
        }
        for (label, callers) in &callsite.values_from_callers {
            for caller in callers {
                println!(
                    "    factory {label} = {} from the caller at {}",
                    render_query_value(&caller.value),
                    place(caller.caller.as_ref())
                );
            }
        }
        println!("    result {:?}", callsite.capability.status);
        for call in &callsite.capability.calls {
            let arguments = call
                .arguments
                .iter()
                .map(|(label, value)| format!("{label} = {}", values(value)))
                .collect::<Vec<_>>()
                .join(", ");
            let mut notes = Vec::new();
            for (label, elements) in &call.elements {
                let shown = elements
                    .iter()
                    .map(render_query_value)
                    .collect::<Vec<_>>()
                    .join(", ");
                notes.push(if call.elements_complete {
                    format!("for {label} {shown}")
                } else {
                    format!("for {label} {shown} (incomplete)")
                });
            }
            if let Some(context) = call.context.first() {
                notes.push(format!("in {context}"));
            }
            if !call.via.is_empty() {
                notes.push(format!("via {}", call.via.join(", ")));
            }
            if !call.explored {
                notes.push("not executed by an explored path".to_owned());
            }
            println!(
                "      call {arguments} at {}{}",
                place(call.location.as_ref()),
                if notes.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", notes.join("; "))
                }
            );
        }
        for call in &callsite.capability.excluded_calls {
            let guards = call
                .guards
                .iter()
                .map(|set| values(set))
                .collect::<Vec<_>>()
                .join("; ");
            println!(
                "      excluded call at {}: its conditions ({guards}) match nothing this callsite requests",
                place(call.location.as_ref())
            );
        }
        for escape in &callsite.capability.escapes {
            println!(
                "      escape at {}: {}",
                place(escape.location.as_ref()),
                escape.detail
            );
        }
    }
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
