#![allow(clippy::needless_pass_by_value, clippy::too_many_lines)]

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::Path,
    rc::Rc,
    time::Instant,
};

use anyhow::{Context, Result, bail};

use crate::{
    cache::Snapshot,
    evidence::{Evidence, RelationKind},
    ids::{EvidenceId, FileId},
    ir::{
        FlowArrowBody, FlowAssignmentTarget, FlowBinding, FlowExpression, FlowExpressionKind,
        FlowFunction, FlowJsxProp, FlowJsxTag, FlowLogicalOperator, FlowPattern, FlowPatternKind,
        FlowStatement, SourceSpan, lazy_component_import,
    },
    link::{LinkedSymbol, LinkedValue, SymbolLinker, ValueResolution, pattern_names},
    models::{CallbackFactoryModel, CaptureSource, ModelEvidence, ModelValue, ModeledOperation},
    project::{ComponentConsumer, Project},
    queries::{AuditFinding, AuditReport, Conclusion, Coverage, FindingRef},
    query::{
        QueryCallPathKind, QueryCallPathStep, QueryCallsiteInventory, QueryCreation, QueryGap,
        QueryInvocation, QueryLocation, QueryReport, QueryReverseImporter,
        QueryReverseImporterEvaluation, QueryScope, QuerySpec, QueryValue, Reachability,
    },
};

const MAX_CALL_DEPTH: usize = 128;
const MAX_REVERSE_IMPORTER_EVALUATIONS: usize = 5_000;
const MAX_UNREACHED_RENDER_EVALUATIONS: usize = 5_000;
const MAX_ASSUMED_RENDER_EVALUATIONS: usize = 20_000;
const MAX_REVERSE_IMPORTER_SOURCE_BYTES: usize = 20_000;
const MAX_HEAP_ALTERNATIVES: usize = 32;
const MAX_RENDER_VISITS_PER_SITE: usize = 16;
const TEXT_FILTER_GAP: &str =
    "directory sources were text-filtered; files without a configured term were not analyzed";

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct FunctionKey {
    file_id: FileId,
    name: String,
}

#[derive(Clone)]
struct TrackedValue {
    value: AbstractValue,
    evidence: Option<EvidenceId>,
    choice: Option<String>,
    heap_id: Option<u64>,
}

impl TrackedValue {
    fn plain(value: AbstractValue) -> Self {
        Self {
            value,
            evidence: None,
            choice: None,
            heap_id: None,
        }
    }

    fn unknown(reason: impl Into<String>) -> Self {
        Self::plain(AbstractValue::Unknown(reason.into()))
    }
}

type Environment = BTreeMap<String, TrackedValue>;

#[derive(Clone)]
enum AbstractValue {
    Null,
    String(String),
    Number(i64),
    EnumMember {
        enum_name: String,
        member_name: String,
        value: i64,
    },
    Boolean(bool),
    Record(Rc<BTreeMap<String, TrackedValue>>),
    Array(Rc<Vec<TrackedValue>>),
    Union(Rc<Vec<TrackedValue>>),
    Function(FunctionKey),
    Namespace(FileId),
    ModelFunction,
    Closure(Rc<ClosureValue>),
    Capability(usize),
    Element(Rc<ElementValue>),
    Intrinsic(String),
    ConfiguredComponent(ComponentConsumer),
    Undefined,
    Unknown(String),
    /// The unknown result of a call that received components, such as an unmodeled
    /// higher-order component. Rendering it may render the wrapped components; otherwise it
    /// behaves like `Unknown`.
    AssumedWrapper {
        reason: String,
        wrapped: Rc<Vec<TrackedValue>>,
    },
}

impl AbstractValue {
    fn record(fields: BTreeMap<String, TrackedValue>) -> Self {
        Self::Record(Rc::new(fields))
    }

    fn array(elements: Vec<TrackedValue>) -> Self {
        Self::Array(Rc::new(elements))
    }

    fn union(values: Vec<TrackedValue>) -> Self {
        Self::Union(Rc::new(values))
    }

    fn element(element: ElementValue) -> Self {
        Self::Element(Rc::new(element))
    }
}

#[derive(Clone)]
struct ClosureValue {
    body: FlowArrowBody,
    params: Vec<FlowPattern>,
    environment: Environment,
    file_id: FileId,
    span: SourceSpan,
}

#[derive(Clone)]
struct ElementValue {
    component: Box<TrackedValue>,
    props: BTreeMap<String, TrackedValue>,
    span: SourceSpan,
    trace: Vec<TraceStep>,
}

struct CapabilityState {
    key: String,
    choice: String,
    callsite: SourceSpan,
    origin: EvidenceId,
    origin_trace: Vec<TraceStep>,
    factory_arguments: Vec<TrackedValue>,
    reachability: Reachability,
    reverse_importer: Option<ReverseImporterSeed>,
    registrations: Vec<EvidenceId>,
    invocations: Vec<InvocationState>,
    unresolved: Vec<EvidenceId>,
    assumptions: Vec<String>,
}

#[derive(Clone)]
struct ReverseImporterSeed {
    symbol: String,
    matched_imports: Vec<String>,
    evaluation: QueryReverseImporterEvaluation,
    span: SourceSpan,
}

struct InvocationState {
    evidence: EvidenceId,
    arguments: Vec<TrackedValue>,
    call_path: Vec<TraceStep>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TraceStep {
    kind: QueryCallPathKind,
    span: SourceSpan,
}

struct LocationSource {
    text: String,
    line_starts: Vec<usize>,
}

impl LocationSource {
    fn new(text: String) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(
            text.bytes()
                .enumerate()
                .filter_map(|(index, byte)| (byte == b'\n').then_some(index + 1)),
        );
        Self { text, line_starts }
    }

    fn line_column(&self, offset: u32) -> (u32, u32) {
        let mut offset = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset = offset.saturating_sub(1);
        }
        let line_index = self
            .line_starts
            .partition_point(|start| *start <= offset)
            .saturating_sub(1);
        let column = self.text[self.line_starts[line_index]..offset]
            .chars()
            .count()
            + 1;
        (
            u32::try_from(line_index + 1).unwrap_or(u32::MAX),
            u32::try_from(column).unwrap_or(u32::MAX),
        )
    }
}

#[cfg(test)]
mod location_tests {
    use super::{LocationSource, gap_kind};

    #[test]
    fn cached_line_offsets_keep_one_based_unicode_columns() {
        let source = LocationSource::new("α\n🙂x".to_owned());
        assert_eq!(source.line_column(0), (1, 1));
        assert_eq!(source.line_column(2), (1, 2));
        assert_eq!(source.line_column(3), (2, 1));
        assert_eq!(source.line_column(7), (2, 2));
        assert_eq!(source.line_column(8), (2, 3));
    }

    #[test]
    fn gap_kinds_do_not_include_dynamic_identifiers() {
        assert_eq!(
            gap_kind("call through unknown target: unresolved_local_identifier:consumer"),
            "call_through_unknown_target"
        );
        assert_eq!(gap_kind("unknown property privateName"), "unknown_property");
        assert_eq!(
            gap_kind("array map receiver is unknown (privateReason)"),
            "array_map_receiver_unknown"
        );
    }
}

#[derive(Clone)]
struct FactoryCallCandidate {
    file_id: FileId,
    callee: FlowExpression,
    span: SourceSpan,
    arguments: Vec<FlowExpression>,
    enclosing_function: Option<FunctionKey>,
}

pub fn audit(
    project: &Project,
    snapshot: &Snapshot,
    model: &CallbackFactoryModel,
    model_hash: &str,
) -> Result<AuditReport> {
    let mut solver = Solver::new(project, snapshot, model.clone())?;
    solver.run()?;
    Ok(solver.report(model_hash))
}

/// The result of one solver pass over a snapshot.
pub struct QueryPass {
    pub report: QueryReport,
    /// Unparsed files requested anywhere in the pass.
    pub requested_imports: BTreeSet<std::path::PathBuf>,
    /// Unparsed files requested while exploring configured roots: values the root paths read and
    /// unknown components on the entry corridor.
    pub root_requested_imports: BTreeSet<std::path::PathBuf>,
    pub producer_paths: BTreeSet<std::path::PathBuf>,
    pub reachable_seed_callsites: Vec<SourceSpan>,
}

#[allow(clippy::too_many_arguments)]
pub fn execute_query(
    project: &Project,
    snapshot: &Snapshot,
    query: &QuerySpec,
    query_hash: &str,
    reverse_seed_paths: &BTreeSet<std::path::PathBuf>,
    reverse_producer_paths: &BTreeSet<std::path::PathBuf>,
    entry_corridor: &BTreeMap<std::path::PathBuf, usize>,
    run_roots: bool,
) -> Result<QueryPass> {
    let captures = query
        .factory_arguments
        .iter()
        .map(|projection| {
            (
                projection.label.clone(),
                CaptureSource::Argument {
                    index: projection.index,
                },
            )
        })
        .collect();
    let model = CallbackFactoryModel {
        id: format!("query:{}", query.id),
        r#match: query.factory.clone(),
        returned_property: query.capability.returned_property.clone(),
        returned_index: query.capability.returned_index,
        scan_callback_bodies: query.scan_callback_bodies,
        retains_returned_callback: Some(false),
        invokes_returned_callback_during_call: Some(false),
        captures,
        on_invoke: Vec::new(),
        evidence: ModelEvidence {
            kind: "query_instrumentation".to_owned(),
            reason: format!(
                "query {} treats the selected return as a capability",
                query.id
            ),
        },
    };
    let mut solver = Solver::new(project, snapshot, model)?;
    solver.entry_corridor = entry_corridor.keys().cloned().collect();
    if !entry_corridor.is_empty() {
        let mut corridor_files = entry_corridor
            .keys()
            .filter_map(|path| solver.symbol_linker.file_at(path))
            .map(|file| file.file_id)
            .collect::<BTreeSet<_>>();
        corridor_files.extend(
            solver
                .factory_candidates()
                .iter()
                .map(|candidate| candidate.file_id),
        );
        solver.corridor_files = corridor_files;
    }
    let phase_start = Instant::now();
    if project.config.entries.is_empty() {
        if query.scope == QueryScope::Reachable {
            bail!("a reachable query requires at least one configured entry point");
        }
        solver.prepare_globals(None);
    } else if run_roots {
        solver.run()?;
    } else {
        solver.prepare_globals(None);
    }
    if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
        eprintln!(
            "query solver roots: {} ms",
            phase_start.elapsed().as_millis()
        );
    }
    if query.scope == QueryScope::AllCreations {
        if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
            eprintln!("query solver: seeding factory creations");
        }
        let phase_start = Instant::now();
        solver.seed_unreached_creations();
        if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
            eprintln!(
                "query solver factory seeds: {} ms",
                phase_start.elapsed().as_millis()
            );
        }
        if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
            eprintln!("query solver: creations seeded; seeding reverse importers");
        }
        let phase_start = Instant::now();
        solver.seed_reverse_importers(reverse_seed_paths, reverse_producer_paths);
        if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
            eprintln!(
                "query solver reverse seeds: {} ms",
                phase_start.elapsed().as_millis()
            );
        }
    }
    let requested_imports = std::mem::take(&mut solver.requested_imports);
    let root_requested_imports = std::mem::take(&mut solver.root_requested_imports);
    if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
        eprintln!(
            "query module environments: hits={} misses={}",
            solver.module_env_cache_hits, solver.module_env_cache_misses
        );
    }
    let producer_paths = solver
        .capability_producer_files
        .union(&solver.caller_producer_files)
        .filter_map(|file_id| snapshot.files.iter().find(|file| file.file_id == *file_id))
        .map(|file| file.path.clone())
        .collect();
    let reachable_seed_callsites = if query.scope == QueryScope::Reachable {
        solver
            .factory_candidates()
            .into_iter()
            .filter(|candidate| {
                solver.expression_matches_model(candidate.file_id, &candidate.callee)
            })
            .map(|candidate| candidate.span)
            .collect()
    } else {
        Vec::new()
    };
    let phase_start = Instant::now();
    let report = solver.query_report(query, query_hash);
    if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
        eprintln!(
            "query solver report: {} ms",
            phase_start.elapsed().as_millis()
        );
    }
    Ok(QueryPass {
        report,
        requested_imports,
        root_requested_imports,
        producer_paths,
        reachable_seed_callsites,
    })
}

struct Solver<'a> {
    project: &'a Project,
    snapshot: &'a Snapshot,
    model: CallbackFactoryModel,
    functions: BTreeMap<FunctionKey, &'a FlowFunction>,
    symbol_linker: SymbolLinker<'a>,
    model_symbol: LinkedSymbol,
    globals_ir: Vec<(FileId, &'a FlowBinding)>,
    global_bindings: BTreeMap<LinkedSymbol, usize>,
    evaluating_globals: BTreeSet<usize>,
    initialized_globals: BTreeSet<usize>,
    globals: BTreeMap<LinkedSymbol, TrackedValue>,
    module_import_links: BTreeMap<FileId, Vec<(String, LinkedValue)>>,
    module_env_cache: BTreeMap<FileId, Environment>,
    module_env_cache_hits: usize,
    module_env_cache_misses: usize,
    heap: BTreeMap<u64, Rc<AbstractValue>>,
    heap_versions: BTreeMap<u64, u64>,
    next_heap_id: u64,
    evidence: Vec<Evidence>,
    capabilities: Vec<CapabilityState>,
    diagnostics: Vec<String>,
    coverage_gaps: Vec<String>,
    coverage_gap_keys: BTreeSet<String>,
    query_gaps: Vec<(String, String, SourceSpan, Option<String>)>,
    query_gap_keys: BTreeSet<(String, Option<String>)>,
    location_sources: RefCell<BTreeMap<FileId, LocationSource>>,
    requested_imports: BTreeSet<std::path::PathBuf>,
    root_requested_imports: BTreeSet<std::path::PathBuf>,
    entry_corridor: BTreeSet<std::path::PathBuf>,
    /// Files on the entry corridor plus factory hosts. Empty when the corridor is unknown.
    corridor_files: BTreeSet<FileId>,
    assumed_evaluations: usize,
    assumed_budget_reported: bool,
    capability_producer_files: BTreeSet<FileId>,
    caller_producer_files: BTreeSet<FileId>,
    current_choice: Option<String>,
    current_reachability: Reachability,
    assumed_renders: Vec<SourceSpan>,
    current_reverse_importer: Option<ReverseImporterSeed>,
    call_depth: usize,
    active_captures: Vec<BTreeSet<String>>,
    reverse_evaluations: usize,
    reverse_budget_active: bool,
    reverse_budget_reported: bool,
    unreached_render_evaluations: usize,
    unreached_render_budget_active: bool,
    unreached_render_budget_reported: bool,
    unreached_render_seed_span: Option<SourceSpan>,
    render_visits: BTreeMap<(Option<String>, bool, FileId, u32, String), usize>,
    trace: Vec<TraceStep>,
}

impl<'a> Solver<'a> {
    fn new(
        project: &'a Project,
        snapshot: &'a Snapshot,
        model: CallbackFactoryModel,
    ) -> Result<Self> {
        let mut functions = BTreeMap::new();
        let mut globals_ir = Vec::new();
        let mut diagnostics = Vec::new();

        for file in &snapshot.files {
            diagnostics.extend(file.diagnostics.iter().map(|diagnostic| {
                format!("{}: {diagnostic}", display_path(&project.root, &file.path))
            }));
            for function in &file.flow.functions {
                let key = FunctionKey {
                    file_id: file.file_id,
                    name: function.name.clone(),
                };
                functions.insert(key, function);
            }
            globals_ir.extend(
                file.flow
                    .globals
                    .iter()
                    .map(|binding| (file.file_id, binding)),
            );
        }

        if model.r#match.project != project.config.name {
            bail!(
                "model {} targets project {}, not {}",
                model.id,
                model.r#match.project,
                project.config.name
            );
        }

        let symbol_linker = SymbolLinker::new(project, snapshot);
        let model_symbol = symbol_linker
            .resolve_matcher(&model.r#match)
            .with_context(|| {
                format!(
                    "factory matcher {}#{} does not resolve to one value declaration",
                    model.r#match.module, model.r#match.export
                )
            })?;
        let global_bindings = globals_ir
            .iter()
            .enumerate()
            .flat_map(|(index, (file_id, binding))| {
                pattern_names(&binding.pattern)
                    .into_iter()
                    .map(move |name| {
                        (
                            LinkedSymbol {
                                file_id: *file_id,
                                name: name.to_owned(),
                            },
                            index,
                        )
                    })
            })
            .collect();
        Ok(Self {
            project,
            snapshot,
            model,
            functions,
            symbol_linker,
            model_symbol,
            globals_ir,
            global_bindings,
            evaluating_globals: BTreeSet::new(),
            initialized_globals: BTreeSet::new(),
            globals: BTreeMap::new(),
            module_import_links: BTreeMap::new(),
            module_env_cache: BTreeMap::new(),
            module_env_cache_hits: 0,
            module_env_cache_misses: 0,
            heap: BTreeMap::new(),
            heap_versions: BTreeMap::new(),
            next_heap_id: 0,
            evidence: Vec::new(),
            capabilities: Vec::new(),
            diagnostics,
            coverage_gaps: if project.config.source_contains_any.is_empty() {
                Vec::new()
            } else {
                vec![TEXT_FILTER_GAP.to_owned()]
            },
            coverage_gap_keys: if project.config.source_contains_any.is_empty() {
                BTreeSet::new()
            } else {
                BTreeSet::from([TEXT_FILTER_GAP.to_owned()])
            },
            query_gaps: Vec::new(),
            query_gap_keys: BTreeSet::new(),
            location_sources: RefCell::new(BTreeMap::new()),
            requested_imports: BTreeSet::new(),
            root_requested_imports: BTreeSet::new(),
            entry_corridor: BTreeSet::new(),
            corridor_files: BTreeSet::new(),
            assumed_evaluations: 0,
            assumed_budget_reported: false,
            capability_producer_files: BTreeSet::new(),
            caller_producer_files: BTreeSet::new(),
            current_choice: None,
            current_reachability: Reachability::Reachable,
            assumed_renders: Vec::new(),
            current_reverse_importer: None,
            call_depth: 0,
            active_captures: Vec::new(),
            reverse_evaluations: 0,
            reverse_budget_active: false,
            reverse_budget_reported: false,
            unreached_render_evaluations: 0,
            unreached_render_budget_active: false,
            unreached_render_budget_reported: false,
            unreached_render_seed_span: None,
            render_visits: BTreeMap::new(),
            trace: Vec::new(),
        })
    }

    fn run(&mut self) -> Result<()> {
        if self.project.config.entries.is_empty() {
            bail!("analysis requires a configured entry point");
        }
        let entries = self.project.config.entries.clone();
        for entry in &entries {
            let entry_path = self
                .project
                .resolve_path(&entry.module)
                .canonicalize()
                .with_context(|| {
                    format!("failed to locate entry module {}", entry.module.display())
                })?;
            let entry_file = self.symbol_linker.file_at(&entry_path).with_context(|| {
                format!("entry module was not indexed: {}", entry_path.display())
            })?;
            let ValueResolution::Resolved(LinkedValue::Declaration(symbol)) = self
                .symbol_linker
                .resolve_exported_value(entry_file.file_id, &entry.export)
            else {
                bail!(
                    "entry export {} does not resolve to one value declaration",
                    entry.export
                );
            };
            let entry_key = FunctionKey {
                file_id: symbol.file_id,
                name: symbol.name,
            };
            let parameter_count = self
                .functions
                .get(&entry_key)
                .with_context(|| {
                    format!(
                        "entry export {} was not lowered from {}",
                        entry.export,
                        entry.module.display()
                    )
                })?
                .params
                .len();
            let entry_span = self.functions[&entry_key].span.clone();

            for props in self.entry_input_combinations(&entry.export)? {
                let choice = choice_label(&props);
                self.current_choice = Some(choice.clone());
                self.assumed_evaluations = 0;
                self.assumed_budget_reported = false;
                self.prepare_globals(Some(entry_file.file_id));
                let record = props
                    .into_iter()
                    .map(|(name, value)| {
                        (
                            name,
                            TrackedValue {
                                value: AbstractValue::String(value),
                                evidence: None,
                                choice: Some(choice.clone()),
                                heap_id: None,
                            },
                        )
                    })
                    .collect();
                let mut arguments = Vec::with_capacity(parameter_count.max(1));
                if parameter_count > 0 {
                    arguments.push(TrackedValue {
                        value: AbstractValue::record(record),
                        evidence: None,
                        choice: Some(choice),
                        heap_id: None,
                    });
                    arguments.extend(
                        (1..parameter_count)
                            .map(|_| TrackedValue::unknown("unconfigured_entry_argument")),
                    );
                }
                self.trace = vec![TraceStep {
                    kind: QueryCallPathKind::Entry,
                    span: entry_span.clone(),
                }];
                let returned = self.call_function(&entry_key, arguments);
                self.render(returned);
                self.trace.clear();
            }
        }
        self.current_choice = None;
        Ok(())
    }

    fn entry_input_combinations(&self, export: &str) -> Result<Vec<BTreeMap<String, String>>> {
        let prefix = format!("{export}.");
        let domains = self
            .project
            .config
            .inputs
            .iter()
            .filter_map(|(name, values)| {
                name.strip_prefix(&prefix)
                    .map(|property| (property.to_owned(), values))
            })
            .collect::<Vec<_>>();
        let mut combinations = vec![BTreeMap::new()];
        for (property, values) in domains {
            if values.is_empty() {
                bail!("input domain {export}.{property} cannot be empty");
            }
            let mut expanded = Vec::new();
            for combination in &combinations {
                for value in values {
                    let mut next = combination.clone();
                    next.insert(property.clone(), value.clone());
                    expanded.push(next);
                    if expanded.len() > 4096 {
                        bail!("entry input product for {export} exceeds 4096 contexts");
                    }
                }
            }
            combinations = expanded;
        }
        Ok(combinations)
    }

    fn prepare_globals(&mut self, entry: Option<FileId>) {
        self.globals.clear();
        self.module_env_cache.clear();
        self.initialized_globals.clear();
        self.evaluating_globals.clear();
        if let Some(entry) = entry {
            let mut modules = Vec::new();
            self.module_initialization_order(entry, &mut BTreeSet::new(), &mut modules);
            for module in modules {
                for index in 0..self.globals_ir.len() {
                    if self.globals_ir[index].0 == module {
                        self.evaluate_global(index);
                    }
                }
            }
        }
    }

    fn module_initialization_order(
        &self,
        module: FileId,
        visited: &mut BTreeSet<FileId>,
        modules: &mut Vec<FileId>,
    ) {
        if !visited.insert(module) {
            return;
        }
        let Some(file) = self.symbol_linker.file(module) else {
            return;
        };
        for resolution in self.symbol_linker.import_resolutions(&file.path) {
            if let Some(dependency) = resolution
                .resolved_path
                .as_ref()
                .and_then(|path| self.symbol_linker.file_at(path))
            {
                self.module_initialization_order(dependency.file_id, visited, modules);
            }
        }
        modules.push(module);
    }

    fn module_environment(&mut self, file_id: FileId) -> Environment {
        if let Some(environment) = self.module_env_cache.get(&file_id) {
            self.module_env_cache_hits += 1;
            return environment.clone();
        }
        self.module_env_cache_misses += 1;
        let mut environment = self
            .globals
            .iter()
            .filter(|(symbol, _)| symbol.file_id == file_id)
            .map(|(symbol, value)| (symbol.name.clone(), value.clone()))
            .collect::<Environment>();
        if !self.module_import_links.contains_key(&file_id) {
            let links = self
                .symbol_linker
                .file(file_id)
                .map(|file| {
                    file.flow
                        .imports
                        .iter()
                        .filter_map(|import| {
                            if let ValueResolution::Resolved(value) =
                                self.symbol_linker.resolve_binding(file_id, &import.local)
                            {
                                Some((import.local.clone(), value))
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            self.module_import_links.insert(file_id, links);
        }
        if let Some(links) = self.module_import_links.get(&file_id) {
            for (local, value) in links {
                match value {
                    LinkedValue::Declaration(symbol) => {
                        if let Some(value) = self.globals.get(symbol) {
                            environment.insert(local.clone(), value.clone());
                        }
                    }
                    LinkedValue::Namespace(module) => {
                        environment.insert(
                            local.clone(),
                            TrackedValue::plain(AbstractValue::Namespace(*module)),
                        );
                    }
                }
            }
        }
        self.module_env_cache.insert(file_id, environment.clone());
        environment
    }

    fn evaluate_global(&mut self, index: usize) {
        if self.initialized_globals.contains(&index) {
            return;
        }
        let (file_id, binding) = self.globals_ir[index];
        if !self.evaluating_globals.insert(index) {
            self.record_coverage_gap("cyclic module value initialization", &binding.span);
            return;
        }
        let environment = self.module_environment(file_id);
        let value = if matches!(&binding.pattern.kind, FlowPatternKind::Identifier { name } if self.model_symbol.file_id == file_id && self.model_symbol.name == *name)
        {
            TrackedValue::plain(AbstractValue::ModelFunction)
        } else {
            self.eval(&binding.value, &environment, file_id)
        };
        let mut bound = Environment::new();
        self.bind_pattern(
            &binding.pattern,
            value,
            &mut bound,
            RelationKind::ValueTransfer,
        );
        self.globals.extend(
            bound
                .into_iter()
                .map(|(name, value)| (LinkedSymbol { file_id, name }, value)),
        );
        self.module_env_cache.clear();
        self.evaluating_globals.remove(&index);
        self.initialized_globals.insert(index);
    }

    fn linked_value(&mut self, resolution: ValueResolution, span: &SourceSpan) -> TrackedValue {
        match resolution {
            ValueResolution::Resolved(LinkedValue::Namespace(file_id)) => {
                TrackedValue::plain(AbstractValue::Namespace(file_id))
            }
            ValueResolution::Resolved(LinkedValue::Declaration(symbol)) => {
                if symbol == self.model_symbol {
                    return TrackedValue::plain(AbstractValue::ModelFunction);
                }
                let key = FunctionKey {
                    file_id: symbol.file_id,
                    name: symbol.name.clone(),
                };
                if self.functions.contains_key(&key) {
                    return TrackedValue::plain(AbstractValue::Function(key));
                }
                if let Some(index) = self.global_bindings.get(&symbol).copied() {
                    self.evaluate_global(index);
                }
                self.globals
                    .get(&symbol)
                    .cloned()
                    .unwrap_or_else(|| TrackedValue::unknown("uninitialized_module_value"))
            }
            ValueResolution::Ambiguous | ValueResolution::Unresolved => {
                let reason = if resolution == ValueResolution::Ambiguous {
                    "ambiguous value linkage"
                } else {
                    "unresolved value linkage"
                };
                self.record_coverage_gap(reason, span);
                TrackedValue::unknown(reason)
            }
            ValueResolution::Missing => TrackedValue::unknown("missing_value_binding"),
        }
    }

    /// Returns the resolved file of a value import when that file is not in the snapshot.
    fn unparsed_import_target(&self, file_id: FileId, local: &str) -> Option<std::path::PathBuf> {
        let file = self.symbol_linker.file(file_id)?;
        let import = file
            .flow
            .imports
            .iter()
            .find(|import| import.local == local && !import.type_only)?;
        let path = self
            .symbol_linker
            .import_resolutions(&file.path)
            .find(|resolution| {
                resolution.specifier == import.module
                    && resolution.status == crate::link::ResolutionStatus::Resolved
            })
            .and_then(|resolution| resolution.resolved_path.as_ref())?;
        self.symbol_linker
            .file_at(path)
            .is_none()
            .then(|| path.clone())
    }

    fn request_import_for_local(&mut self, file_id: FileId, local: &str) {
        if let Some(path) = self.unparsed_import_target(file_id, local) {
            if self.current_reachability != Reachability::Unknown {
                self.root_requested_imports.insert(path.clone());
            }
            self.requested_imports.insert(path);
        }
    }

    /// Requests the file of an unknown component. Off the entry corridor this only feeds the
    /// general expansion when `general` is set; root paths follow corridor files, which can lead
    /// toward factory hosts, and leave other components to the possible-render assumption.
    fn request_component_import(&mut self, file_id: FileId, local: &str, general: bool) {
        let Some(path) = self.unparsed_import_target(file_id, local) else {
            return;
        };
        let on_corridor = self.entry_corridor.contains(&path);
        if on_corridor && self.current_reachability != Reachability::Unknown {
            self.root_requested_imports.insert(path.clone());
        }
        if general || on_corridor {
            self.requested_imports.insert(path);
        }
    }

    fn request_imported_callee(&mut self, file_id: FileId, callee: &FlowExpression) {
        let local = match &callee.kind {
            FlowExpressionKind::Identifier { name, .. } => Some(name.as_str()),
            FlowExpressionKind::StaticMember { object, .. } => {
                if let FlowExpressionKind::Identifier { name, .. } = &object.kind {
                    Some(name.as_str())
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(local) = local {
            self.request_import_for_local(file_id, local);
        }
    }

    fn record_coverage_gap(&mut self, reason: &str, span: &SourceSpan) {
        let gap = format!(
            "{reason} at file {} bytes {}..{}",
            span.file_id.0, span.start, span.end
        );
        if self.coverage_gap_keys.insert(gap.clone()) {
            self.coverage_gaps.push(gap.clone());
        }
        if self
            .query_gap_keys
            .insert((gap.clone(), self.current_choice.clone()))
        {
            self.query_gaps.push((
                gap,
                gap_kind(reason),
                span.clone(),
                self.current_choice.clone(),
            ));
        }
    }

    fn call_function(&mut self, key: &FunctionKey, arguments: Vec<TrackedValue>) -> TrackedValue {
        if self.call_depth >= MAX_CALL_DEPTH {
            self.mark_values_unresolved(
                arguments.iter(),
                "call depth budget exhausted",
                self.functions.get(key).map_or_else(
                    || fallback_span(key.file_id),
                    |function| function.span.clone(),
                ),
            );
            return TrackedValue::unknown("call_depth_budget_exhausted");
        }
        let Some(function) = self.functions.get(key).copied() else {
            return TrackedValue::unknown(format!("missing_function:{}", key.name));
        };
        let trace_len = self.trace.len();
        if self
            .trace
            .last()
            .is_none_or(|step| step.span != function.span)
        {
            self.trace.push(TraceStep {
                kind: QueryCallPathKind::Call,
                span: function.span.clone(),
            });
        }
        self.call_depth += 1;
        let mut environment = self.module_environment(key.file_id);
        for (index, pattern) in function.params.iter().enumerate() {
            let value = arguments
                .get(index)
                .cloned()
                .unwrap_or_else(|| TrackedValue::plain(AbstractValue::Undefined));
            self.bind_pattern(
                pattern,
                value,
                &mut environment,
                RelationKind::RenderPropBinding,
            );
        }
        self.active_captures.push(BTreeSet::new());
        let returned = self.execute_statements(&function.body, &mut environment, key.file_id);
        self.active_captures.pop();
        self.call_depth -= 1;
        self.trace.truncate(trace_len);
        let returned = returned.unwrap_or_else(|| TrackedValue::plain(AbstractValue::Undefined));
        if returns_capability_data(&returned) && self.symbol_linker.file(key.file_id).is_some_and(|file| {
            file.flow.exports.iter().any(|export| matches!(export, crate::ir::FlowExport::Local { local, type_only: false, .. } if local == &key.name))
        }) {
            self.capability_producer_files.insert(key.file_id);
        }
        returned
    }

    fn execute_statements(
        &mut self,
        statements: &[FlowStatement],
        environment: &mut Environment,
        file_id: FileId,
    ) -> Option<TrackedValue> {
        for (statement_index, statement) in statements.iter().enumerate() {
            match statement {
                FlowStatement::Bind(binding) => {
                    let value = self.eval(&binding.value, environment, file_id);
                    self.bind_pattern(
                        &binding.pattern,
                        value,
                        environment,
                        RelationKind::ValueTransfer,
                    );
                }
                FlowStatement::Expression { value, .. } => {
                    if !self.eval_array_push(value, environment, file_id) {
                        self.eval(value, environment, file_id);
                    }
                }
                FlowStatement::Assign {
                    target,
                    value,
                    span,
                } => {
                    let value = self.eval(value, environment, file_id);
                    self.assign_target(target, value, environment, file_id, span.clone());
                }
                FlowStatement::Return { value, span } => {
                    let mut value = value.as_ref().map_or_else(
                        || TrackedValue::plain(AbstractValue::Undefined),
                        |expression| self.eval(expression, environment, file_id),
                    );
                    let evidence = self.push_evidence(
                        RelationKind::Return,
                        "return_value",
                        span.clone(),
                        value.evidence.into_iter().collect(),
                        None,
                        "value returned from function",
                    );
                    value.evidence = Some(evidence);
                    return Some(value);
                }
                FlowStatement::If {
                    test,
                    consequent,
                    alternate,
                    span,
                } => {
                    let test_value = self.eval(test, environment, file_id);
                    self.push_evidence(
                        RelationKind::BranchDependency,
                        "branch_condition",
                        span.clone(),
                        test_value.evidence.into_iter().collect(),
                        None,
                        "control flow depends on this condition",
                    );
                    match truthy(&test_value.value) {
                        Some(true) => {
                            if let Some(value) =
                                self.execute_branch(consequent, environment, file_id)
                            {
                                return Some(value);
                            }
                        }
                        Some(false) => {
                            if let Some(value) =
                                self.execute_branch(alternate, environment, file_id)
                            {
                                return Some(value);
                            }
                        }
                        _ => {
                            let initial_heap = self.heap.clone();
                            let initial_versions = self.heap_versions.clone();
                            let mut consequent_environment = environment.clone();
                            let mut alternate_environment = environment.clone();
                            refine_environment_for_condition(
                                test,
                                true,
                                &mut consequent_environment,
                            );
                            refine_environment_for_condition(
                                test,
                                false,
                                &mut alternate_environment,
                            );
                            let left = self.execute_branch(
                                consequent,
                                &mut consequent_environment,
                                file_id,
                            );
                            let left_heap = self.heap.clone();
                            let left_versions = self.heap_versions.clone();
                            self.heap = initial_heap.clone();
                            self.heap_versions = initial_versions.clone();
                            let right =
                                self.execute_branch(alternate, &mut alternate_environment, file_id);
                            self.join_heap_branches(
                                &initial_heap,
                                &initial_versions,
                                &left_heap,
                                &left_versions,
                                span,
                            );
                            match (left, right) {
                                (Some(left), Some(right)) => {
                                    return Some(TrackedValue::plain(AbstractValue::union(vec![
                                        left, right,
                                    ])));
                                }
                                (Some(value), None) => {
                                    // The other path still executes the rest of the function.
                                    let continued = self
                                        .execute_statements(
                                            &statements[statement_index + 1..],
                                            &mut alternate_environment,
                                            file_id,
                                        )
                                        .unwrap_or_else(|| {
                                            TrackedValue::plain(AbstractValue::Undefined)
                                        });
                                    self.mark_values_unresolved(
                                        [&value, &continued].into_iter(),
                                        "unknown branch returns on only one path",
                                        span.clone(),
                                    );
                                    return Some(TrackedValue::plain(AbstractValue::union(vec![
                                        value, continued,
                                    ])));
                                }
                                (None, Some(value)) => {
                                    let continued = self
                                        .execute_statements(
                                            &statements[statement_index + 1..],
                                            &mut consequent_environment,
                                            file_id,
                                        )
                                        .unwrap_or_else(|| {
                                            TrackedValue::plain(AbstractValue::Undefined)
                                        });
                                    self.mark_values_unresolved(
                                        [&value, &continued].into_iter(),
                                        "unknown branch returns on only one path",
                                        span.clone(),
                                    );
                                    return Some(TrackedValue::plain(AbstractValue::union(vec![
                                        continued, value,
                                    ])));
                                }
                                (None, None) => {
                                    self.join_branch_environments(
                                        environment,
                                        &consequent_environment,
                                        &alternate_environment,
                                        span,
                                    );
                                }
                            }
                        }
                    }
                }
                FlowStatement::Unsupported(unsupported) => {
                    if unsupported.syntax == "switch_fallthrough_not_modeled" {
                        self.record_coverage_gap(
                            "switch fallthrough is not modeled",
                            &unsupported.span,
                        );
                    }
                    self.mark_values_unresolved(
                        environment.values(),
                        &format!("unsupported statement: {}", unsupported.syntax),
                        unsupported.span.clone(),
                    );
                }
            }
        }
        None
    }

    fn eval_array_push(
        &mut self,
        expression: &FlowExpression,
        environment: &mut Environment,
        file_id: FileId,
    ) -> bool {
        let FlowExpressionKind::Call { callee, arguments } = &expression.kind else {
            return false;
        };
        let FlowExpressionKind::StaticMember { object, property } = &callee.kind else {
            return false;
        };
        let FlowExpressionKind::Identifier { name, .. } = &object.kind else {
            return false;
        };
        if property != "push" {
            return false;
        }
        let Some(previous) = environment.get(name).map(|value| self.materialize(value)) else {
            return false;
        };
        let added = arguments
            .iter()
            .map(|argument| self.eval(argument, environment, file_id))
            .collect::<Vec<_>>();
        let Some(value) = append_array_values(&previous.value, &added) else {
            self.mark_values_unresolved(
                std::iter::once(&previous).chain(added.iter()),
                "array push receiver is not a finite local array",
                expression.span.clone(),
            );
            self.record_coverage_gap(
                "array push receiver is not a finite local array",
                &expression.span,
            );
            return true;
        };
        let evidence = self.push_evidence(
            RelationKind::Mutation,
            "append_local_array",
            expression.span.clone(),
            previous
                .evidence
                .into_iter()
                .chain(added.iter().filter_map(|value| value.evidence))
                .collect(),
            None,
            &format!("append to local array {name}"),
        );
        environment.insert(
            name.clone(),
            TrackedValue {
                value: value.clone(),
                evidence: Some(evidence),
                choice: previous.choice,
                heap_id: previous.heap_id,
            },
        );
        if let Some(id) = previous.heap_id {
            self.heap.insert(id, Rc::new(value));
            *self.heap_versions.entry(id).or_default() += 1;
        }
        true
    }

    fn join_branch_environments(
        &mut self,
        environment: &mut Environment,
        left: &Environment,
        right: &Environment,
        span: &SourceSpan,
    ) {
        let names = left
            .keys()
            .chain(right.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        for name in names {
            let old = environment.get(&name);
            let unchanged = |value: Option<&TrackedValue>| match (old, value) {
                (Some(old), Some(value)) => old.evidence == value.evidence,
                (None, None) => true,
                _ => false,
            };
            if unchanged(left.get(&name)) && unchanged(right.get(&name)) {
                continue;
            }
            let values = [left.get(&name), right.get(&name)].map(|value| {
                value
                    .cloned()
                    .unwrap_or_else(|| TrackedValue::plain(AbstractValue::Undefined))
            });
            self.mark_values_unresolved(
                values.iter(),
                "unknown branch changes a callback-bearing binding",
                span.clone(),
            );
            let parents = values.iter().filter_map(|value| value.evidence).collect();
            let evidence = self.push_evidence(
                RelationKind::ValueTransfer,
                "conservative_branch_join",
                span.clone(),
                parents,
                None,
                &format!("retain alternatives for {name} after an unknown branch"),
            );
            if let Some(id) = values[0].heap_id
                && values[1].heap_id == Some(id)
            {
                environment.insert(
                    name,
                    TrackedValue {
                        value: self
                            .heap
                            .get(&id)
                            .map_or_else(|| values[0].value.clone(), |value| (**value).clone()),
                        evidence: Some(evidence),
                        choice: self.current_choice.clone(),
                        heap_id: Some(id),
                    },
                );
                continue;
            }
            environment.insert(
                name,
                TrackedValue {
                    value: AbstractValue::union(values.into()),
                    evidence: Some(evidence),
                    choice: self.current_choice.clone(),
                    heap_id: None,
                },
            );
        }
    }

    fn execute_branch(
        &mut self,
        statements: &[FlowStatement],
        environment: &mut Environment,
        file_id: FileId,
    ) -> Option<TrackedValue> {
        let saved = statements
            .iter()
            .filter_map(|statement| match statement {
                FlowStatement::Bind(binding) if binding.lexical => {
                    Some(pattern_names(&binding.pattern))
                }
                _ => None,
            })
            .flatten()
            .map(|name| (name.to_owned(), environment.get(name).cloned()))
            .collect::<BTreeMap<_, _>>();
        let returned = self.execute_statements(statements, environment, file_id);
        for (name, value) in saved {
            if let Some(value) = value {
                environment.insert(name, value);
            } else {
                environment.remove(&name);
            }
        }
        returned
    }

    fn assign_target(
        &mut self,
        target: &FlowAssignmentTarget,
        mut value: TrackedValue,
        environment: &mut Environment,
        file_id: FileId,
        span: SourceSpan,
    ) {
        match target {
            FlowAssignmentTarget::Identifier { name } => {
                if self
                    .active_captures
                    .last()
                    .is_some_and(|captures| captures.contains(name))
                    || environment
                        .values()
                        .any(|value| captures_binding(value, name))
                {
                    self.mark_values_unresolved(
                        environment.values().chain(std::iter::once(&value)),
                        "reassignment of a captured binding requires live closure cells",
                        span.clone(),
                    );
                }
                let parents = environment
                    .get(name)
                    .and_then(|previous| previous.evidence)
                    .into_iter()
                    .chain(value.evidence)
                    .collect();
                let evidence = self.push_evidence(
                    RelationKind::Mutation,
                    "assign_identifier",
                    span,
                    parents,
                    None,
                    &format!("assign a new value to {name}"),
                );
                value.evidence = Some(evidence);
                environment.insert(name.clone(), value);
            }
            FlowAssignmentTarget::StaticMember { object, property } => {
                self.assign_property(object, property, value, environment, file_id, span);
            }
            FlowAssignmentTarget::ComputedMember { object, property } => {
                let property = self.eval(property, environment, file_id);
                if let AbstractValue::String(property) = property.value {
                    self.assign_property(object, &property, value, environment, file_id, span);
                } else {
                    let object = self.eval(object, environment, file_id);
                    self.mark_values_unresolved(
                        [&object, &value].into_iter(),
                        "computed mutation key is not a finite string",
                        span,
                    );
                }
            }
            FlowAssignmentTarget::Unsupported {
                syntax,
                span: target_span,
            } => self.mark_value_unresolved(
                &value,
                &format!("unsupported assignment target: {syntax}"),
                target_span.clone(),
            ),
        }
    }

    fn assign_property(
        &mut self,
        object: &FlowExpression,
        property: &str,
        mut value: TrackedValue,
        environment: &mut Environment,
        file_id: FileId,
        span: SourceSpan,
    ) {
        let FlowExpressionKind::Identifier { name, .. } = &object.kind else {
            let object = self.eval(object, environment, file_id);
            self.mark_values_unresolved(
                [&object, &value].into_iter(),
                "mutation target is not a directly tracked record binding",
                span,
            );
            return;
        };
        let Some(previous) = environment.get(name).map(|value| self.materialize(value)) else {
            self.mark_value_unresolved(&value, "mutation target binding is unknown", span);
            return;
        };
        let AbstractValue::Record(mut fields) = previous.value.clone() else {
            self.mark_values_unresolved(
                [&previous, &value].into_iter(),
                "mutation target is not a known record",
                span,
            );
            return;
        };
        let parents = fields
            .get(property)
            .and_then(|old| old.evidence)
            .into_iter()
            .chain(value.evidence)
            .collect();
        let evidence = self.push_evidence(
            RelationKind::Mutation,
            "assign_record_property",
            span,
            parents,
            None,
            &format!("assign property {name}.{property}"),
        );
        value.evidence = Some(evidence);
        Rc::make_mut(&mut fields).insert(property.to_owned(), value);
        let updated = AbstractValue::Record(fields);
        if let Some(id) = previous.heap_id {
            self.heap.insert(id, Rc::new(updated.clone()));
            *self.heap_versions.entry(id).or_default() += 1;
        }
        environment.insert(
            name.clone(),
            TrackedValue {
                value: updated,
                evidence: Some(evidence),
                choice: previous.choice,
                heap_id: previous.heap_id,
            },
        );
    }

    fn bind_pattern(
        &mut self,
        pattern: &FlowPattern,
        value: TrackedValue,
        environment: &mut Environment,
        relation: RelationKind,
    ) {
        match &pattern.kind {
            FlowPatternKind::Identifier { name } => {
                let evidence = self.push_evidence(
                    relation,
                    "bind_identifier",
                    pattern.span.clone(),
                    value.evidence.into_iter().collect(),
                    None,
                    &format!("bind value to {name}"),
                );
                environment.insert(
                    name.clone(),
                    TrackedValue {
                        evidence: Some(evidence),
                        ..value
                    },
                );
            }
            FlowPatternKind::Object { fields, rest } => {
                for field in fields {
                    let selected = self.read_property(
                        value.clone(),
                        &field.source_property,
                        field.target.span.clone(),
                        relation,
                    );
                    self.bind_pattern(&field.target, selected, environment, relation);
                }
                if let Some(rest) = rest {
                    let remainder = match self.materialize(&value).value {
                        AbstractValue::Record(mut values) => {
                            for field in fields {
                                Rc::make_mut(&mut values).remove(&field.source_property);
                            }
                            TrackedValue::plain(AbstractValue::Record(values))
                        }
                        _ => {
                            self.mark_value_unresolved(
                                &value,
                                "object rest source is not a known record",
                                pattern.span.clone(),
                            );
                            TrackedValue::unknown("unknown_object_rest")
                        }
                    };
                    self.bind_pattern(rest, remainder, environment, relation);
                }
            }
            FlowPatternKind::Array { elements } => {
                for (index, target) in elements.iter().enumerate() {
                    if let Some(target) = target {
                        let selected = self.read_property(
                            value.clone(),
                            &index.to_string(),
                            target.span.clone(),
                            relation,
                        );
                        self.bind_pattern(target, selected, environment, relation);
                    }
                }
            }
            FlowPatternKind::Unsupported { syntax } => {
                self.mark_value_unresolved(
                    &value,
                    &format!("unsupported binding pattern: {syntax}"),
                    pattern.span.clone(),
                );
            }
        }
    }

    fn eval(
        &mut self,
        expression: &FlowExpression,
        environment: &Environment,
        file_id: FileId,
    ) -> TrackedValue {
        if self.unreached_render_budget_active {
            self.unreached_render_evaluations += 1;
            if self.unreached_render_evaluations > MAX_UNREACHED_RENDER_EVALUATIONS {
                if !self.unreached_render_budget_reported {
                    self.mark_values_unresolved(
                        environment.values(),
                        "unreached component render budget exhausted",
                        expression.span.clone(),
                    );
                    self.record_coverage_gap(
                        "unreached component render budget exhausted",
                        &expression.span,
                    );
                    self.unreached_render_budget_reported = true;
                }
                return TrackedValue::unknown("unreached_render_budget_exhausted");
            }
        }
        if self.current_reachability == Reachability::Possible && self.assumed_budget_exhausted() {
            return TrackedValue::unknown("assumed_render_budget_exhausted");
        }
        if self.reverse_budget_active {
            self.reverse_evaluations += 1;
            if self.reverse_evaluations > MAX_REVERSE_IMPORTER_EVALUATIONS {
                if !self.reverse_budget_reported {
                    self.mark_values_unresolved(
                        environment.values(),
                        "reverse importer exploration budget exhausted",
                        expression.span.clone(),
                    );
                    self.record_coverage_gap(
                        "reverse importer exploration budget exhausted",
                        &expression.span,
                    );
                    self.reverse_budget_reported = true;
                }
                return TrackedValue::unknown("reverse_importer_budget_exhausted");
            }
        }
        let result = match &expression.kind {
            FlowExpressionKind::Null => TrackedValue::plain(AbstractValue::Null),
            FlowExpressionKind::String { value } => {
                TrackedValue::plain(AbstractValue::String(value.clone()))
            }
            FlowExpressionKind::Number { value } => {
                TrackedValue::plain(AbstractValue::Number(*value))
            }
            FlowExpressionKind::NumericEnumMember {
                enum_name,
                member_name,
                value,
            } => TrackedValue::plain(AbstractValue::EnumMember {
                enum_name: enum_name.clone(),
                member_name: member_name.clone(),
                value: *value,
            }),
            FlowExpressionKind::Boolean { value } => {
                TrackedValue::plain(AbstractValue::Boolean(*value))
            }
            FlowExpressionKind::Identifier {
                name,
                module_binding,
            } => {
                if let Some(value) = environment.get(name) {
                    let evidence = self.push_evidence(
                        RelationKind::ValueTransfer,
                        "read_binding",
                        expression.span.clone(),
                        value.evidence.into_iter().collect(),
                        None,
                        &format!("read {name}"),
                    );
                    return TrackedValue {
                        evidence: Some(evidence),
                        ..value.clone()
                    };
                }
                if name == "undefined" && !module_binding {
                    return TrackedValue::plain(AbstractValue::Undefined);
                }
                if !module_binding && name.contains('.') {
                    let key = FunctionKey {
                        file_id,
                        name: name.clone(),
                    };
                    if self.functions.contains_key(&key) {
                        return TrackedValue::plain(AbstractValue::Function(key));
                    }
                }
                if !module_binding {
                    return TrackedValue::unknown(format!("unresolved_local_identifier:{name}"));
                }
                let resolution = self.symbol_linker.resolve_binding(file_id, name);
                if resolution == ValueResolution::Missing
                    && self.symbol_linker.file(file_id).is_some_and(|file| {
                        file.flow.imports.iter().any(|import| import.local == *name)
                    })
                {
                    self.record_coverage_gap("missing imported value", &expression.span);
                }
                self.linked_value(resolution, &expression.span)
            }
            FlowExpressionKind::Record { fields } => {
                let values = fields
                    .iter()
                    .map(|field| {
                        (
                            field.property.clone(),
                            self.eval(&field.value, environment, file_id),
                        )
                    })
                    .collect();
                TrackedValue::plain(AbstractValue::record(values))
            }
            FlowExpressionKind::Array { elements } => {
                self.eval_array_literal(elements, environment, file_id, &expression.span)
            }
            FlowExpressionKind::Spread { value } => self.eval(value, environment, file_id),
            FlowExpressionKind::StaticMember { object, property } => {
                let object = self.eval(object, environment, file_id);
                self.read_property(
                    object,
                    property,
                    expression.span.clone(),
                    RelationKind::ValueTransfer,
                )
            }
            FlowExpressionKind::ComputedMember { object, property } => {
                let object = self.eval(object, environment, file_id);
                let property = self.eval(property, environment, file_id);
                let property_name = match &property.value {
                    AbstractValue::String(value) => value.clone(),
                    AbstractValue::Number(value) => value.to_string(),
                    _ => {
                        self.mark_value_unresolved(
                            &object,
                            "computed property is not a finite string",
                            expression.span.clone(),
                        );
                        return TrackedValue::unknown("unknown_computed_property");
                    }
                };
                self.read_property(
                    object,
                    &property_name,
                    expression.span.clone(),
                    RelationKind::KeySelection,
                )
            }
            FlowExpressionKind::StrictEquality {
                left,
                right,
                negated,
            } => {
                let left = self.eval(left, environment, file_id);
                let right = self.eval(right, environment, file_id);
                let value = exact_equality(&left.value, &right.value).map_or_else(
                    || AbstractValue::Unknown("non_finite_strict_equality".to_owned()),
                    |equal| AbstractValue::Boolean(equal != *negated),
                );
                let evidence = self.push_evidence(
                    RelationKind::Derivation,
                    "strict_equality",
                    expression.span.clone(),
                    left.evidence.into_iter().chain(right.evidence).collect(),
                    None,
                    "derive exact strict-equality result",
                );
                TrackedValue {
                    value,
                    evidence: Some(evidence),
                    choice: left.choice.or(right.choice),
                    heap_id: None,
                }
            }
            FlowExpressionKind::LooseNullEquality { value, negated } => {
                let value = self.eval(value, environment, file_id);
                TrackedValue {
                    value: nullish(&value.value).map_or_else(
                        || AbstractValue::Unknown("non_finite_null_equality".to_owned()),
                        |is_nullish| AbstractValue::Boolean(is_nullish != *negated),
                    ),
                    evidence: value.evidence,
                    choice: value.choice,
                    heap_id: None,
                }
            }
            FlowExpressionKind::LogicalNot { value } => {
                let value = self.eval(value, environment, file_id);
                TrackedValue {
                    value: truthy(&value.value).map_or_else(
                        || AbstractValue::Unknown("unknown_logical_not".to_owned()),
                        |truthy| AbstractValue::Boolean(!truthy),
                    ),
                    evidence: value.evidence,
                    choice: value.choice,
                    heap_id: None,
                }
            }
            FlowExpressionKind::Logical {
                left,
                right,
                operator,
            } => {
                let left = self.eval(left, environment, file_id);
                let short_circuits = match operator {
                    FlowLogicalOperator::And => truthy(&left.value).map(|value| !value),
                    FlowLogicalOperator::Or => truthy(&left.value),
                    FlowLogicalOperator::Coalesce => nullish(&left.value).map(|value| !value),
                };
                match short_circuits {
                    Some(true) => left,
                    Some(false) => self.eval(right, environment, file_id),
                    None => TrackedValue::plain(AbstractValue::union(vec![
                        left,
                        self.eval(right, environment, file_id),
                    ])),
                }
            }
            FlowExpressionKind::Conditional {
                test,
                consequent,
                alternate,
            } => {
                let test = self.eval(test, environment, file_id);
                self.push_evidence(
                    RelationKind::BranchDependency,
                    "conditional_expression",
                    expression.span.clone(),
                    test.evidence.into_iter().collect(),
                    None,
                    "conditional expression depends on this condition",
                );
                match truthy(&test.value) {
                    Some(true) => self.eval(consequent, environment, file_id),
                    Some(false) => self.eval(alternate, environment, file_id),
                    _ => TrackedValue::plain(AbstractValue::union(vec![
                        self.eval(consequent, environment, file_id),
                        self.eval(alternate, environment, file_id),
                    ])),
                }
            }
            FlowExpressionKind::Call { callee, arguments } => {
                if let Some(wrapper) = self
                    .symbol_linker
                    .file(file_id)
                    .and_then(|file| self.project.config.component_wrapper(&file.flow, callee))
                    .cloned()
                {
                    let Some(component) = arguments.get(wrapper.component_argument) else {
                        self.record_coverage_gap(
                            "configured component wrapper argument is missing",
                            &expression.span,
                        );
                        return TrackedValue::unknown("missing_wrapped_component");
                    };
                    let mut component = self.eval(component, environment, file_id);
                    let evidence = self.push_evidence(
                        RelationKind::ValueTransfer,
                        "configured_component_wrapper",
                        expression.span.clone(),
                        component.evidence.into_iter().collect(),
                        None,
                        "configured wrapper may render its component argument",
                    );
                    component.evidence = Some(evidence);
                    return component;
                }
                if let Some(property) = self.symbol_linker.file(file_id).and_then(|file| {
                    self.project
                        .config
                        .lazy_factory_property(&file.flow, callee)
                }) && let Some(module) = lazy_component_import(expression, property)
                {
                    let target = self
                        .symbol_linker
                        .file(file_id)
                        .and_then(|file| {
                            self.symbol_linker
                                .import_resolutions(&file.path)
                                .find(|resolution| resolution.specifier == module)
                        })
                        .and_then(|resolution| resolution.resolved_path.clone());
                    if let Some(target) = target {
                        if let Some(file) = self.symbol_linker.file_at(&target) {
                            let exported = self
                                .symbol_linker
                                .resolve_exported_value(file.file_id, "default");
                            return self.linked_value(exported, &expression.span);
                        }
                        if self.current_reachability != Reachability::Unknown {
                            self.root_requested_imports.insert(target.clone());
                        }
                        self.requested_imports.insert(target);
                    } else {
                        self.record_coverage_gap(
                            "lazy component import did not resolve",
                            &expression.span,
                        );
                    }
                    return TrackedValue::unknown("unresolved_lazy_component");
                }
                if let FlowExpressionKind::StaticMember { object, property } = &callee.kind
                    && property == "values"
                    && matches!(&object.kind, FlowExpressionKind::Identifier { name, .. } if name == "Object")
                    && !environment.contains_key("Object")
                {
                    let source = arguments.first().map_or_else(
                        || TrackedValue::unknown("missing_object_values_argument"),
                        |argument| self.eval(argument, environment, file_id),
                    );
                    if object_values_order_is_unknown(&source.value) {
                        self.record_coverage_gap(
                            "Object.values record key order is not modeled",
                            &expression.span,
                        );
                    }
                    return object_values(&source.value).map_or_else(
                        || TrackedValue::unknown("object_values_unknown_record"),
                        TrackedValue::plain,
                    );
                }
                if let Some(index) = self.configured_callback_selector(file_id, callee) {
                    let callback = arguments.get(index).map_or_else(
                        || TrackedValue::unknown("missing_configured_selector_callback"),
                        |argument| self.eval(argument, environment, file_id),
                    );
                    return self.invoke_value(callback, Vec::new(), expression.span.clone());
                }
                if let Some(kind) = self.known_hook_call(file_id, callee) {
                    match kind {
                        "useMemo" => {
                            let callback = arguments.first().map_or_else(
                                || TrackedValue::unknown("missing_use_memo_callback"),
                                |argument| self.eval(argument, environment, file_id),
                            );
                            let returned =
                                self.invoke_value(callback, Vec::new(), expression.span.clone());
                            if self.model.scan_callback_bodies {
                                self.scan_callback_bodies(&returned, &expression.span, 0);
                            }
                            return returned;
                        }
                        "useCallback" | "memo" | "forwardRef" => {
                            let callback = arguments.first().map_or_else(
                                || TrackedValue::unknown("missing_react_callback_argument"),
                                |argument| self.eval(argument, environment, file_id),
                            );
                            if self.model.scan_callback_bodies {
                                self.scan_callback_bodies(&callback, &expression.span, 0);
                            }
                            return callback;
                        }
                        "createContext" => {
                            // A context Provider renders its children; a Consumer calls its
                            // function child with the current value.
                            let component = |export: &str, forward_children, invoke_children| {
                                TrackedValue::plain(AbstractValue::ConfiguredComponent(
                                    ComponentConsumer {
                                        module: "react".to_owned(),
                                        export: format!("createContext().{export}"),
                                        forward_children,
                                        invoke_children,
                                        render_props: Vec::new(),
                                        render_callback_names: Vec::new(),
                                        component_props: Vec::new(),
                                    },
                                ))
                            };
                            return TrackedValue::plain(AbstractValue::record(BTreeMap::from([
                                ("Provider".to_owned(), component("Provider", true, false)),
                                ("Consumer".to_owned(), component("Consumer", false, true)),
                            ])));
                        }
                        "useState" => {
                            // The initializer only describes the first render. A later render
                            // can observe a setter update, including one scheduled by an effect.
                            // Keep the initial value and an unknown later value so guards in
                            // callbacks cannot silently suppress possible invocations.
                            let initial = arguments.first().map_or_else(
                                || TrackedValue::plain(AbstractValue::Undefined),
                                |argument| self.eval(argument, environment, file_id),
                            );
                            return TrackedValue::plain(AbstractValue::array(vec![
                                TrackedValue::plain(AbstractValue::union(vec![
                                    initial,
                                    TrackedValue::unknown("react_state_after_update"),
                                ])),
                                TrackedValue::unknown("react_state_setter"),
                            ]));
                        }
                        _ => {}
                    }
                }
                if let FlowExpressionKind::StaticMember { object, property } = &callee.kind
                    && property == "map"
                {
                    return self.eval_array_map(
                        object,
                        arguments,
                        environment,
                        file_id,
                        expression.span.clone(),
                    );
                }
                if let FlowExpressionKind::StaticMember { object, property } = &callee.kind
                    && property == "filter"
                {
                    let receiver = self.eval(object, environment, file_id);
                    let callback = arguments.first().map_or_else(
                        || TrackedValue::unknown("missing_filter_callback"),
                        |argument| self.eval(argument, environment, file_id),
                    );
                    return self.eval_array_filter_value(receiver, &callback, &expression.span);
                }
                let callee_value = self.eval(callee, environment, file_id);
                let arguments = arguments
                    .iter()
                    .map(|argument| self.eval(argument, environment, file_id))
                    .collect::<Vec<_>>();
                if matches!(&callee_value.value, AbstractValue::Unknown(_))
                    && arguments
                        .iter()
                        .any(|argument| !capability_ids(argument).is_empty())
                {
                    self.request_imported_callee(file_id, callee);
                }
                self.invoke_value(callee_value, arguments, expression.span.clone())
            }
            FlowExpressionKind::Arrow { params, body } => {
                let mut references = ClosureReferences::default();
                collect_body_references(body, &mut references);
                let mut captured = environment
                    .iter()
                    .filter(|(name, _)| {
                        references.has_unsupported || references.names.contains(*name)
                    })
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect::<Environment>();
                for pattern in params {
                    for name in pattern_names(pattern) {
                        captured.remove(name);
                    }
                }
                for name in references.modules {
                    if let std::collections::btree_map::Entry::Vacant(entry) = captured.entry(name)
                    {
                        let resolution = self.symbol_linker.resolve_binding(file_id, entry.key());
                        let value = self.linked_value(resolution, &expression.span);
                        entry.insert(value);
                    }
                }
                let mut parents = captured
                    .values()
                    .flat_map(|value| {
                        value.evidence.into_iter().chain(
                            self.value_capability_ids(value)
                                .into_iter()
                                .map(|id| self.capabilities[id].origin),
                        )
                    })
                    .collect::<Vec<_>>();
                parents.sort();
                parents.dedup();
                let evidence = (!parents.is_empty()).then(|| {
                    self.push_evidence(
                        RelationKind::Capture,
                        "closure_capture",
                        expression.span.clone(),
                        parents,
                        None,
                        "inline wrapper captures tracked callback state",
                    )
                });
                TrackedValue {
                    value: AbstractValue::Closure(Rc::new(ClosureValue {
                        body: body.clone(),
                        params: params.clone(),
                        environment: captured,
                        file_id,
                        span: expression.span.clone(),
                    })),
                    evidence,
                    choice: self.current_choice.clone(),
                    heap_id: None,
                }
            }
            FlowExpressionKind::JsxElement { tag, props } => {
                self.create_element(tag, props, expression, environment, file_id)
            }
            FlowExpressionKind::DynamicImport { .. } => {
                TrackedValue::unknown("unmodeled_dynamic_import")
            }
            FlowExpressionKind::Unsupported { syntax, references } => {
                if syntax == "symbolic_for_of_iteration" {
                    self.record_coverage_gap(
                        "for-of loop explored for zero or one iteration",
                        &expression.span,
                    );
                }
                self.mark_values_unresolved(
                    references.iter().filter_map(|name| environment.get(name)),
                    &format!("unsupported expression: {syntax}"),
                    expression.span.clone(),
                );
                TrackedValue::unknown(syntax.clone())
            }
        };
        self.track_heap(result)
    }

    fn track_heap(&mut self, mut value: TrackedValue) -> TrackedValue {
        if value.heap_id.is_none()
            && matches!(
                value.value,
                AbstractValue::Record(_) | AbstractValue::Array(_)
            )
        {
            let id = self.next_heap_id;
            self.next_heap_id += 1;
            self.heap.insert(id, Rc::new(value.value.clone()));
            self.heap_versions.insert(id, 0);
            value.heap_id = Some(id);
        }
        value
    }

    fn materialize(&self, value: &TrackedValue) -> TrackedValue {
        let mut result = value.clone();
        if let Some(id) = result.heap_id {
            if let Some(current) = self.heap.get(&id) {
                result.value = (**current).clone();
            }
        }
        result
    }

    fn join_heap_branches(
        &mut self,
        initial: &BTreeMap<u64, Rc<AbstractValue>>,
        initial_versions: &BTreeMap<u64, u64>,
        left: &BTreeMap<u64, Rc<AbstractValue>>,
        left_versions: &BTreeMap<u64, u64>,
        span: &SourceSpan,
    ) {
        let right = self.heap.clone();
        let right_versions = self.heap_versions.clone();
        for id in initial
            .keys()
            .chain(left.keys())
            .chain(right.keys())
            .copied()
            .collect::<BTreeSet<_>>()
        {
            let left_value = left.get(&id).or_else(|| initial.get(&id));
            let right_value = right.get(&id).or_else(|| initial.get(&id));
            let baseline = initial_versions.get(&id).copied().unwrap_or(0);
            let left_version = left_versions.get(&id).copied().unwrap_or(baseline);
            let right_version = right_versions.get(&id).copied().unwrap_or(baseline);
            let joined = match (left_value, right_value) {
                (Some(left), Some(right))
                    if left_version != baseline || right_version != baseline =>
                {
                    let mut alternatives = Vec::new();
                    collect_heap_alternatives(left, &mut alternatives);
                    collect_heap_alternatives(right, &mut alternatives);
                    if alternatives.len() > MAX_HEAP_ALTERNATIVES {
                        self.record_coverage_gap("heap alternative budget exhausted", span);
                        AbstractValue::Unknown("heap_alternative_budget_exhausted".to_owned())
                    } else {
                        AbstractValue::union(alternatives)
                    }
                }
                (Some(value), _) | (_, Some(value)) => (**value).clone(),
                (None, None) => continue,
            };
            self.heap.insert(id, Rc::new(joined));
            self.heap_versions.insert(
                id,
                left_version.max(right_version)
                    + u64::from(left_version != baseline || right_version != baseline),
            );
        }
    }

    fn known_hook_call(&self, file_id: FileId, callee: &FlowExpression) -> Option<&'static str> {
        let file = self.symbol_linker.file(file_id)?;
        match &callee.kind {
            FlowExpressionKind::StaticMember { object, property } => {
                let FlowExpressionKind::Identifier { name, .. } = &object.kind else {
                    return None;
                };
                file.flow
                    .imports
                    .iter()
                    .any(|import| import.local == *name && import.module == "react")
                    .then(|| match property.as_str() {
                        "useMemo" => Some("useMemo"),
                        "useCallback" => Some("useCallback"),
                        "useState" => Some("useState"),
                        "memo" => Some("memo"),
                        "forwardRef" => Some("forwardRef"),
                        "createContext" => Some("createContext"),
                        _ => None,
                    })
                    .flatten()
            }
            FlowExpressionKind::Identifier { name, .. } => file
                .flow
                .imports
                .iter()
                .find(|import| import.local == *name)
                .and_then(
                    |import| match (import.module.as_str(), import.imported.as_str()) {
                        ("react", "useMemo") => Some("useMemo"),
                        ("react", "useCallback") => Some("useCallback"),
                        ("react", "useState") => Some("useState"),
                        ("react", "memo") => Some("memo"),
                        ("react", "forwardRef") => Some("forwardRef"),
                        ("react", "createContext") => Some("createContext"),
                        _ => None,
                    },
                ),
            _ => None,
        }
    }

    fn configured_callback_selector(
        &self,
        file_id: FileId,
        callee: &FlowExpression,
    ) -> Option<usize> {
        let file = self.symbol_linker.file(file_id)?;
        let (local, export) = match &callee.kind {
            FlowExpressionKind::Identifier { name, .. } => (name.as_str(), None),
            FlowExpressionKind::StaticMember { object, property } => {
                let FlowExpressionKind::Identifier { name, .. } = &object.kind else {
                    return None;
                };
                (name.as_str(), Some(property.as_str()))
            }
            _ => return None,
        };
        let import = file
            .flow
            .imports
            .iter()
            .find(|import| import.local == local && !import.type_only)?;
        let imported_name = match export {
            Some(name) if import.imported == "*" => name,
            Some(_) => return None,
            None => &import.imported,
        };
        self.project
            .config
            .callback_selector_imports
            .iter()
            .find(|selector| selector.module == import.module && selector.export == imported_name)
            .map(|selector| selector.callback_argument)
    }

    fn invoke_value(
        &mut self,
        callee: TrackedValue,
        arguments: Vec<TrackedValue>,
        span: SourceSpan,
    ) -> TrackedValue {
        let trace_len = self.trace.len();
        self.trace.push(TraceStep {
            kind: QueryCallPathKind::Call,
            span: span.clone(),
        });
        let callee = match callee.value {
            AbstractValue::AssumedWrapper { reason, .. } => TrackedValue {
                value: AbstractValue::Unknown(reason),
                ..callee
            },
            _ => callee,
        };
        let result = match callee.value {
            AbstractValue::ModelFunction => self.call_model(&arguments, span),
            AbstractValue::Capability(capability) => {
                let projected_arguments = arguments.iter().map(query_value).collect::<Vec<_>>();
                let call_path = self.trace_at(QueryCallPathKind::Invocation, &span);
                if self.capabilities.get(capability).is_some_and(|state| {
                    state.invocations.iter().any(|invocation| {
                        self.evidence[invocation.evidence.0 as usize].span == span
                            && invocation.call_path == call_path
                            && invocation
                                .arguments
                                .iter()
                                .map(query_value)
                                .collect::<Vec<_>>()
                                == projected_arguments
                    })
                }) {
                    self.trace.truncate(trace_len);
                    return TrackedValue::plain(AbstractValue::Undefined);
                }
                let evidence = self.push_evidence(
                    RelationKind::Invocation,
                    "invoke_capability",
                    span.clone(),
                    callee.evidence.into_iter().collect(),
                    Some(self.model.id.clone()),
                    "matching callback capability is invoked",
                );
                if let Some(state) = self.capabilities.get_mut(capability) {
                    state.invocations.push(InvocationState {
                        evidence,
                        arguments: arguments.clone(),
                        call_path,
                    });
                }
                self.emit_modeled_effects(capability, evidence, span);
                TrackedValue::plain(AbstractValue::Undefined)
            }
            AbstractValue::Closure(closure) => {
                let returned = self.call_closure(&closure, arguments.clone());
                if self.model.scan_callback_bodies {
                    for argument in &arguments {
                        self.scan_callback_bodies(argument, &span, 0);
                    }
                }
                returned
            }
            AbstractValue::Function(key) => {
                let returned = self.call_function(&key, arguments.clone());
                if self.model.scan_callback_bodies {
                    for argument in &arguments {
                        self.scan_callback_bodies(argument, &span, 0);
                    }
                }
                returned
            }
            AbstractValue::Unknown(reason) => {
                self.mark_values_unresolved(
                    arguments.iter(),
                    &format!("call through unknown target: {reason}"),
                    span.clone(),
                );
                if self.model.scan_callback_bodies {
                    for argument in &arguments {
                        self.scan_callback_bodies(argument, &span, 0);
                    }
                }
                let wrapped = arguments
                    .iter()
                    .filter(|argument| self.is_component_like(argument))
                    .cloned()
                    .collect::<Vec<_>>();
                if wrapped.is_empty() {
                    TrackedValue::unknown("unknown_call_result")
                } else {
                    TrackedValue::plain(AbstractValue::AssumedWrapper {
                        reason: "unknown_call_result".to_owned(),
                        wrapped: Rc::new(wrapped),
                    })
                }
            }
            _ => {
                self.mark_values_unresolved(
                    std::iter::once(&callee).chain(arguments.iter()),
                    "value passed to unsupported call target",
                    span.clone(),
                );
                if self.model.scan_callback_bodies {
                    for argument in &arguments {
                        self.scan_callback_bodies(argument, &span, 0);
                    }
                }
                TrackedValue::unknown("unsupported_call_target")
            }
        };
        self.trace.truncate(trace_len);
        result
    }

    fn scan_callback_bodies(&mut self, value: &TrackedValue, span: &SourceSpan, depth: usize) {
        if depth >= 16 || capability_ids(value).is_empty() {
            return;
        }
        match &value.value {
            AbstractValue::Closure(closure) => {
                self.record_coverage_gap("callback body explored through opaque consumer", span);
                let returned = self.call_closure(closure, Vec::new());
                self.scan_callback_bodies(&returned, span, depth + 1);
            }
            AbstractValue::Array(values) | AbstractValue::Union(values) => {
                for value in values.iter() {
                    self.scan_callback_bodies(value, span, depth + 1);
                }
            }
            AbstractValue::Record(fields) => {
                for value in fields.values() {
                    self.scan_callback_bodies(value, span, depth + 1);
                }
            }
            AbstractValue::Element(element) => {
                for value in element.props.values() {
                    self.scan_callback_bodies(value, span, depth + 1);
                }
            }
            _ => {}
        }
    }

    fn eval_array_literal(
        &mut self,
        elements: &[FlowExpression],
        environment: &Environment,
        file_id: FileId,
        span: &SourceSpan,
    ) -> TrackedValue {
        let mut alternatives = vec![Vec::new()];
        for element in elements {
            let (parts, is_spread) = match &element.kind {
                FlowExpressionKind::Spread { value } => {
                    let spread = self.eval(value, environment, file_id);
                    match finite_array_parts(&spread.value) {
                        Some(parts) => (parts, true),
                        None => {
                            self.mark_value_unresolved(
                                &spread,
                                "array spread is not a finite array",
                                element.span.clone(),
                            );
                            (
                                vec![vec![TrackedValue::unknown("unknown_array_spread")]],
                                true,
                            )
                        }
                    }
                }
                _ => (vec![vec![self.eval(element, environment, file_id)]], false),
            };
            if alternatives.len().saturating_mul(parts.len()) > 64 {
                self.record_coverage_gap(
                    "array construction exceeded finite alternatives budget",
                    span,
                );
                return TrackedValue::unknown("array_alternatives_budget_exhausted");
            }
            alternatives = alternatives
                .into_iter()
                .flat_map(|prefix| {
                    parts.iter().map(move |part| {
                        let mut merged = prefix.clone();
                        if is_spread {
                            merged.extend(part.clone());
                        } else {
                            merged.push(part[0].clone());
                        }
                        merged
                    })
                })
                .collect();
        }
        let mut values = alternatives
            .into_iter()
            .map(|elements| TrackedValue::plain(AbstractValue::array(elements)));
        let first = values
            .next()
            .unwrap_or_else(|| TrackedValue::plain(AbstractValue::array(Vec::new())));
        if let Some(second) = values.next() {
            TrackedValue::plain(AbstractValue::union(
                std::iter::once(first)
                    .chain(std::iter::once(second))
                    .chain(values)
                    .collect(),
            ))
        } else {
            first
        }
    }

    fn eval_array_map(
        &mut self,
        object: &FlowExpression,
        arguments: &[FlowExpression],
        environment: &Environment,
        file_id: FileId,
        span: SourceSpan,
    ) -> TrackedValue {
        let receiver = self.eval(object, environment, file_id);
        let callback = arguments.first().map_or_else(
            || TrackedValue::unknown("missing_map_callback"),
            |callback| self.eval(callback, environment, file_id),
        );
        self.eval_array_map_value(receiver, &callback, &span)
    }

    fn eval_array_map_value(
        &mut self,
        receiver: TrackedValue,
        callback: &TrackedValue,
        span: &SourceSpan,
    ) -> TrackedValue {
        match receiver.value {
            AbstractValue::Array(elements) => {
                let original = TrackedValue::plain(AbstractValue::Array(elements.clone()));
                let mapped = elements
                    .iter()
                    .cloned()
                    .enumerate()
                    .map(|(index, element)| {
                        self.invoke_value(
                            callback.clone(),
                            vec![
                                element,
                                i64::try_from(index).map_or_else(
                                    |_| TrackedValue::unknown("array_index_out_of_range"),
                                    |index| TrackedValue::plain(AbstractValue::Number(index)),
                                ),
                                original.clone(),
                            ],
                            span.clone(),
                        )
                    })
                    .collect();
                TrackedValue::plain(AbstractValue::array(mapped))
            }
            AbstractValue::Union(values) => TrackedValue::plain(AbstractValue::union(
                Rc::unwrap_or_clone(values)
                    .into_iter()
                    .map(|value| self.eval_array_map_value(value, callback, span))
                    .collect(),
            )),
            AbstractValue::Unknown(reason) => {
                let before = self.capabilities.len();
                let mut affected = capability_ids(&callback);
                let mapped = self.invoke_value(
                    callback.clone(),
                    vec![TrackedValue::unknown("symbolic_array_element")],
                    span.clone(),
                );
                affected.extend(capability_ids(&mapped));
                affected.extend(before..self.capabilities.len());
                affected.sort_unstable();
                affected.dedup();
                if !affected.is_empty() {
                    self.record_coverage_gap(
                        &format!("array map receiver is unknown ({reason})"),
                        span,
                    );
                    for capability in affected {
                        let evidence = self.push_evidence(
                            RelationKind::UnresolvedEscape,
                            "unknown_array_map_receiver",
                            span.clone(),
                            Vec::new(),
                            None,
                            "array map receiver is unknown, so iteration coverage is incomplete",
                        );
                        if let Some(state) = self.capabilities.get_mut(capability) {
                            state.unresolved.push(evidence);
                        }
                    }
                }
                TrackedValue::plain(AbstractValue::array(vec![mapped]))
            }
            _ => {
                self.mark_value_unresolved(
                    &receiver,
                    "map receiver is not a known array",
                    span.clone(),
                );
                TrackedValue::unknown("unsupported_map_receiver")
            }
        }
    }

    fn eval_array_filter_value(
        &mut self,
        receiver: TrackedValue,
        callback: &TrackedValue,
        span: &SourceSpan,
    ) -> TrackedValue {
        match receiver.value {
            AbstractValue::Array(elements) if elements.len() <= 8 => {
                let original = TrackedValue::plain(AbstractValue::Array(elements.clone()));
                let mut subsets = vec![Vec::new()];
                for (index, element) in elements.iter().cloned().enumerate() {
                    let predicate = self.invoke_value(
                        callback.clone(),
                        vec![
                            element.clone(),
                            i64::try_from(index).map_or_else(
                                |_| TrackedValue::unknown("array_index_out_of_range"),
                                |index| TrackedValue::plain(AbstractValue::Number(index)),
                            ),
                            original.clone(),
                        ],
                        span.clone(),
                    );
                    match truthy(&predicate.value) {
                        Some(true) => subsets
                            .iter_mut()
                            .for_each(|subset| subset.push(element.clone())),
                        Some(false) => {}
                        None => {
                            self.record_coverage_gap(
                                "filter predicate is unknown; both outcomes retained",
                                span,
                            );
                            let mut included = subsets.clone();
                            included
                                .iter_mut()
                                .for_each(|subset| subset.push(element.clone()));
                            subsets.extend(included);
                        }
                    }
                }
                let mut alternatives = subsets
                    .into_iter()
                    .map(|elements| TrackedValue::plain(AbstractValue::array(elements)));
                let first = alternatives
                    .next()
                    .unwrap_or_else(|| TrackedValue::plain(AbstractValue::array(Vec::new())));
                if let Some(second) = alternatives.next() {
                    TrackedValue::plain(AbstractValue::union(
                        std::iter::once(first)
                            .chain(std::iter::once(second))
                            .chain(alternatives)
                            .collect(),
                    ))
                } else {
                    first
                }
            }
            AbstractValue::Union(values) => TrackedValue::plain(AbstractValue::union(
                Rc::unwrap_or_clone(values)
                    .into_iter()
                    .map(|value| self.eval_array_filter_value(value, callback, span))
                    .collect(),
            )),
            _ => {
                self.mark_value_unresolved(
                    &receiver,
                    "filter receiver is not a bounded array",
                    span.clone(),
                );
                TrackedValue::unknown("unknown_filter_receiver")
            }
        }
    }

    fn create_element(
        &mut self,
        tag: &FlowJsxTag,
        props: &[FlowJsxProp],
        expression: &FlowExpression,
        environment: &Environment,
        file_id: FileId,
    ) -> TrackedValue {
        let configured = match tag {
            FlowJsxTag::Identifier {
                name,
                intrinsic: false,
                ..
            }
            | FlowJsxTag::Member { object: name, .. } => self
                .symbol_linker
                .file(file_id)
                .and_then(|file| {
                    let object = FlowExpression {
                        kind: FlowExpressionKind::Identifier {
                            name: name.clone(),
                            module_binding: true,
                        },
                        span: expression.span.clone(),
                    };
                    let reference = match tag {
                        FlowJsxTag::Member { property, .. } => FlowExpression {
                            kind: FlowExpressionKind::StaticMember {
                                object: Box::new(object),
                                property: property.clone(),
                            },
                            span: expression.span.clone(),
                        },
                        _ => object,
                    };
                    self.project
                        .config
                        .component_consumer(&file.flow, &reference)
                })
                .cloned(),
            _ => None,
        };
        let component = if let Some(model) = configured {
            TrackedValue::plain(AbstractValue::ConfiguredComponent(model))
        } else {
            match tag {
                FlowJsxTag::Identifier {
                    name,
                    intrinsic: true,
                    ..
                } => TrackedValue::plain(AbstractValue::Intrinsic(name.clone())),
                FlowJsxTag::Identifier {
                    name,
                    intrinsic: false,
                    module_binding,
                } => self.eval(
                    &FlowExpression {
                        kind: FlowExpressionKind::Identifier {
                            name: name.clone(),
                            module_binding: *module_binding,
                        },
                        span: expression.span.clone(),
                    },
                    environment,
                    file_id,
                ),
                FlowJsxTag::Unsupported { syntax } => TrackedValue::unknown(syntax.clone()),
                FlowJsxTag::Member {
                    object,
                    property,
                    module_binding,
                } => self.eval(
                    &FlowExpression {
                        kind: FlowExpressionKind::StaticMember {
                            object: Box::new(FlowExpression {
                                kind: FlowExpressionKind::Identifier {
                                    name: object.clone(),
                                    module_binding: *module_binding,
                                },
                                span: expression.span.clone(),
                            }),
                            property: property.clone(),
                        },
                        span: expression.span.clone(),
                    },
                    environment,
                    file_id,
                ),
            }
        };
        let mut values = BTreeMap::new();
        for prop in props {
            match prop {
                FlowJsxProp::Property { name, value, span } => {
                    let mut value = self.eval(value, environment, file_id);
                    let evidence = self.push_evidence(
                        RelationKind::RenderPropBinding,
                        "jsx_prop",
                        span.clone(),
                        value.evidence.into_iter().collect(),
                        None,
                        &format!("value supplied as JSX prop {name}"),
                    );
                    value.evidence = Some(evidence);
                    values.insert(name.clone(), value);
                }
                FlowJsxProp::Spread { value, span } => {
                    let spread = self.eval(value, environment, file_id);
                    if let AbstractValue::Record(fields) = spread.value {
                        values.extend(Rc::unwrap_or_clone(fields));
                    } else {
                        self.mark_value_unresolved(
                            &spread,
                            "JSX spread value is not a known record",
                            span.clone(),
                        );
                    }
                }
                FlowJsxProp::Unsupported(unsupported) => self.mark_values_unresolved(
                    environment.values(),
                    &format!("unsupported JSX prop: {}", unsupported.syntax),
                    unsupported.span.clone(),
                ),
            }
        }
        if matches!(&component.value, AbstractValue::Unknown(_)) {
            let local = match tag {
                FlowJsxTag::Identifier {
                    name,
                    intrinsic: false,
                    ..
                }
                | FlowJsxTag::Member { object: name, .. } => Some(name.as_str()),
                _ => None,
            };
            let renders_content = matches!(tag, FlowJsxTag::Identifier { .. })
                && (values.contains_key("children")
                    || values.contains_key("render")
                    || values.contains_key("component"));
            if let Some(local) = local {
                if matches!(tag, FlowJsxTag::Identifier { .. })
                    && values
                        .values()
                        .any(|value| !capability_ids(value).is_empty())
                {
                    self.request_import_for_local(file_id, local);
                } else if renders_content || self.current_reachability != Reachability::Unknown {
                    self.request_component_import(file_id, local, renders_content);
                }
            }
        }
        TrackedValue::plain(AbstractValue::element(ElementValue {
            component: Box::new(component),
            props: values,
            span: expression.span.clone(),
            trace: self.trace.clone(),
        }))
    }

    fn render(&mut self, value: TrackedValue) {
        if self.unreached_render_budget_active {
            self.unreached_render_evaluations += 1;
            if self.unreached_render_evaluations > MAX_UNREACHED_RENDER_EVALUATIONS {
                if !self.unreached_render_budget_reported {
                    let span = self
                        .unreached_render_seed_span
                        .as_ref()
                        .expect("active unreached render has a seed span")
                        .clone();
                    self.mark_value_unresolved(
                        &value,
                        "unreached component render budget exhausted",
                        span.clone(),
                    );
                    self.record_coverage_gap("unreached component render budget exhausted", &span);
                    self.unreached_render_budget_reported = true;
                }
                return;
            }
        }
        let element = match value.value {
            AbstractValue::Element(element) => Rc::unwrap_or_clone(element),
            AbstractValue::Array(values) | AbstractValue::Union(values) => {
                for value in Rc::unwrap_or_clone(values) {
                    self.render(value);
                }
                return;
            }
            _ => return,
        };
        if self.current_reachability == Reachability::Possible {
            if self.assumed_budget_exhausted() {
                return;
            }
            // Under an assumption, explore only what can lead toward a factory host: corridor
            // components, or components handed JSX or callbacks from the path above.
            if let AbstractValue::Function(key) = &element.component.value
                && !self.corridor_files.is_empty()
                && !self.corridor_files.contains(&key.file_id)
                && !element.props.values().any(|value| {
                    self.carries_render_content(value, 0) || !capability_ids(value).is_empty()
                })
            {
                return;
            }
        }
        let previous_trace = std::mem::take(&mut self.trace);
        self.trace = merge_trace(&previous_trace, &element.trace);
        self.trace.push(TraceStep {
            kind: match &element.component.value {
                AbstractValue::ConfiguredComponent(_) => QueryCallPathKind::ModeledRender,
                AbstractValue::Unknown(_) | AbstractValue::AssumedWrapper { .. }
                    if self.current_reachability != Reachability::Unknown =>
                {
                    QueryCallPathKind::AssumedRender
                }
                _ => QueryCallPathKind::Render,
            },
            span: element.span.clone(),
        });
        let identity = match &element.component.value {
            AbstractValue::Function(key) => format!("function:{}:{}", key.file_id.0, key.name),
            AbstractValue::ConfiguredComponent(model) => {
                let rendered = model
                    .render_props
                    .iter()
                    .chain(&model.component_props)
                    .filter_map(|prop| element.props.get(prop))
                    .map(|value| match &value.value {
                        AbstractValue::Function(key) => format!("{}:{}", key.file_id.0, key.name),
                        AbstractValue::Closure(_) => "closure".to_owned(),
                        _ => "other".to_owned(),
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                format!("configured:{}#{}:{rendered}", model.module, model.export)
            }
            AbstractValue::Closure(_) => "closure".to_owned(),
            AbstractValue::Intrinsic(name) => format!("intrinsic:{name}"),
            _ => "other".to_owned(),
        };
        // Assumed renders have their own budget so they cannot crowd out exact paths.
        let key = (
            self.current_choice.clone(),
            self.current_reachability == Reachability::Possible,
            element.span.file_id,
            element.span.start,
            identity,
        );
        let visits = self.render_visits.entry(key).or_default();
        *visits += 1;
        if *visits > MAX_RENDER_VISITS_PER_SITE {
            self.record_coverage_gap("render visit budget exhausted", &element.span);
            self.mark_values_unresolved(
                element.props.values(),
                "render visit budget exhausted",
                element.span,
            );
            self.trace = previous_trace;
            return;
        }
        match &element.component.value {
            AbstractValue::Function(key) => {
                let props = element.props.clone();
                let argument = TrackedValue {
                    value: AbstractValue::record(element.props),
                    evidence: element.component.evidence,
                    choice: element.component.choice.clone(),
                    heap_id: None,
                };
                let returned = self.call_function(key, vec![argument]);
                self.render(returned);
                if self.model.scan_callback_bodies {
                    for prop in props.values() {
                        self.scan_callback_bodies(prop, &element.span, 0);
                    }
                }
            }
            AbstractValue::Closure(closure) => {
                let props = element.props.clone();
                let argument = TrackedValue::plain(AbstractValue::record(element.props));
                let returned = self.invoke_value(
                    TrackedValue::plain(AbstractValue::Closure(closure.clone())),
                    vec![argument],
                    element.span.clone(),
                );
                self.render(returned);
                if self.model.scan_callback_bodies {
                    for prop in props.values() {
                        self.scan_callback_bodies(prop, &element.span, 0);
                    }
                }
            }
            AbstractValue::Intrinsic(name) => {
                for (prop_name, handler) in element.props {
                    if prop_name == "onClick" {
                        self.register_and_explore_handler(name, &handler, &element.span);
                    } else if prop_name == "children" {
                        self.render(handler);
                    }
                }
            }
            AbstractValue::ConfiguredComponent(model) => {
                for prop in &model.render_props {
                    if let Some(callback) = element.props.get(prop) {
                        if !model.render_callback_names.is_empty()
                            && !matches!(&callback.value, AbstractValue::Function(key)
                                if model.render_callback_names.contains(&key.name))
                        {
                            self.record_coverage_gap(
                                "configured render callback filter skipped a callback",
                                &element.span,
                            );
                            continue;
                        }
                        let returned = self.invoke_value(
                            callback.clone(),
                            vec![TrackedValue::unknown("component_render_props")],
                            element.span.clone(),
                        );
                        self.render(returned);
                    }
                }
                for prop in &model.component_props {
                    if let Some(component) = element.props.get(prop) {
                        self.render(TrackedValue::plain(AbstractValue::element(ElementValue {
                            component: Box::new(component.clone()),
                            props: BTreeMap::new(),
                            span: element.span.clone(),
                            trace: self.trace.clone(),
                        })));
                    }
                }
                if model.forward_children {
                    if let Some(children) = element.props.get("children") {
                        self.render(children.clone());
                    }
                }
                if model.invoke_children {
                    if let Some(children) = element.props.get("children") {
                        self.render_child_callback(children.clone(), &element.span);
                    }
                }
            }
            AbstractValue::Unknown(reason) => {
                self.mark_values_unresolved(
                    element.props.values(),
                    &format!("element has unknown component target: {reason}"),
                    element.span.clone(),
                );
                if self.model.scan_callback_bodies {
                    for prop in element.props.values() {
                        self.scan_callback_bodies(prop, &element.span, 0);
                    }
                }
                if self.current_reachability != Reachability::Unknown {
                    self.render_through_unmodeled_component(&element, &[]);
                }
            }
            AbstractValue::AssumedWrapper { reason, wrapped } => {
                self.mark_values_unresolved(
                    element.props.values(),
                    &format!("element has unknown component target: {reason}"),
                    element.span.clone(),
                );
                if self.model.scan_callback_bodies {
                    for prop in element.props.values() {
                        self.scan_callback_bodies(prop, &element.span, 0);
                    }
                }
                if self.current_reachability != Reachability::Unknown {
                    self.render_through_unmodeled_component(&element, wrapped);
                }
            }
            AbstractValue::Union(components) => {
                for component in components.iter().cloned() {
                    self.render(TrackedValue::plain(AbstractValue::element(ElementValue {
                        component: Box::new(component),
                        props: element.props.clone(),
                        span: element.span.clone(),
                        trace: self.trace.clone(),
                    })));
                }
            }
            _ => {
                self.mark_values_unresolved(
                    element.props.values(),
                    "element target is not a component",
                    element.span.clone(),
                );
                if self.model.scan_callback_bodies {
                    for prop in element.props.values() {
                        self.scan_callback_bodies(prop, &element.span, 0);
                    }
                }
            }
        }
        self.trace = previous_trace;
    }

    /// Explores what an unmodeled component may render on a path from a configured root.
    ///
    /// Children, JSX-valued props, component props, and render functions that return JSX are
    /// treated as rendered, as are components wrapped by an unmodeled higher-order component,
    /// which receive the element's props. Anything reached this way is only possibly reachable,
    /// and creations record each assumed component so the assumption can be resolved later.
    fn render_through_unmodeled_component(
        &mut self,
        element: &ElementValue,
        wrapped: &[TrackedValue],
    ) {
        let previous = self.current_reachability;
        self.current_reachability = Reachability::Possible;
        self.assumed_renders.push(element.span.clone());
        for component in wrapped {
            self.render(TrackedValue::plain(AbstractValue::element(ElementValue {
                component: Box::new(component.clone()),
                props: element.props.clone(),
                span: element.span.clone(),
                trace: self.trace.clone(),
            })));
        }
        for value in element.props.values() {
            self.render_assumed_prop(value, &element.span);
        }
        self.assumed_renders.pop();
        self.current_reachability = previous;
    }

    /// Renders a prop handed to an unmodeled component, including function children.
    fn render_assumed_prop(&mut self, value: &TrackedValue, span: &SourceSpan) {
        match &value.value {
            AbstractValue::Array(values) | AbstractValue::Union(values) => {
                for value in values.iter() {
                    self.render_assumed_prop(value, span);
                }
            }
            AbstractValue::Function(key) => {
                if self
                    .functions
                    .get(key)
                    .is_some_and(|function| statements_render_jsx(&function.body))
                {
                    self.render(TrackedValue::plain(AbstractValue::element(ElementValue {
                        component: Box::new(value.clone()),
                        props: BTreeMap::new(),
                        span: span.clone(),
                        trace: self.trace.clone(),
                    })));
                }
            }
            AbstractValue::Closure(closure) => {
                if arrow_body_renders_jsx(&closure.body) {
                    let returned = self.invoke_value(
                        value.clone(),
                        vec![TrackedValue::unknown("unmodeled_component_argument")],
                        span.clone(),
                    );
                    self.render(returned);
                }
            }
            _ => self.render(value.clone()),
        }
    }

    /// Counts one assumed-render step and reports when the per-root budget is exhausted.
    fn assumed_budget_exhausted(&mut self) -> bool {
        self.assumed_evaluations += 1;
        if self.assumed_evaluations <= MAX_ASSUMED_RENDER_EVALUATIONS {
            return false;
        }
        if !self.assumed_budget_reported {
            self.assumed_budget_reported = true;
            if let Some(span) = self.assumed_renders.first().cloned() {
                self.record_coverage_gap("assumed render budget exhausted", &span);
            }
        }
        true
    }

    /// Returns whether a prop value can carry something renderable: JSX, a component, or a
    /// render function.
    fn carries_render_content(&self, value: &TrackedValue, depth: usize) -> bool {
        match &value.value {
            AbstractValue::Element(_) => true,
            AbstractValue::Array(values) | AbstractValue::Union(values) => values
                .iter()
                .any(|value| self.carries_render_content(value, depth)),
            AbstractValue::Record(fields) => {
                depth < 4
                    && fields
                        .values()
                        .any(|value| self.carries_render_content(value, depth + 1))
            }
            _ => self.is_component_like(value),
        }
    }

    /// Returns whether a value can be rendered as a component: a function or closure that returns
    /// JSX, a configured component, or another assumed wrapper.
    fn is_component_like(&self, value: &TrackedValue) -> bool {
        match &value.value {
            AbstractValue::Function(key) => self
                .functions
                .get(key)
                .is_some_and(|function| statements_render_jsx(&function.body)),
            AbstractValue::Closure(closure) => arrow_body_renders_jsx(&closure.body),
            AbstractValue::ConfiguredComponent(_) | AbstractValue::AssumedWrapper { .. } => true,
            _ => false,
        }
    }

    fn render_child_callback(&mut self, child: TrackedValue, span: &SourceSpan) {
        match child.value {
            AbstractValue::Array(children) | AbstractValue::Union(children) => {
                for child in Rc::unwrap_or_clone(children) {
                    self.render_child_callback(child, span);
                }
            }
            AbstractValue::Element(_) => self.render(child),
            _ => {
                let returned = self.invoke_value(
                    child,
                    vec![TrackedValue::unknown("render_child_props")],
                    span.clone(),
                );
                self.render(returned);
            }
        }
    }

    fn register_and_explore_handler(
        &mut self,
        intrinsic: &str,
        handler: &TrackedValue,
        element_span: &SourceSpan,
    ) {
        let capability_ids = self.value_capability_ids(handler);
        for capability in capability_ids {
            let evidence = self.push_evidence(
                RelationKind::EventRegistration,
                "react_intrinsic_event_sink",
                element_span.clone(),
                handler.evidence.into_iter().collect(),
                None,
                &format!("wrapper registered as {intrinsic}.onClick handler"),
            );
            if let Some(state) = self.capabilities.get_mut(capability) {
                state.registrations.push(evidence);
            }
        }
        match &handler.value {
            AbstractValue::Closure(closure) => {
                // Explore a possible later event call. React ignores its return value.
                self.call_closure(closure, Vec::new());
            }
            AbstractValue::Capability(capability) => {
                let call_path = self.trace_at(QueryCallPathKind::Invocation, element_span);
                let evidence = self.push_evidence(
                    RelationKind::Invocation,
                    "react_event_dispatch",
                    element_span.clone(),
                    handler.evidence.into_iter().collect(),
                    Some(self.model.id.clone()),
                    "registered callback may be invoked by a later click",
                );
                if let Some(state) = self.capabilities.get_mut(*capability) {
                    state.invocations.push(InvocationState {
                        evidence,
                        arguments: Vec::new(),
                        call_path,
                    });
                }
                self.emit_modeled_effects(*capability, evidence, element_span.clone());
            }
            _ => self.mark_value_unresolved(
                handler,
                "event handler is not a known callback",
                element_span.clone(),
            ),
        }
    }

    fn call_closure(
        &mut self,
        closure: &ClosureValue,
        arguments: Vec<TrackedValue>,
    ) -> TrackedValue {
        let trace_len = self.trace.len();
        if self
            .trace
            .last()
            .is_none_or(|step| step.span != closure.span)
        {
            self.trace.push(TraceStep {
                kind: QueryCallPathKind::Call,
                span: closure.span.clone(),
            });
        }
        let mut environment = closure.environment.clone();
        let mut references = ClosureReferences::default();
        collect_body_references(&closure.body, &mut references);
        let mut captures = references.names;
        captures.retain(|name| closure.environment.contains_key(name));
        for (index, pattern) in closure.params.iter().enumerate() {
            for name in pattern_names(pattern) {
                captures.remove(name);
            }
            let argument = arguments
                .get(index)
                .cloned()
                .unwrap_or_else(|| TrackedValue::plain(AbstractValue::Undefined));
            self.bind_pattern(
                pattern,
                argument,
                &mut environment,
                RelationKind::ValueTransfer,
            );
        }
        self.active_captures.push(captures);
        let returned = match &closure.body {
            FlowArrowBody::Expression { expression } => {
                self.eval(expression, &environment, closure.file_id)
            }
            FlowArrowBody::Statements { statements } => self
                .execute_statements(statements, &mut environment, closure.file_id)
                .unwrap_or_else(|| TrackedValue::plain(AbstractValue::Undefined)),
        };
        self.active_captures.pop();
        self.trace.truncate(trace_len);
        returned
    }

    fn call_model(&mut self, arguments: &[TrackedValue], span: SourceSpan) -> TrackedValue {
        let arguments = arguments
            .iter()
            .map(|value| self.materialize(value))
            .collect::<Vec<_>>();
        let mut captures = BTreeMap::new();
        let mut missing_capture = false;
        for (name, source) in &self.model.captures {
            let CaptureSource::Argument { index } = source;
            missing_capture |= arguments.get(*index).is_none();
            captures.insert(
                name.clone(),
                arguments
                    .get(*index)
                    .cloned()
                    .unwrap_or_else(|| TrackedValue::unknown("missing_model_argument")),
            );
        }
        let key_value = captures
            .get("notice_key")
            .cloned()
            .or_else(|| captures.values().next().cloned())
            .unwrap_or_else(|| TrackedValue::unknown("missing_key_capture"));
        let key = render_compact_value(&key_value);
        let choice = key_value
            .choice
            .clone()
            .or_else(|| self.current_choice.clone())
            .unwrap_or_else(|| "<uncorrelated>".to_owned());
        let origin = self.push_evidence(
            RelationKind::Capture,
            "callback_factory_model",
            span.clone(),
            key_value.evidence.into_iter().collect(),
            Some(self.model.id.clone()),
            &format!("{} creates callback capturing key {key}", self.model.id),
        );
        let origin_trace = self.trace_at(QueryCallPathKind::Factory, &span);
        let capability_id = self.capabilities.len();
        let mut assumptions = vec![
            self.model.evidence.reason.clone(),
            format!("input domain constrains current registry selector to {choice}"),
            "single-render React abstraction; registered handlers are explored as possible later events"
                .to_owned(),
        ];
        let mut unresolved = Vec::new();
        let mut invocations = Vec::new();
        if missing_capture {
            let evidence = self.push_evidence(
                RelationKind::UnresolvedEscape,
                "incomplete_callback_factory_capture",
                span.clone(),
                vec![origin],
                Some(self.model.id.clone()),
                "callback factory capture could not be resolved from call arguments",
            );
            unresolved.push(evidence);
        }
        if captures.values().any(value_is_uncertain) {
            unresolved.push(self.push_evidence(
                RelationKind::UnresolvedEscape,
                "uncertain_callback_factory_capture",
                span.clone(),
                vec![origin],
                Some(self.model.id.clone()),
                "projected factory argument contains an unknown or joined value",
            ));
            self.record_coverage_gap("uncertain callback factory capture", &span);
        }
        if self.current_reachability == Reachability::Possible {
            let mut assumed = Vec::<SourceSpan>::new();
            for span in &self.assumed_renders {
                if !assumed.contains(span) {
                    assumed.push(span.clone());
                }
            }
            for span in assumed {
                unresolved.push(self.push_evidence(
                    RelationKind::UnresolvedEscape,
                    "factory_callsite_reached_through_unmodeled_component",
                    span.clone(),
                    vec![origin],
                    Some(self.model.id.clone()),
                    "factory callsite is reached only if an unmodeled component renders it",
                ));
                self.record_coverage_gap("render assumed through unmodeled component", &span);
            }
        }
        if self.current_reachability == Reachability::Unknown {
            unresolved.push(self.push_evidence(
                RelationKind::UnresolvedEscape,
                "factory_callsite_not_reached",
                span.clone(),
                vec![origin],
                Some(self.model.id.clone()),
                "factory callsite was not reached from configured roots; local context is unknown",
            ));
        }
        match self.model.retains_returned_callback {
            Some(false) => {
                assumptions.push("model does not retain the returned callback".to_owned());
            }
            Some(true) => unresolved.push(self.push_evidence(
                RelationKind::UnresolvedEscape,
                "callback_factory_retains_callback",
                span.clone(),
                vec![origin],
                Some(self.model.id.clone()),
                "factory may retain the returned callback beyond modeled scope",
            )),
            None => unresolved.push(self.push_evidence(
                RelationKind::UnresolvedEscape,
                "unknown_callback_factory_retention",
                span.clone(),
                vec![origin],
                Some(self.model.id.clone()),
                "model does not specify whether the factory retains the returned callback",
            )),
        }
        match self.model.invokes_returned_callback_during_call {
            Some(false) => {
                assumptions.push("model does not invoke the capability during creation".to_owned());
            }
            Some(true) => invocations.push(InvocationState {
                evidence: self.push_evidence(
                    RelationKind::Invocation,
                    "callback_factory_invokes_during_call",
                    span.clone(),
                    vec![origin],
                    Some(self.model.id.clone()),
                    "factory model invokes the returned capability during creation",
                ),
                arguments: Vec::new(),
                call_path: self.trace_at(QueryCallPathKind::Invocation, &span),
            }),
            None => unresolved.push(self.push_evidence(
                RelationKind::UnresolvedEscape,
                "unknown_callback_factory_creation_invocation",
                span.clone(),
                vec![origin],
                Some(self.model.id.clone()),
                "model does not specify whether the factory invokes the capability during creation",
            )),
        }
        self.capabilities.push(CapabilityState {
            key,
            choice: choice.clone(),
            callsite: span.clone(),
            origin,
            origin_trace,
            factory_arguments: arguments.to_vec(),
            reachability: self.current_reachability,
            reverse_importer: self.current_reverse_importer.clone(),
            registrations: Vec::new(),
            invocations,
            unresolved,
            assumptions,
        });
        let creation_invocations = self.capabilities[capability_id]
            .invocations
            .iter()
            .map(|invocation| invocation.evidence)
            .collect::<Vec<_>>();
        for invocation in creation_invocations {
            self.emit_modeled_effects(capability_id, invocation, span.clone());
        }
        let capability = TrackedValue {
            value: AbstractValue::Capability(capability_id),
            evidence: Some(origin),
            choice: Some(choice),
            heap_id: None,
        };
        if let Some(index) = self.model.returned_index {
            let mut elements = vec![TrackedValue::unknown("unselected_return_element"); index + 1];
            elements[index] = capability;
            TrackedValue {
                value: AbstractValue::array(elements),
                evidence: Some(origin),
                choice: self.current_choice.clone(),
                heap_id: None,
            }
        } else {
            let mut returned = capability;
            for property in self.model.returned_property.iter().rev() {
                returned = TrackedValue {
                    value: AbstractValue::record(BTreeMap::from([(property.clone(), returned)])),
                    evidence: Some(origin),
                    choice: self.current_choice.clone(),
                    heap_id: None,
                };
            }
            returned
        }
    }

    fn emit_modeled_effects(
        &mut self,
        capability: usize,
        invocation: EvidenceId,
        span: SourceSpan,
    ) {
        let key = self
            .capabilities
            .get(capability)
            .map_or_else(|| "<unknown>".to_owned(), |state| state.key.clone());
        for operation in self.model.on_invoke.clone() {
            let ModeledOperation::Effect { name, arguments } = operation;
            let rendered_arguments = arguments
                .into_iter()
                .map(|argument| match argument {
                    ModelValue::Capture { name } if name == "notice_key" => key.clone(),
                    ModelValue::Capture { name } => format!("<{name}>"),
                })
                .collect::<Vec<_>>()
                .join(", ");
            self.push_evidence(
                RelationKind::Effect,
                "callback_factory_on_invoke",
                span.clone(),
                vec![invocation],
                Some(self.model.id.clone()),
                &format!("modeled effect {name}({rendered_arguments})"),
            );
        }
    }

    fn read_property(
        &mut self,
        object: TrackedValue,
        property: &str,
        span: SourceSpan,
        relation: RelationKind,
    ) -> TrackedValue {
        let object = self.materialize(&object);
        if let AbstractValue::Namespace(file_id) = object.value {
            let resolution = self.symbol_linker.resolve_exported_value(file_id, property);
            if resolution == ValueResolution::Missing {
                self.record_coverage_gap("missing namespace export", &span);
            }
            let mut value = self.linked_value(resolution, &span);
            let parents = object.evidence.into_iter().chain(value.evidence).collect();
            let evidence = self.push_evidence(
                relation,
                "read_namespace_export",
                span,
                parents,
                None,
                &format!("read namespace export {property}"),
            );
            value.evidence = Some(evidence);
            value.choice = object.choice.or(value.choice);
            return value;
        }
        if let AbstractValue::Union(values) = object.value.clone() {
            return TrackedValue::plain(AbstractValue::union(
                values
                    .iter()
                    .cloned()
                    .map(|value| self.read_property(value, property, span.clone(), relation))
                    .collect(),
            ));
        }
        if let AbstractValue::Array(elements) = &object.value {
            if let Ok(index) = property.parse::<usize>() {
                let value = elements
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| TrackedValue::plain(AbstractValue::Undefined));
                let evidence = self.push_evidence(
                    relation,
                    "read_array_element",
                    span,
                    object.evidence.into_iter().chain(value.evidence).collect(),
                    None,
                    &format!("read array index {index}"),
                );
                return TrackedValue {
                    evidence: Some(evidence),
                    choice: object.choice.or(value.choice.clone()),
                    ..value
                };
            }
        }
        let AbstractValue::Record(fields) = &object.value else {
            self.mark_value_unresolved(
                &object,
                &format!("property read {property} from non-record value"),
                span,
            );
            return TrackedValue::unknown("property_read_from_non_record");
        };
        let Some(value) = fields.get(property).cloned() else {
            self.mark_value_unresolved(&object, &format!("unknown property {property}"), span);
            return TrackedValue::unknown(format!("unknown_property:{property}"));
        };
        let choice = object.choice.clone().or(value.choice.clone()).or_else(|| {
            (relation == RelationKind::KeySelection)
                .then(|| self.current_choice.clone())
                .flatten()
        });
        let mut parents = object
            .evidence
            .into_iter()
            .chain(value.evidence)
            .collect::<Vec<_>>();
        parents.sort();
        parents.dedup();
        let evidence = self.push_evidence(
            relation,
            if relation == RelationKind::KeySelection {
                "finite_record_selection"
            } else {
                "read_known_property"
            },
            span,
            parents,
            None,
            &format!("read property {property}"),
        );
        TrackedValue {
            evidence: Some(evidence),
            choice,
            ..value
        }
    }

    fn linked_expression(&self, file_id: FileId, expression: &FlowExpression) -> ValueResolution {
        match &expression.kind {
            FlowExpressionKind::Identifier {
                name,
                module_binding: true,
            } => self.symbol_linker.resolve_binding(file_id, name),
            FlowExpressionKind::StaticMember { object, property } => {
                match self.linked_expression(file_id, object) {
                    ValueResolution::Resolved(LinkedValue::Namespace(module)) => {
                        self.symbol_linker.resolve_exported_value(module, property)
                    }
                    _ => ValueResolution::Missing,
                }
            }
            FlowExpressionKind::ComputedMember { object, property } => {
                if let FlowExpressionKind::String { value } = &property.kind {
                    match self.linked_expression(file_id, object) {
                        ValueResolution::Resolved(LinkedValue::Namespace(module)) => {
                            self.symbol_linker.resolve_exported_value(module, value)
                        }
                        _ => ValueResolution::Missing,
                    }
                } else {
                    ValueResolution::Missing
                }
            }
            _ => ValueResolution::Missing,
        }
    }

    fn expression_matches_model(&self, file_id: FileId, expression: &FlowExpression) -> bool {
        self.linked_expression(file_id, expression)
            == ValueResolution::Resolved(LinkedValue::Declaration(self.model_symbol.clone()))
    }

    fn factory_candidates(&self) -> Vec<FactoryCallCandidate> {
        let mut candidates = Vec::new();
        for file in &self.snapshot.files {
            for binding in &file.flow.globals {
                collect_factory_calls(&binding.value, file.file_id, None, &mut candidates);
            }
            for function in &file.flow.functions {
                let key = FunctionKey {
                    file_id: file.file_id,
                    name: function.name.clone(),
                };
                for statement in &function.body {
                    collect_factory_calls_statement(
                        statement,
                        file.file_id,
                        Some(&key),
                        &mut candidates,
                    );
                }
            }
        }
        candidates.sort_by_key(|candidate| {
            (
                candidate.span.file_id,
                candidate.span.start,
                candidate.span.end,
            )
        });
        candidates.dedup_by(|left, right| left.span == right.span);
        candidates
    }

    fn seed_unreached_creations(&mut self) {
        for candidate in self.factory_candidates() {
            // Each unreached callsite is a separate hypothetical execution.
            self.heap.clear();
            self.heap_versions.clear();
            if !self.expression_matches_model(candidate.file_id, &candidate.callee) {
                if self.possible_unresolved_factory_call(&candidate) {
                    self.record_coverage_gap(
                        "possible factory call has unresolved import",
                        &candidate.span,
                    );
                }
                continue;
            }
            if self
                .capabilities
                .iter()
                .any(|capability| capability.callsite == candidate.span)
            {
                if self.capabilities.iter().any(|capability| {
                    capability.callsite == candidate.span
                        && capability.reachability == Reachability::Unknown
                }) {
                    self.record_unreached_gap(&candidate.span);
                }
                continue;
            }
            self.current_choice = Some("<unreached>".to_owned());
            self.current_reachability = Reachability::Unknown;
            if let Some(key) = &candidate.enclosing_function {
                let parameter_count = self
                    .functions
                    .get(key)
                    .map_or(0, |function| function.params.len());
                let arguments = (0..parameter_count)
                    .map(|_| TrackedValue::unknown("unreached_function_parameter"))
                    .collect();
                let returned = self.call_function(key, arguments);
                self.render_unreached(returned, &candidate.span);
            } else if let Some(binding) = self
                .symbol_linker
                .file(candidate.file_id)
                .and_then(|file| {
                    file.flow.globals.iter().find(|binding| {
                        binding.value.span.start <= candidate.span.start
                            && candidate.span.end <= binding.value.span.end
                    })
                })
                .cloned()
            {
                let environment = self.module_environment(candidate.file_id);
                let callable = self.eval(&binding.value, &environment, candidate.file_id);
                if let AbstractValue::Closure(closure) = &callable.value {
                    let exported = matches!(
                        &binding.pattern.kind,
                        FlowPatternKind::Identifier { name }
                            if self.symbol_linker.file(candidate.file_id).is_some_and(|file| {
                                file.flow.exports.iter().any(|export| {
                                        matches!(export, crate::ir::FlowExport::Local { local, type_only: false, .. } if local == name)
                                    })
                            })
                    );
                    let arguments = (0..closure.params.len())
                        .map(|_| TrackedValue::unknown("unreached_function_parameter"))
                        .collect();
                    let returned = self.invoke_value(callable, arguments, candidate.span.clone());
                    if exported && returns_capability_data(&returned) {
                        self.capability_producer_files.insert(candidate.file_id);
                    }
                    self.render_unreached(returned, &candidate.span);
                }
            }
            if !self
                .capabilities
                .iter()
                .any(|capability| capability.callsite == candidate.span)
            {
                let environment = self.module_environment(candidate.file_id);
                let arguments = candidate
                    .arguments
                    .iter()
                    .map(|argument| self.eval(argument, &environment, candidate.file_id))
                    .collect::<Vec<_>>();
                self.call_model(&arguments, candidate.span.clone());
            }
            if self.capabilities.iter().any(|capability| {
                capability.callsite == candidate.span
                    && capability.factory_arguments.iter().any(value_is_uncertain)
            }) {
                self.request_factory_argument_imports(&candidate);
                let exported_host =
                    self.symbol_linker
                        .file(candidate.file_id)
                        .is_some_and(|file| {
                            file.flow.exports.iter().any(|export| {
                                let crate::ir::FlowExport::Local {
                                    local,
                                    type_only: false,
                                    ..
                                } = export
                                else {
                                    return false;
                                };
                                if candidate
                                    .enclosing_function
                                    .as_ref()
                                    .is_some_and(|key| key.name == *local)
                                {
                                    return true;
                                }
                                file.flow.globals.iter().any(|binding| {
                                    pattern_names(&binding.pattern).contains(&local.as_str())
                                        && binding.value.span.start <= candidate.span.start
                                        && candidate.span.end <= binding.value.span.end
                                })
                            })
                        });
                if exported_host {
                    self.caller_producer_files.insert(candidate.file_id);
                }
            }
            self.record_unreached_gap(&candidate.span);
        }
        self.current_choice = None;
        self.current_reachability = Reachability::Reachable;
    }

    fn render_unreached(&mut self, value: TrackedValue, span: &SourceSpan) {
        self.unreached_render_evaluations = 0;
        self.unreached_render_budget_reported = false;
        self.unreached_render_seed_span = Some(span.clone());
        self.unreached_render_budget_active = true;
        self.render(value);
        self.unreached_render_budget_active = false;
        self.unreached_render_seed_span = None;
    }

    fn request_factory_argument_imports(&mut self, candidate: &FactoryCallCandidate) {
        let mut names = BTreeSet::new();
        for argument in &candidate.arguments {
            let mut references = ClosureReferences::default();
            collect_expression_references(argument, &mut references);
            names.extend(references.names);
        }
        let statements = candidate
            .enclosing_function
            .as_ref()
            .and_then(|key| self.functions.get(key).copied())
            .map(|function| function.body.as_slice())
            .unwrap_or_default();
        let globals = self
            .symbol_linker
            .file(candidate.file_id)
            .map(|file| file.flow.globals.as_slice())
            .unwrap_or_default();
        for _ in 0..8 {
            let before = names.len();
            extend_binding_dependencies(statements, &mut names);
            for binding in globals {
                if pattern_names(&binding.pattern)
                    .iter()
                    .any(|name| names.contains(*name))
                {
                    let mut references = ClosureReferences::default();
                    collect_expression_references(&binding.value, &mut references);
                    names.extend(references.names);
                }
            }
            if names.len() == before {
                break;
            }
        }
        let mut pending = names
            .into_iter()
            .map(|name| (candidate.file_id, name))
            .collect::<VecDeque<_>>();
        let mut visited = BTreeSet::new();
        while let Some((file_id, name)) = pending.pop_front() {
            if !visited.insert((file_id, name.clone())) {
                continue;
            }
            if visited.len() > 64 {
                self.record_coverage_gap(
                    "factory argument import dependency budget exhausted",
                    &candidate.span,
                );
                break;
            }
            self.request_import_for_local(file_id, &name);
            let target = self
                .symbol_linker
                .file(file_id)
                .and_then(|file| {
                    file.flow
                        .imports
                        .iter()
                        .find(|import| import.local == name)
                        .map(|import| (file, import))
                })
                .and_then(|(file, import)| {
                    self.symbol_linker
                        .import_resolutions(&file.path)
                        .find(|resolution| resolution.specifier == import.module)
                        .and_then(|resolution| resolution.resolved_path.as_ref())
                        .map(|path| (path, import.imported.as_str()))
                })
                .and_then(|(path, imported)| {
                    self.symbol_linker.file_at(path).map(|file| {
                        (
                            file.file_id,
                            file.flow.globals.as_slice(),
                            imported.to_owned(),
                        )
                    })
                });
            if let Some((target_id, globals, imported)) = target {
                for binding in globals {
                    if pattern_names(&binding.pattern).contains(&imported.as_str()) {
                        let mut references = ClosureReferences::default();
                        collect_expression_references(&binding.value, &mut references);
                        pending.extend(references.names.into_iter().map(|name| (target_id, name)));
                    }
                }
            }
        }
    }

    fn seed_reverse_importers(
        &mut self,
        seed_paths: &BTreeSet<std::path::PathBuf>,
        producer_paths: &BTreeSet<std::path::PathBuf>,
    ) {
        if seed_paths.is_empty() || producer_paths.is_empty() {
            return;
        }
        self.current_choice = Some("<reverse-importer>".to_owned());
        self.current_reachability = Reachability::Unknown;
        let mut seeded = 0_usize;
        for path in seed_paths {
            let Some(file) = self.symbol_linker.file_at(path) else {
                continue;
            };
            let file_id = file.file_id;
            let file_len = file.source_len;
            let imported_names = file
                .flow
                .imports
                .iter()
                .filter(|import| !import.type_only)
                .filter(|import| {
                    self.symbol_linker
                        .import_resolutions(&file.path)
                        .any(|resolution| {
                            resolution.specifier == import.module
                                && resolution
                                    .resolved_path
                                    .as_ref()
                                    .is_some_and(|path| producer_paths.contains(path))
                        })
                })
                .map(|import| import.local.clone())
                .collect::<BTreeSet<_>>();
            if imported_names.is_empty() {
                continue;
            }
            for function in &file.flow.functions {
                self.heap.clear();
                self.heap_versions.clear();
                let mut references = ClosureReferences::default();
                for statement in &function.body {
                    collect_statement_references(statement, &mut references);
                }
                if references.names.is_disjoint(&imported_names) {
                    continue;
                }
                if file_len > MAX_REVERSE_IMPORTER_SOURCE_BYTES {
                    self.seed_direct_import_uses(
                        file_id,
                        &function.name,
                        &function.body,
                        &function.params,
                        &imported_names,
                    );
                    continue;
                }
                let mut direct_uses = Vec::new();
                for statement in &function.body {
                    collect_import_uses_statement(statement, &imported_names, &mut direct_uses);
                }
                for expression in direct_uses {
                    let mut argument_references = ClosureReferences::default();
                    collect_expression_references(&expression, &mut argument_references);
                    for name in argument_references.modules {
                        self.request_import_for_local(file_id, &name);
                    }
                }
                let key = FunctionKey {
                    file_id,
                    name: function.name.clone(),
                };
                let arguments = function
                    .params
                    .iter()
                    .map(|_| TrackedValue::unknown("reverse_importer_parameter"))
                    .collect();
                seeded += 1;
                if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
                    eprintln!(
                        "query seeding importer function {seeded} in file {} (file_bytes={file_len}, body_statements={})",
                        file_id.0,
                        function.body.len()
                    );
                }
                self.reverse_evaluations = 0;
                self.reverse_budget_reported = false;
                self.reverse_budget_active = true;
                self.current_reverse_importer = Some(ReverseImporterSeed {
                    symbol: function.name.clone(),
                    matched_imports: imported_names.iter().cloned().collect(),
                    evaluation: QueryReverseImporterEvaluation::Function,
                    span: function.span.clone(),
                });
                let returned = self.call_function(&key, arguments);
                self.render(returned);
                self.current_reverse_importer = None;
                self.reverse_budget_active = false;
            }
            for binding in &file.flow.globals {
                let mut references = ClosureReferences::default();
                collect_expression_references(&binding.value, &mut references);
                if references.names.is_disjoint(&imported_names) {
                    continue;
                }
                self.current_reverse_importer = Some(ReverseImporterSeed {
                    symbol: pattern_names(&binding.pattern).join(", "),
                    matched_imports: imported_names.iter().cloned().collect(),
                    evaluation: QueryReverseImporterEvaluation::ModuleBinding,
                    span: binding.span.clone(),
                });
                let environment = self.module_environment(file_id);
                let returned = self.eval(&binding.value, &environment, file_id);
                self.render(returned);
                self.current_reverse_importer = None;
            }
        }
        if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
            eprintln!("query reverse importer functions seeded: {seeded}");
        }
        self.current_choice = None;
        self.current_reachability = Reachability::Reachable;
        self.current_reverse_importer = None;
    }

    fn seed_direct_import_uses(
        &mut self,
        file_id: FileId,
        symbol: &str,
        statements: &[FlowStatement],
        params: &[FlowPattern],
        imported_names: &BTreeSet<String>,
    ) {
        let mut uses = Vec::new();
        for statement in statements {
            collect_import_uses_statement(statement, imported_names, &mut uses);
        }
        if uses.is_empty() {
            return;
        }
        if uses.len() > 64 {
            self.record_coverage_gap("imported callsite budget exhausted", &uses[64].span);
            uses.truncate(64);
        }
        for expression in uses {
            self.current_reverse_importer = Some(ReverseImporterSeed {
                symbol: symbol.to_owned(),
                matched_imports: imported_names.iter().cloned().collect(),
                evaluation: QueryReverseImporterEvaluation::DirectImportUse,
                span: expression.span.clone(),
            });
            self.heap.clear();
            self.heap_versions.clear();
            let mut references = ClosureReferences::default();
            collect_expression_references(&expression, &mut references);
            let mut needed = references.names;
            for _ in 0..8 {
                let previous = needed.len();
                extend_binding_dependencies(statements, &mut needed);
                if needed.len() == previous {
                    break;
                }
            }
            for name in &needed {
                self.request_import_for_local(file_id, &name);
            }
            let mut environment = self.module_environment(file_id);
            for pattern in params {
                self.bind_pattern(
                    pattern,
                    TrackedValue::unknown("unreached_caller_parameter"),
                    &mut environment,
                    RelationKind::RenderPropBinding,
                );
            }
            self.reverse_evaluations = 0;
            self.reverse_budget_reported = false;
            self.reverse_budget_active = true;
            for statement in statements {
                let (span, relevant) = match statement {
                    FlowStatement::Bind(binding) => (
                        &binding.span,
                        pattern_names(&binding.pattern)
                            .iter()
                            .any(|name| needed.contains(*name)),
                    ),
                    FlowStatement::Assign {
                        target: FlowAssignmentTarget::Identifier { name },
                        span,
                        ..
                    } => (span, needed.contains(name)),
                    FlowStatement::Assign {
                        target: FlowAssignmentTarget::StaticMember { object, .. },
                        span,
                        ..
                    } => {
                        let relevant = matches!(&object.kind, FlowExpressionKind::Identifier { name, .. } if needed.contains(name));
                        (span, relevant)
                    }
                    FlowStatement::Expression { value, span } => {
                        let relevant = matches!(&value.kind, FlowExpressionKind::Call { callee, .. } if matches!(&callee.kind, FlowExpressionKind::StaticMember { object, property } if property == "push" && matches!(&object.kind, FlowExpressionKind::Identifier { name, .. } if needed.contains(name))));
                        (span, relevant)
                    }
                    FlowStatement::If { span, .. } => {
                        let mut branch_references = ClosureReferences::default();
                        collect_statement_references(statement, &mut branch_references);
                        (span, !branch_references.names.is_disjoint(&needed))
                    }
                    FlowStatement::Return { span, .. }
                    | FlowStatement::Unsupported(crate::ir::UnsupportedIr { span, .. }) => {
                        (span, false)
                    }
                    FlowStatement::Assign {
                        target:
                            FlowAssignmentTarget::ComputedMember { .. }
                            | FlowAssignmentTarget::Unsupported { .. },
                        span,
                        ..
                    } => (span, false),
                };
                if span.end > expression.span.start || !relevant {
                    continue;
                }
                match statement {
                    FlowStatement::Bind(binding) => {
                        let value = self.eval(&binding.value, &environment, file_id);
                        self.bind_pattern(
                            &binding.pattern,
                            value,
                            &mut environment,
                            RelationKind::ValueTransfer,
                        );
                    }
                    FlowStatement::Assign {
                        target,
                        value,
                        span,
                    } => {
                        let value = self.eval(value, &environment, file_id);
                        self.assign_target(target, value, &mut environment, file_id, span.clone());
                    }
                    FlowStatement::Expression { value, .. } => {
                        self.eval_array_push(value, &mut environment, file_id);
                    }
                    FlowStatement::If { .. } => {
                        self.execute_statements(
                            std::slice::from_ref(statement),
                            &mut environment,
                            file_id,
                        );
                    }
                    _ => {}
                }
            }
            let value = self.eval(&expression, &environment, file_id);
            self.render(value);
            self.reverse_budget_active = false;
            self.current_reverse_importer = None;
        }
    }

    fn possible_unresolved_factory_call(&self, candidate: &FactoryCallCandidate) -> bool {
        let (local, imported) = match &candidate.callee.kind {
            FlowExpressionKind::Identifier { name, .. } => {
                (name.as_str(), self.model.r#match.export.as_str())
            }
            FlowExpressionKind::StaticMember { object, property }
                if property == &self.model.r#match.export =>
            {
                let FlowExpressionKind::Identifier { name, .. } = &object.kind else {
                    return false;
                };
                (name.as_str(), "*")
            }
            _ => return false,
        };
        let Some(file) = self.symbol_linker.file(candidate.file_id) else {
            return false;
        };
        file.flow.imports.iter().any(|import| {
            import.local == local
                && import.imported == imported
                && self.symbol_linker.resolve_binding(candidate.file_id, local)
                    == ValueResolution::Unresolved
        })
    }

    fn record_unreached_gap(&mut self, span: &SourceSpan) {
        let gap = format!(
            "factory call at file {} bytes {}..{} was not reached from configured roots",
            span.file_id.0, span.start, span.end
        );
        if self.coverage_gap_keys.insert(gap.clone()) {
            self.coverage_gaps.push(gap.clone());
        }
        if self
            .query_gap_keys
            .insert((gap.clone(), self.current_choice.clone()))
        {
            self.query_gaps.push((
                gap,
                "factory_call_not_reached".to_owned(),
                span.clone(),
                self.current_choice.clone(),
            ));
        }
    }

    fn mark_values_unresolved<'b>(
        &mut self,
        values: impl Iterator<Item = &'b TrackedValue>,
        reason: &str,
        span: SourceSpan,
    ) {
        let mut ids = values
            .flat_map(|value| self.value_capability_ids(value))
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        for capability in ids {
            let origin = self.capabilities[capability].origin;
            let evidence = self.push_evidence(
                RelationKind::UnresolvedEscape,
                "unsupported_reachable_operation",
                span.clone(),
                vec![origin],
                None,
                reason,
            );
            if let Some(state) = self.capabilities.get_mut(capability) {
                state.unresolved.push(evidence);
            }
            self.record_coverage_gap(reason, &span);
        }
    }

    fn mark_value_unresolved(&mut self, value: &TrackedValue, reason: &str, span: SourceSpan) {
        self.mark_values_unresolved(std::iter::once(value), reason, span);
    }

    fn value_capability_ids(&self, value: &TrackedValue) -> Vec<usize> {
        let mut ids = capability_ids(value);
        let mut namespaces = BTreeSet::new();
        collect_namespace_ids(value, &mut namespaces);
        let mut modules = BTreeSet::new();
        for namespace in namespaces {
            self.module_initialization_order(namespace, &mut modules, &mut Vec::new());
        }
        for (symbol, value) in &self.globals {
            if modules.contains(&symbol.file_id) {
                ids.extend(capability_ids(value));
            }
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    fn push_evidence(
        &mut self,
        relation: RelationKind,
        rule: &str,
        span: SourceSpan,
        parents: Vec<EvidenceId>,
        model_id: Option<String>,
        summary: &str,
    ) -> EvidenceId {
        let id = EvidenceId(u32::try_from(self.evidence.len()).unwrap_or(u32::MAX));
        self.evidence.push(Evidence {
            id,
            relation,
            rule: rule.to_owned(),
            span,
            parents,
            choice: self.current_choice.clone(),
            model_id,
            summary: summary.to_owned(),
        });
        id
    }

    fn report(self, model_hash: &str) -> AuditReport {
        let snapshot_prefix = &self.snapshot.snapshot_id[..12.min(self.snapshot.snapshot_id.len())];
        let findings = self
            .capabilities
            .iter()
            .enumerate()
            .map(|(index, capability)| {
                let conclusion = if !capability.invocations.is_empty() {
                    Conclusion::CandidateInvocation
                } else if capability.unresolved.is_empty() {
                    Conclusion::AbsentWithinModel
                } else {
                    Conclusion::Unresolved
                };
                AuditFinding {
                    finding_id: format!("{snapshot_prefix}-F{index}"),
                    key: capability.key.clone(),
                    factory_callsite: capability.callsite.clone(),
                    choice: capability.choice.clone(),
                    origins: vec![self.finding_ref(capability.origin)],
                    registrations: capability
                        .registrations
                        .iter()
                        .map(|&evidence| self.finding_ref(evidence))
                        .collect(),
                    invocations: capability
                        .invocations
                        .iter()
                        .map(|invocation| self.finding_ref(invocation.evidence))
                        .collect(),
                    unresolved: capability
                        .unresolved
                        .iter()
                        .map(|&evidence| self.finding_ref(evidence))
                        .collect(),
                    assumptions: capability.assumptions.clone(),
                    conclusion,
                }
            })
            .collect();
        let roots = self
            .project
            .config
            .entries
            .iter()
            .map(|entry| format!("{}#{}", entry.module.display(), entry.export))
            .collect();
        AuditReport {
            schema_version: 1,
            snapshot_id: self.snapshot.snapshot_id.clone(),
            config_hash: self.snapshot.config_hash.clone(),
            model_hash: model_hash.to_owned(),
            model_id: self.model.id,
            findings,
            evidence: self.evidence,
            coverage: Coverage {
                scope: "reachable from configured roots with finite input domains".to_owned(),
                roots,
                processed_files: self.snapshot.files.len(),
                complete: self.diagnostics.is_empty() && self.coverage_gaps.is_empty(),
                gaps: self.coverage_gaps,
            },
            diagnostics: self.diagnostics,
        }
    }

    fn query_report(self, query: &QuerySpec, query_hash: &str) -> QueryReport {
        let snapshot_prefix = &self.snapshot.snapshot_id[..12.min(self.snapshot.snapshot_id.len())];
        let creations =
            self.capabilities
                .iter()
                .enumerate()
                .filter_map(|(index, capability)| {
                    if !query.report.include_non_invoked && capability.invocations.is_empty() {
                        return None;
                    }
                    let conclusion = if !capability.invocations.is_empty() {
                        Conclusion::CandidateInvocation
                    } else if capability.unresolved.is_empty() {
                        Conclusion::AbsentWithinModel
                    } else {
                        Conclusion::Unresolved
                    };
                    let factory_arguments = query
                        .factory_arguments
                        .iter()
                        .map(|projection| {
                            (
                                projection.label.clone(),
                                capability
                                    .factory_arguments
                                    .get(projection.index)
                                    .map_or_else(
                                        || QueryValue::Unknown {
                                            reason: "missing_argument".to_owned(),
                                        },
                                        query_value,
                                    ),
                            )
                        })
                        .collect();
                    let factory_argument_evidence = query
                        .factory_arguments
                        .iter()
                        .map(|projection| {
                            (
                                projection.label.clone(),
                                capability.factory_arguments.get(projection.index).and_then(
                                    |value| value.evidence.map(|id| format!("E{}", id.0)),
                                ),
                            )
                        })
                        .collect();
                    let invocations = capability
                        .invocations
                        .iter()
                        .map(|invocation| QueryInvocation {
                            evidence_id: format!("E{}", invocation.evidence.0),
                            callsite: self.evidence[invocation.evidence.0 as usize].span.clone(),
                            location: self.query_location(
                                &self.evidence[invocation.evidence.0 as usize].span,
                            ),
                            call_path: merge_trace(&capability.origin_trace, &invocation.call_path)
                                .iter()
                                .filter_map(|step| {
                                    self.query_location(&step.span).map(|location| {
                                        QueryCallPathStep {
                                            kind: step.kind,
                                            location,
                                        }
                                    })
                                })
                                .collect(),
                            arguments: query
                                .capability
                                .invocation_arguments
                                .iter()
                                .map(|projection| {
                                    (
                                        projection.label.clone(),
                                        invocation.arguments.get(projection.index).map_or_else(
                                            || QueryValue::Unknown {
                                                reason: "missing_argument".to_owned(),
                                            },
                                            query_value,
                                        ),
                                    )
                                })
                                .collect(),
                            argument_evidence: query
                                .capability
                                .invocation_arguments
                                .iter()
                                .map(|projection| {
                                    (
                                        projection.label.clone(),
                                        invocation.arguments.get(projection.index).and_then(
                                            |value| value.evidence.map(|id| format!("E{}", id.0)),
                                        ),
                                    )
                                })
                                .collect(),
                        })
                        .collect();
                    Some(QueryCreation {
                        creation_id: format!("{snapshot_prefix}-Q{index}"),
                        factory_callsite: capability.callsite.clone(),
                        factory_location: self.query_location(&capability.callsite),
                        reachability: capability.reachability,
                        reverse_importer: capability.reverse_importer.as_ref().map(|seed| {
                            QueryReverseImporter {
                                symbol: seed.symbol.clone(),
                                matched_imports: seed.matched_imports.clone(),
                                evaluation: seed.evaluation,
                                span: seed.span.clone(),
                                location: self.query_location(&seed.span),
                            }
                        }),
                        choice: capability.choice.clone(),
                        factory_arguments,
                        factory_argument_evidence,
                        capability_path: query.capability.returned_index.map_or_else(
                            || query.capability.returned_property.clone(),
                            |index| vec![format!("[{index}]")],
                        ),
                        registrations: if query.report.include_registrations {
                            capability
                                .registrations
                                .iter()
                                .map(|&evidence| self.finding_ref(evidence))
                                .collect()
                        } else {
                            Vec::new()
                        },
                        invocations,
                        unresolved: if query.report.include_unresolved_escapes {
                            capability
                                .unresolved
                                .iter()
                                .map(|&evidence| self.finding_ref(evidence))
                                .collect()
                        } else {
                            Vec::new()
                        },
                        unresolved_count: capability.unresolved.len(),
                        all_unresolved_evidence_ids: capability
                            .unresolved
                            .iter()
                            .map(|id| format!("E{}", id.0))
                            .collect(),
                        conclusion,
                    })
                })
                .collect();
        let roots = self
            .project
            .config
            .entries
            .iter()
            .map(|entry| format!("{}#{}", entry.module.display(), entry.export))
            .collect();
        let gaps = self
            .query_gaps
            .iter()
            .map(|(summary, kind, span, choice)| {
                QueryGap::new(
                    kind.clone(),
                    summary.clone(),
                    Some(span.clone()),
                    self.query_location(span),
                    choice.clone(),
                )
            })
            .collect();
        QueryReport {
            schema_version: 8,
            snapshot_id: self.snapshot.snapshot_id.clone(),
            config_hash: self.snapshot.config_hash.clone(),
            query_hash: query_hash.to_owned(),
            query_id: query.id.clone(),
            kind: query.kind,
            scope: query.scope,
            creations,
            callsite_inventory: QueryCallsiteInventory {
                configured_files: 0,
                candidate_files: 0,
                skipped_candidate_files: 0,
                round_limit_hit: false,
                callsites: Vec::new(),
            },
            evidence: self.evidence,
            gaps,
            coverage: Coverage {
                scope: match query.scope {
                    QueryScope::Reachable => {
                        "reachable from configured roots with finite input domains".to_owned()
                    }
                    QueryScope::AllCreations => {
                        "all matched creations; reachability explored from configured roots"
                            .to_owned()
                    }
                },
                roots,
                processed_files: self.snapshot.files.len(),
                complete: self.diagnostics.is_empty() && self.coverage_gaps.is_empty(),
                gaps: self.coverage_gaps,
            },
            diagnostics: self.diagnostics,
        }
    }

    fn finding_ref(&self, evidence_id: EvidenceId) -> FindingRef {
        let evidence = &self.evidence[evidence_id.0 as usize];
        FindingRef {
            finding_id: format!("E{}", evidence_id.0),
            summary: evidence.summary.clone(),
            span: evidence.span.clone(),
        }
    }

    fn query_location(&self, span: &SourceSpan) -> Option<QueryLocation> {
        let file = self.symbol_linker.file(span.file_id)?;
        if !self.location_sources.borrow().contains_key(&span.file_id) {
            let source = LocationSource::new(fs::read_to_string(&file.path).ok()?);
            self.location_sources
                .borrow_mut()
                .insert(span.file_id, source);
        }
        let sources = self.location_sources.borrow();
        let source = sources.get(&span.file_id)?;
        let (start_line, start_column) = source.line_column(span.start);
        let (end_line, end_column) = source.line_column(span.end);
        Some(QueryLocation {
            path: display_path(&self.project.root, &file.path),
            start_line,
            start_column,
            end_line,
            end_column,
        })
    }

    fn trace_at(&self, kind: QueryCallPathKind, span: &SourceSpan) -> Vec<TraceStep> {
        let mut trace = self.trace.clone();
        if let Some(last) = trace.last_mut()
            && last.span == *span
        {
            last.kind = kind;
        } else {
            trace.push(TraceStep {
                kind,
                span: span.clone(),
            });
        }
        trace
    }
}

fn merge_trace(prefix: &[TraceStep], continuation: &[TraceStep]) -> Vec<TraceStep> {
    let common = prefix
        .iter()
        .zip(continuation)
        .take_while(|(left, right)| left == right)
        .count();
    let mut result = prefix.to_vec();
    result.extend_from_slice(&continuation[common..]);
    result
}

fn gap_kind(reason: &str) -> String {
    if reason.starts_with("array map receiver is unknown") {
        return "array_map_receiver_unknown".to_owned();
    }
    if reason.starts_with("property read ") && reason.ends_with(" from non-record value") {
        return "property_read_from_non_record".to_owned();
    }
    if reason.starts_with("unknown property ") {
        return "unknown_property".to_owned();
    }
    let reason = reason.split_once(':').map_or(reason, |(prefix, _)| prefix);
    let mut kind = String::new();
    for word in reason.split_whitespace().take(7) {
        if !kind.is_empty() {
            kind.push('_');
        }
        kind.extend(
            word.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .map(|c| c.to_ascii_lowercase()),
        );
    }
    kind.trim_matches('_').to_owned()
}

fn statements_render_jsx(statements: &[FlowStatement]) -> bool {
    statements.iter().any(|statement| match statement {
        FlowStatement::Return {
            value: Some(value), ..
        } => expression_renders_jsx(value),
        FlowStatement::If {
            consequent,
            alternate,
            ..
        } => statements_render_jsx(consequent) || statements_render_jsx(alternate),
        _ => false,
    })
}

fn arrow_body_renders_jsx(body: &FlowArrowBody) -> bool {
    match body {
        FlowArrowBody::Expression { expression } => expression_renders_jsx(expression),
        FlowArrowBody::Statements { statements } => statements_render_jsx(statements),
    }
}

/// Returns whether a returned expression syntactically produces JSX.
fn expression_renders_jsx(expression: &FlowExpression) -> bool {
    match &expression.kind {
        FlowExpressionKind::JsxElement { .. } => true,
        FlowExpressionKind::Array { elements } => elements.iter().any(expression_renders_jsx),
        FlowExpressionKind::Spread { value } => expression_renders_jsx(value),
        FlowExpressionKind::Logical { left, right, .. } => {
            expression_renders_jsx(left) || expression_renders_jsx(right)
        }
        FlowExpressionKind::Conditional {
            consequent,
            alternate,
            ..
        } => expression_renders_jsx(consequent) || expression_renders_jsx(alternate),
        FlowExpressionKind::Call { arguments, .. } => arguments.iter().any(|argument| {
            matches!(&argument.kind, FlowExpressionKind::Arrow { body, .. } if arrow_body_renders_jsx(body))
        }),
        _ => false,
    }
}

fn capability_ids(value: &TrackedValue) -> Vec<usize> {
    let mut ids = Vec::new();
    collect_capability_ids(value, &mut ids);
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn collect_heap_alternatives(value: &AbstractValue, alternatives: &mut Vec<TrackedValue>) {
    if alternatives.len() > MAX_HEAP_ALTERNATIVES {
        return;
    }
    if let AbstractValue::Union(values) = value {
        for value in values.iter() {
            collect_heap_alternatives(&value.value, alternatives);
            if alternatives.len() > MAX_HEAP_ALTERNATIVES {
                break;
            }
        }
    } else {
        alternatives.push(TrackedValue::plain(value.clone()));
    }
}

fn append_array_values(receiver: &AbstractValue, added: &[TrackedValue]) -> Option<AbstractValue> {
    match receiver {
        AbstractValue::Array(elements) => {
            let mut result = elements.to_vec();
            result.extend_from_slice(added);
            Some(AbstractValue::array(result))
        }
        AbstractValue::Union(values) => Some(AbstractValue::union(
            values
                .iter()
                .map(|value| {
                    Some(TrackedValue {
                        value: append_array_values(&value.value, added)?,
                        evidence: value.evidence,
                        choice: value.choice.clone(),
                        heap_id: value.heap_id,
                    })
                })
                .collect::<Option<Vec<_>>>()?,
        )),
        _ => None,
    }
}

fn finite_array_parts(value: &AbstractValue) -> Option<Vec<Vec<TrackedValue>>> {
    match value {
        AbstractValue::Array(elements) => Some(vec![elements.to_vec()]),
        AbstractValue::Union(values) => {
            let mut parts = Vec::new();
            for value in values.iter() {
                parts.extend(finite_array_parts(&value.value)?);
                if parts.len() > 64 {
                    return None;
                }
            }
            Some(parts)
        }
        _ => None,
    }
}

fn object_values(value: &AbstractValue) -> Option<AbstractValue> {
    match value {
        AbstractValue::Record(fields) => {
            Some(AbstractValue::array(fields.values().cloned().collect()))
        }
        AbstractValue::Union(values) => Some(AbstractValue::union(
            values
                .iter()
                .map(|value| Some(TrackedValue::plain(object_values(&value.value)?)))
                .collect::<Option<Vec<_>>>()?,
        )),
        _ => None,
    }
}

fn object_values_order_is_unknown(value: &AbstractValue) -> bool {
    match value {
        AbstractValue::Record(fields) => fields.len() > 1,
        AbstractValue::Union(values) => values
            .iter()
            .any(|value| object_values_order_is_unknown(&value.value)),
        _ => false,
    }
}

fn collect_namespace_ids(value: &TrackedValue, ids: &mut BTreeSet<FileId>) {
    match &value.value {
        AbstractValue::Namespace(file_id) => {
            ids.insert(*file_id);
        }
        AbstractValue::Record(fields) => {
            for value in fields.values() {
                collect_namespace_ids(value, ids);
            }
        }
        AbstractValue::Array(values) | AbstractValue::Union(values) => {
            for value in values.iter() {
                collect_namespace_ids(value, ids);
            }
        }
        AbstractValue::Closure(closure) => {
            for value in closure.environment.values() {
                collect_namespace_ids(value, ids);
            }
        }
        AbstractValue::Element(element) => {
            collect_namespace_ids(&element.component, ids);
            for value in element.props.values() {
                collect_namespace_ids(value, ids);
            }
        }
        _ => {}
    }
}

fn captures_binding(value: &TrackedValue, name: &str) -> bool {
    match &value.value {
        AbstractValue::Closure(closure) => {
            let mut references = ClosureReferences::default();
            collect_body_references(&closure.body, &mut references);
            closure.environment.contains_key(name)
                && references.names.contains(name)
                && !closure
                    .params
                    .iter()
                    .any(|pattern| pattern_names(pattern).contains(&name))
        }
        AbstractValue::Record(fields) => fields.values().any(|value| captures_binding(value, name)),
        AbstractValue::Array(values) | AbstractValue::Union(values) => {
            values.iter().any(|value| captures_binding(value, name))
        }
        AbstractValue::Element(element) => {
            captures_binding(&element.component, name)
                || element
                    .props
                    .values()
                    .any(|value| captures_binding(value, name))
        }
        _ => false,
    }
}

#[derive(Default)]
struct ClosureReferences {
    names: BTreeSet<String>,
    modules: BTreeSet<String>,
    has_unsupported: bool,
}

fn collect_body_references(body: &FlowArrowBody, references: &mut ClosureReferences) {
    match body {
        FlowArrowBody::Expression { expression } => {
            collect_expression_references(expression, references);
        }
        FlowArrowBody::Statements { statements } => {
            for statement in statements {
                collect_statement_references(statement, references);
            }
        }
    }
}

fn collect_expression_references(expression: &FlowExpression, references: &mut ClosureReferences) {
    match &expression.kind {
        FlowExpressionKind::Identifier {
            name,
            module_binding,
        } => {
            references.names.insert(name.clone());
            if *module_binding {
                references.modules.insert(name.clone());
            }
        }
        FlowExpressionKind::Record { fields } => {
            for field in fields {
                collect_expression_references(&field.value, references);
            }
        }
        FlowExpressionKind::Array { elements } => {
            for value in elements {
                collect_expression_references(value, references);
            }
        }
        FlowExpressionKind::Spread { value } => collect_expression_references(value, references),
        FlowExpressionKind::StaticMember { object, .. } => {
            collect_expression_references(object, references);
        }
        FlowExpressionKind::ComputedMember { object, property } => {
            collect_expression_references(object, references);
            collect_expression_references(property, references);
        }
        FlowExpressionKind::StrictEquality { left, right, .. }
        | FlowExpressionKind::Logical { left, right, .. } => {
            collect_expression_references(left, references);
            collect_expression_references(right, references);
        }
        FlowExpressionKind::LooseNullEquality { value, .. }
        | FlowExpressionKind::LogicalNot { value } => {
            collect_expression_references(value, references);
        }
        FlowExpressionKind::Conditional {
            test,
            consequent,
            alternate,
        } => {
            for value in [test, consequent, alternate] {
                collect_expression_references(value, references);
            }
        }
        FlowExpressionKind::Call { callee, arguments } => {
            collect_expression_references(callee, references);
            for value in arguments {
                collect_expression_references(value, references);
            }
        }
        FlowExpressionKind::Arrow { body, .. } => collect_body_references(body, references),
        FlowExpressionKind::JsxElement { tag, props } => {
            if matches!(tag, FlowJsxTag::Unsupported { .. }) {
                references.has_unsupported = true;
            }
            if let FlowJsxTag::Identifier {
                name,
                intrinsic: false,
                module_binding,
            } = tag
            {
                references.names.insert(name.clone());
                if *module_binding {
                    references.modules.insert(name.clone());
                }
            }
            if let FlowJsxTag::Member {
                object,
                module_binding,
                ..
            } = tag
            {
                references.names.insert(object.clone());
                if *module_binding {
                    references.modules.insert(object.clone());
                }
            }
            for prop in props {
                match prop {
                    FlowJsxProp::Property { value, .. } | FlowJsxProp::Spread { value, .. } => {
                        collect_expression_references(value, references);
                    }
                    FlowJsxProp::Unsupported(_) => references.has_unsupported = true,
                }
            }
        }
        FlowExpressionKind::Unsupported {
            references: names, ..
        } => {
            references.names.extend(names.iter().cloned());
            references.modules.extend(names.iter().cloned());
        }
        _ => {}
    }
}

fn collect_statement_references(statement: &FlowStatement, references: &mut ClosureReferences) {
    match statement {
        FlowStatement::Bind(binding) => {
            collect_expression_references(&binding.value, references);
        }
        FlowStatement::Expression { value, .. }
        | FlowStatement::Return {
            value: Some(value), ..
        } => collect_expression_references(value, references),
        FlowStatement::Assign { target, value, .. } => {
            collect_expression_references(value, references);
            match target {
                FlowAssignmentTarget::Identifier { name } => {
                    references.names.insert(name.clone());
                }
                FlowAssignmentTarget::StaticMember { object, .. } => {
                    collect_expression_references(object, references);
                }
                FlowAssignmentTarget::ComputedMember { object, property } => {
                    collect_expression_references(object, references);
                    collect_expression_references(property, references);
                }
                FlowAssignmentTarget::Unsupported { .. } => references.has_unsupported = true,
            }
        }
        FlowStatement::If {
            test,
            consequent,
            alternate,
            ..
        } => {
            collect_expression_references(test, references);
            for statement in consequent.iter().chain(alternate) {
                collect_statement_references(statement, references);
            }
        }
        FlowStatement::Unsupported(_) => references.has_unsupported = true,
        FlowStatement::Return { value: None, .. } => {}
    }
}

pub(crate) fn flow_statement_names(statements: &[FlowStatement]) -> BTreeSet<String> {
    let mut references = ClosureReferences::default();
    for statement in statements {
        collect_statement_references(statement, &mut references);
    }
    references.names
}

pub(crate) fn flow_expression_names(expression: &FlowExpression) -> BTreeSet<String> {
    let mut references = ClosureReferences::default();
    collect_expression_references(expression, &mut references);
    references.names
}

fn extend_binding_dependencies(statements: &[FlowStatement], names: &mut BTreeSet<String>) {
    for statement in statements {
        match statement {
            FlowStatement::Bind(binding)
                if pattern_names(&binding.pattern)
                    .iter()
                    .any(|name| names.contains(*name)) =>
            {
                let mut references = ClosureReferences::default();
                collect_expression_references(&binding.value, &mut references);
                names.extend(references.names);
            }
            FlowStatement::Assign {
                target: FlowAssignmentTarget::Identifier { name },
                value,
                ..
            } if names.contains(name) => {
                let mut references = ClosureReferences::default();
                collect_expression_references(value, &mut references);
                names.extend(references.names);
            }
            FlowStatement::If {
                consequent,
                alternate,
                ..
            } => {
                extend_binding_dependencies(consequent, names);
                extend_binding_dependencies(alternate, names);
            }
            _ => {}
        }
    }
}

fn collect_capability_ids(value: &TrackedValue, ids: &mut Vec<usize>) {
    match &value.value {
        AbstractValue::Capability(id) => ids.push(*id),
        AbstractValue::Record(fields) => {
            for field in fields.values() {
                collect_capability_ids(field, ids);
            }
        }
        AbstractValue::Array(elements) | AbstractValue::Union(elements) => {
            for element in elements.iter() {
                collect_capability_ids(element, ids);
            }
        }
        AbstractValue::Closure(closure) => {
            for captured in closure.environment.values() {
                collect_capability_ids(captured, ids);
            }
        }
        AbstractValue::Element(element) => {
            collect_capability_ids(&element.component, ids);
            for prop in element.props.values() {
                collect_capability_ids(prop, ids);
            }
        }
        _ => {}
    }
}

fn returns_capability_data(value: &TrackedValue) -> bool {
    match &value.value {
        AbstractValue::Capability(_) | AbstractValue::Closure(_) => {
            !capability_ids(value).is_empty()
        }
        AbstractValue::Record(fields) => fields.values().any(returns_capability_data),
        AbstractValue::Array(values) | AbstractValue::Union(values) => {
            values.iter().any(returns_capability_data)
        }
        _ => false,
    }
}

fn render_compact_value(value: &TrackedValue) -> String {
    match &value.value {
        AbstractValue::Null => "null".to_owned(),
        AbstractValue::String(value) => value.clone(),
        AbstractValue::Number(value) => value.to_string(),
        AbstractValue::EnumMember {
            enum_name,
            member_name,
            ..
        } => format!("{enum_name}.{member_name}"),
        AbstractValue::Boolean(value) => value.to_string(),
        AbstractValue::Array(elements) => format!(
            "[{}]",
            elements
                .iter()
                .map(render_compact_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        AbstractValue::Undefined => "undefined".to_owned(),
        AbstractValue::Unknown(reason) | AbstractValue::AssumedWrapper { reason, .. } => {
            format!("<{reason}>")
        }
        AbstractValue::Union(values) => format!(
            "({})",
            values
                .iter()
                .map(render_compact_value)
                .collect::<Vec<_>>()
                .join(" | ")
        ),
        _ => "<symbolic>".to_owned(),
    }
}

fn query_value(value: &TrackedValue) -> QueryValue {
    match &value.value {
        AbstractValue::Null => QueryValue::Null,
        AbstractValue::String(value) => QueryValue::String {
            value: value.clone(),
        },
        AbstractValue::Number(value) => QueryValue::Number { value: *value },
        AbstractValue::EnumMember {
            enum_name,
            member_name,
            value,
        } => QueryValue::EnumMember {
            enum_name: enum_name.clone(),
            member_name: member_name.clone(),
            value: *value,
        },
        AbstractValue::Array(elements) => QueryValue::Array {
            elements: elements.iter().map(query_value).collect(),
        },
        AbstractValue::Undefined => QueryValue::Undefined,
        AbstractValue::Unknown(reason) | AbstractValue::AssumedWrapper { reason, .. } => {
            QueryValue::Unknown {
                reason: reason.clone(),
            }
        }
        AbstractValue::Boolean(value) => QueryValue::Boolean { value: *value },
        AbstractValue::Union(values) if array_alternatives(value) || enum_alternatives(value) => {
            QueryValue::Alternatives {
                values: values.iter().map(query_value).collect(),
            }
        }
        AbstractValue::Union(_) => QueryValue::Unknown {
            reason: "joined_alternatives".to_owned(),
        },
        AbstractValue::Record(_) => QueryValue::Unknown {
            reason: "record_value".to_owned(),
        },
        AbstractValue::Function(_)
        | AbstractValue::Namespace(_)
        | AbstractValue::ModelFunction
        | AbstractValue::Closure(_)
        | AbstractValue::Capability(_)
        | AbstractValue::Element(_)
        | AbstractValue::Intrinsic(_)
        | AbstractValue::ConfiguredComponent(_) => QueryValue::Unknown {
            reason: "non_data_value".to_owned(),
        },
    }
}

fn exact_equality(left: &AbstractValue, right: &AbstractValue) -> Option<bool> {
    match (left, right) {
        (AbstractValue::Null, AbstractValue::Null) => Some(true),
        (AbstractValue::String(left), AbstractValue::String(right)) => Some(left == right),
        (AbstractValue::Number(left), AbstractValue::Number(right)) => Some(left == right),
        (
            AbstractValue::EnumMember { value: left, .. },
            AbstractValue::EnumMember { value: right, .. },
        ) => Some(left == right),
        (AbstractValue::EnumMember { value: left, .. }, AbstractValue::Number(right))
        | (AbstractValue::Number(left), AbstractValue::EnumMember { value: right, .. }) => {
            Some(left == right)
        }
        (AbstractValue::Boolean(left), AbstractValue::Boolean(right)) => Some(left == right),
        (AbstractValue::Undefined, AbstractValue::Undefined) => Some(true),
        (AbstractValue::Null, AbstractValue::Undefined)
        | (AbstractValue::Undefined, AbstractValue::Null) => Some(false),
        (
            AbstractValue::String(_) | AbstractValue::Boolean(_) | AbstractValue::Undefined,
            AbstractValue::String(_) | AbstractValue::Boolean(_) | AbstractValue::Undefined,
        ) => Some(false),
        _ => None,
    }
}

fn nullish(value: &AbstractValue) -> Option<bool> {
    match value {
        AbstractValue::Null | AbstractValue::Undefined => Some(true),
        AbstractValue::Unknown(_) | AbstractValue::AssumedWrapper { .. } => None,
        AbstractValue::Union(values) => {
            same_known(values.iter().map(|value| nullish(&value.value)))
        }
        _ => Some(false),
    }
}

fn refine_environment_for_condition(
    condition: &FlowExpression,
    expected: bool,
    environment: &mut Environment,
) {
    match &condition.kind {
        FlowExpressionKind::Logical {
            left,
            right,
            operator: FlowLogicalOperator::And,
        } if expected => {
            refine_environment_for_condition(left, true, environment);
            refine_environment_for_condition(right, true, environment);
        }
        FlowExpressionKind::Logical {
            left,
            right,
            operator: FlowLogicalOperator::Or,
        } if !expected => {
            refine_environment_for_condition(left, false, environment);
            refine_environment_for_condition(right, false, environment);
        }
        FlowExpressionKind::LogicalNot { value } => {
            refine_environment_for_condition(value, !expected, environment);
        }
        FlowExpressionKind::LooseNullEquality { value, negated } => {
            let FlowExpressionKind::Identifier { name, .. } = &value.kind else {
                return;
            };
            let Some(binding) = environment.get_mut(name) else {
                return;
            };
            let AbstractValue::Union(alternatives) = &binding.value else {
                return;
            };
            let wants_nullish = expected != *negated;
            let mut narrowed = alternatives
                .iter()
                .filter(|alternative| {
                    nullish(&alternative.value).is_none_or(|is_nullish| is_nullish == wants_nullish)
                })
                .cloned()
                .collect::<Vec<_>>();
            if narrowed.len() == 1 {
                binding.value = narrowed.remove(0).value;
            } else if !narrowed.is_empty() {
                binding.value = AbstractValue::union(narrowed);
            }
        }
        _ => {}
    }
}

fn truthy(value: &AbstractValue) -> Option<bool> {
    match value {
        AbstractValue::Null | AbstractValue::Undefined => Some(false),
        AbstractValue::Boolean(value) => Some(*value),
        AbstractValue::Number(value) => Some(*value != 0),
        AbstractValue::EnumMember { value, .. } => Some(*value != 0),
        AbstractValue::String(value) => Some(!value.is_empty()),
        AbstractValue::Unknown(_) | AbstractValue::AssumedWrapper { .. } => None,
        AbstractValue::Union(values) => same_known(values.iter().map(|value| truthy(&value.value))),
        _ => Some(true),
    }
}

fn same_known(values: impl Iterator<Item = Option<bool>>) -> Option<bool> {
    let mut values = values;
    let first = values.next()??;
    values.all(|value| value == Some(first)).then_some(first)
}

fn value_is_uncertain(value: &TrackedValue) -> bool {
    match &value.value {
        AbstractValue::Unknown(_) | AbstractValue::AssumedWrapper { .. } => true,
        AbstractValue::Union(values) => {
            !finite_array_alternatives(value) || values.iter().any(value_is_uncertain)
        }
        AbstractValue::Array(values) => values.iter().any(value_is_uncertain),
        AbstractValue::Record(fields) => fields.values().any(value_is_uncertain),
        _ => false,
    }
}

fn finite_array_alternatives(value: &TrackedValue) -> bool {
    match &value.value {
        AbstractValue::Array(elements) => {
            elements.iter().all(|element| !value_is_uncertain(element))
        }
        AbstractValue::Union(values) => values.iter().all(finite_array_alternatives),
        _ => false,
    }
}

fn array_alternatives(value: &TrackedValue) -> bool {
    match &value.value {
        AbstractValue::Array(_) => true,
        AbstractValue::Union(values) => values.iter().all(array_alternatives),
        _ => false,
    }
}

fn enum_alternatives(value: &TrackedValue) -> bool {
    match &value.value {
        AbstractValue::EnumMember { .. } | AbstractValue::Undefined => true,
        AbstractValue::Union(values) => values.iter().all(enum_alternatives),
        _ => false,
    }
}

fn collect_factory_calls(
    expression: &FlowExpression,
    file_id: FileId,
    enclosing_function: Option<&FunctionKey>,
    candidates: &mut Vec<FactoryCallCandidate>,
) {
    match &expression.kind {
        FlowExpressionKind::Call { callee, arguments } => {
            candidates.push(FactoryCallCandidate {
                file_id,
                callee: callee.as_ref().clone(),
                span: expression.span.clone(),
                arguments: arguments.clone(),
                enclosing_function: enclosing_function.cloned(),
            });
            collect_factory_calls(callee, file_id, enclosing_function, candidates);
            for argument in arguments {
                collect_factory_calls(argument, file_id, enclosing_function, candidates);
            }
        }
        FlowExpressionKind::Record { fields } => {
            for field in fields {
                collect_factory_calls(&field.value, file_id, enclosing_function, candidates);
            }
        }
        FlowExpressionKind::Array { elements } => {
            for element in elements {
                collect_factory_calls(element, file_id, enclosing_function, candidates);
            }
        }
        FlowExpressionKind::Spread { value } => {
            collect_factory_calls(value, file_id, enclosing_function, candidates);
        }
        FlowExpressionKind::StaticMember { object, .. } => {
            collect_factory_calls(object, file_id, enclosing_function, candidates);
        }
        FlowExpressionKind::ComputedMember { object, property } => {
            collect_factory_calls(object, file_id, enclosing_function, candidates);
            collect_factory_calls(property, file_id, enclosing_function, candidates);
        }
        FlowExpressionKind::StrictEquality { left, right, .. }
        | FlowExpressionKind::Logical { left, right, .. } => {
            collect_factory_calls(left, file_id, enclosing_function, candidates);
            collect_factory_calls(right, file_id, enclosing_function, candidates);
        }
        FlowExpressionKind::LooseNullEquality { value, .. }
        | FlowExpressionKind::LogicalNot { value } => {
            collect_factory_calls(value, file_id, enclosing_function, candidates);
        }
        FlowExpressionKind::Conditional {
            test,
            consequent,
            alternate,
        } => {
            collect_factory_calls(test, file_id, enclosing_function, candidates);
            collect_factory_calls(consequent, file_id, enclosing_function, candidates);
            collect_factory_calls(alternate, file_id, enclosing_function, candidates);
        }
        FlowExpressionKind::Arrow { body, .. } => match body {
            FlowArrowBody::Expression { expression } => {
                collect_factory_calls(expression, file_id, enclosing_function, candidates);
            }
            FlowArrowBody::Statements { statements } => {
                for statement in statements {
                    collect_factory_calls_statement(
                        statement,
                        file_id,
                        enclosing_function,
                        candidates,
                    );
                }
            }
        },
        FlowExpressionKind::JsxElement { props, .. } => {
            for prop in props {
                match prop {
                    FlowJsxProp::Property { value, .. } | FlowJsxProp::Spread { value, .. } => {
                        collect_factory_calls(value, file_id, enclosing_function, candidates);
                    }
                    FlowJsxProp::Unsupported(_) => {}
                }
            }
        }
        FlowExpressionKind::Null
        | FlowExpressionKind::String { .. }
        | FlowExpressionKind::Number { .. }
        | FlowExpressionKind::NumericEnumMember { .. }
        | FlowExpressionKind::Boolean { .. }
        | FlowExpressionKind::Identifier { .. }
        | FlowExpressionKind::DynamicImport { .. }
        | FlowExpressionKind::Unsupported { .. } => {}
    }
}

fn collect_import_uses_statement(
    statement: &FlowStatement,
    imported_names: &BTreeSet<String>,
    uses: &mut Vec<FlowExpression>,
) {
    match statement {
        FlowStatement::Bind(binding) => {
            collect_import_uses(&binding.value, imported_names, uses);
        }
        FlowStatement::Return { value, .. } => {
            if let Some(value) = value {
                collect_import_uses(value, imported_names, uses);
            }
        }
        FlowStatement::Expression { value, .. } => {
            collect_import_uses(value, imported_names, uses);
        }
        FlowStatement::Assign { value, .. } => {
            collect_import_uses(value, imported_names, uses);
        }
        FlowStatement::If {
            test,
            consequent,
            alternate,
            ..
        } => {
            collect_import_uses(test, imported_names, uses);
            for statement in consequent.iter().chain(alternate) {
                collect_import_uses_statement(statement, imported_names, uses);
            }
        }
        FlowStatement::Unsupported(_) => {}
    }
}

fn collect_import_uses(
    expression: &FlowExpression,
    imported_names: &BTreeSet<String>,
    uses: &mut Vec<FlowExpression>,
) {
    match &expression.kind {
        FlowExpressionKind::Call { callee, arguments } => {
            if matches!(&callee.kind, FlowExpressionKind::Identifier { name, module_binding: true } if imported_names.contains(name))
                || matches!(&callee.kind, FlowExpressionKind::StaticMember { object, .. } if matches!(&object.kind, FlowExpressionKind::Identifier { name, module_binding: true } if imported_names.contains(name)))
            {
                uses.push(expression.clone());
                return;
            }
            collect_import_uses(callee, imported_names, uses);
            for argument in arguments {
                collect_import_uses(argument, imported_names, uses);
            }
        }
        FlowExpressionKind::JsxElement { tag, props } => {
            if matches!(tag, FlowJsxTag::Identifier { name, intrinsic: false, module_binding: true } if imported_names.contains(name))
            {
                uses.push(expression.clone());
                return;
            }
            for prop in props {
                match prop {
                    FlowJsxProp::Property { value, .. } | FlowJsxProp::Spread { value, .. } => {
                        collect_import_uses(value, imported_names, uses);
                    }
                    FlowJsxProp::Unsupported(_) => {}
                }
            }
        }
        FlowExpressionKind::Record { fields } => {
            for field in fields {
                collect_import_uses(&field.value, imported_names, uses);
            }
        }
        FlowExpressionKind::Array { elements } => {
            for element in elements {
                collect_import_uses(element, imported_names, uses);
            }
        }
        FlowExpressionKind::Spread { value }
        | FlowExpressionKind::StaticMember { object: value, .. }
        | FlowExpressionKind::LooseNullEquality { value, .. }
        | FlowExpressionKind::LogicalNot { value } => {
            collect_import_uses(value, imported_names, uses);
        }
        FlowExpressionKind::ComputedMember { object, property }
        | FlowExpressionKind::StrictEquality {
            left: object,
            right: property,
            ..
        }
        | FlowExpressionKind::Logical {
            left: object,
            right: property,
            ..
        } => {
            collect_import_uses(object, imported_names, uses);
            collect_import_uses(property, imported_names, uses);
        }
        FlowExpressionKind::Conditional {
            test,
            consequent,
            alternate,
        } => {
            for value in [test, consequent, alternate] {
                collect_import_uses(value, imported_names, uses);
            }
        }
        FlowExpressionKind::Arrow { body, .. } => match body {
            FlowArrowBody::Expression { expression } => {
                collect_import_uses(expression, imported_names, uses)
            }
            FlowArrowBody::Statements { statements } => {
                for statement in statements {
                    collect_import_uses_statement(statement, imported_names, uses);
                }
            }
        },
        FlowExpressionKind::Null
        | FlowExpressionKind::String { .. }
        | FlowExpressionKind::Number { .. }
        | FlowExpressionKind::NumericEnumMember { .. }
        | FlowExpressionKind::Boolean { .. }
        | FlowExpressionKind::Identifier { .. }
        | FlowExpressionKind::DynamicImport { .. }
        | FlowExpressionKind::Unsupported { .. } => {}
    }
}

fn collect_factory_calls_statement(
    statement: &FlowStatement,
    file_id: FileId,
    enclosing_function: Option<&FunctionKey>,
    candidates: &mut Vec<FactoryCallCandidate>,
) {
    match statement {
        FlowStatement::Bind(binding) => {
            collect_factory_calls(&binding.value, file_id, enclosing_function, candidates);
        }
        FlowStatement::Return { value, .. } => {
            if let Some(value) = value {
                collect_factory_calls(value, file_id, enclosing_function, candidates);
            }
        }
        FlowStatement::Expression { value, .. } => {
            collect_factory_calls(value, file_id, enclosing_function, candidates);
        }
        FlowStatement::Assign { target, value, .. } => {
            collect_factory_calls(value, file_id, enclosing_function, candidates);
            match target {
                FlowAssignmentTarget::StaticMember { object, .. } => {
                    collect_factory_calls(object, file_id, enclosing_function, candidates);
                }
                FlowAssignmentTarget::ComputedMember { object, property } => {
                    collect_factory_calls(object, file_id, enclosing_function, candidates);
                    collect_factory_calls(property, file_id, enclosing_function, candidates);
                }
                FlowAssignmentTarget::Identifier { .. }
                | FlowAssignmentTarget::Unsupported { .. } => {}
            }
        }
        FlowStatement::If {
            test,
            consequent,
            alternate,
            ..
        } => {
            collect_factory_calls(test, file_id, enclosing_function, candidates);
            for statement in consequent.iter().chain(alternate) {
                collect_factory_calls_statement(statement, file_id, enclosing_function, candidates);
            }
        }
        FlowStatement::Unsupported(_) => {}
    }
}

pub(crate) fn syntactic_callsite_spans(
    file: &crate::ir::FileIr,
    names: &BTreeSet<String>,
) -> Vec<SourceSpan> {
    let mut candidates = Vec::new();
    for binding in &file.flow.globals {
        collect_factory_calls(&binding.value, file.file_id, None, &mut candidates);
    }
    for function in &file.flow.functions {
        for statement in &function.body {
            collect_factory_calls_statement(statement, file.file_id, None, &mut candidates);
        }
    }
    let mut spans = candidates
        .into_iter()
        .filter(|candidate| match &candidate.callee.kind {
            FlowExpressionKind::Identifier { name, .. } => names.contains(name),
            FlowExpressionKind::StaticMember { property, .. } => names.contains(property),
            _ => false,
        })
        .map(|candidate| candidate.span)
        .collect::<Vec<_>>();
    spans.sort_by_key(|span| (span.start, span.end));
    spans.dedup();
    spans
}

fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn fallback_span(file_id: FileId) -> SourceSpan {
    SourceSpan {
        file_id,
        start: 0,
        end: 0,
    }
}

fn choice_label(props: &BTreeMap<String, String>) -> String {
    if props.is_empty() {
        return "<unconstrained>".to_owned();
    }
    if props.len() == 1 {
        return props.values().next().cloned().unwrap_or_default();
    }
    props
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join(",")
}
