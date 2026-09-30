#![allow(clippy::needless_pass_by_value, clippy::too_many_lines)]

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::Path,
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
        FlowStatement, SourceSpan,
    },
    link::{LinkedSymbol, LinkedValue, SymbolLinker, ValueResolution, pattern_names},
    models::{CallbackFactoryModel, CaptureSource, ModelEvidence, ModelValue, ModeledOperation},
    project::Project,
    queries::{AuditFinding, AuditReport, Conclusion, Coverage, FindingRef},
    query::{
        QueryCreation, QueryInvocation, QueryLocation, QueryReport, QueryScope, QuerySpec,
        QueryValue, Reachability,
    },
};

const MAX_CALL_DEPTH: usize = 128;
const MAX_REVERSE_IMPORTER_EVALUATIONS: usize = 5_000;
const MAX_REVERSE_IMPORTER_SOURCE_BYTES: usize = 20_000;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct FunctionKey {
    file_id: FileId,
    name: String,
}

#[derive(Clone)]
struct FunctionDef {
    key: FunctionKey,
    function: FlowFunction,
}

#[derive(Clone)]
struct TrackedValue {
    value: AbstractValue,
    evidence: Option<EvidenceId>,
    choice: Option<String>,
}

impl TrackedValue {
    fn plain(value: AbstractValue) -> Self {
        Self {
            value,
            evidence: None,
            choice: None,
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
    Record(BTreeMap<String, TrackedValue>),
    Array(Vec<TrackedValue>),
    Union(Vec<TrackedValue>),
    Function(FunctionKey),
    Namespace(FileId),
    ModelFunction,
    Closure(ClosureValue),
    Capability(usize),
    Element(ElementValue),
    Intrinsic(String),
    Undefined,
    Unknown(String),
}

#[derive(Clone)]
struct ClosureValue {
    body: FlowArrowBody,
    params: Vec<FlowPattern>,
    environment: Environment,
    file_id: FileId,
}

#[derive(Clone)]
struct ElementValue {
    component: Box<TrackedValue>,
    props: BTreeMap<String, TrackedValue>,
    span: SourceSpan,
}

struct CapabilityState {
    key: String,
    choice: String,
    callsite: SourceSpan,
    origin: EvidenceId,
    factory_arguments: Vec<TrackedValue>,
    reachability: Reachability,
    registrations: Vec<EvidenceId>,
    invocations: Vec<InvocationState>,
    unresolved: Vec<EvidenceId>,
    assumptions: Vec<String>,
}

struct InvocationState {
    evidence: EvidenceId,
    arguments: Vec<TrackedValue>,
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

pub fn execute_query(
    project: &Project,
    snapshot: &Snapshot,
    query: &QuerySpec,
    query_hash: &str,
    reverse_seed_paths: &BTreeSet<std::path::PathBuf>,
    reverse_producer_paths: &BTreeSet<std::path::PathBuf>,
) -> Result<(
    QueryReport,
    BTreeSet<std::path::PathBuf>,
    BTreeSet<std::path::PathBuf>,
)> {
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
    let phase_start = Instant::now();
    if project.config.entries.is_empty() {
        if query.scope == QueryScope::Reachable {
            bail!("a reachable query requires at least one configured entry point");
        }
        solver.prepare_globals(None);
    } else {
        solver.run()?;
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
    let requested_imports = solver.requested_imports.clone();
    if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
        eprintln!(
            "query module environments: hits={} misses={}",
            solver.module_env_cache_hits, solver.module_env_cache_misses
        );
    }
    let producer_paths = solver
        .capability_producer_files
        .iter()
        .filter_map(|file_id| snapshot.files.iter().find(|file| file.file_id == *file_id))
        .map(|file| file.path.clone())
        .collect();
    let phase_start = Instant::now();
    let report = solver.query_report(query, query_hash);
    if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
        eprintln!(
            "query solver report: {} ms",
            phase_start.elapsed().as_millis()
        );
    }
    Ok((report, requested_imports, producer_paths))
}

struct Solver<'a> {
    project: &'a Project,
    snapshot: &'a Snapshot,
    model: CallbackFactoryModel,
    functions: BTreeMap<FunctionKey, FunctionDef>,
    symbol_linker: SymbolLinker<'a>,
    model_symbol: LinkedSymbol,
    globals_ir: Vec<(FileId, FlowBinding)>,
    global_bindings: BTreeMap<LinkedSymbol, usize>,
    evaluating_globals: BTreeSet<usize>,
    initialized_globals: BTreeSet<usize>,
    globals: BTreeMap<LinkedSymbol, TrackedValue>,
    module_import_links: BTreeMap<FileId, Vec<(String, LinkedValue)>>,
    module_env_cache: BTreeMap<FileId, Environment>,
    module_env_cache_hits: usize,
    module_env_cache_misses: usize,
    evidence: Vec<Evidence>,
    capabilities: Vec<CapabilityState>,
    diagnostics: Vec<String>,
    coverage_gaps: Vec<String>,
    requested_imports: BTreeSet<std::path::PathBuf>,
    capability_producer_files: BTreeSet<FileId>,
    current_choice: Option<String>,
    current_reachability: Reachability,
    call_depth: usize,
    active_captures: Vec<BTreeSet<String>>,
    reverse_evaluations: usize,
    reverse_budget_active: bool,
    reverse_budget_reported: bool,
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
                functions.insert(
                    key.clone(),
                    FunctionDef {
                        key,
                        function: function.clone(),
                    },
                );
            }
            globals_ir.extend(
                file.flow
                    .globals
                    .iter()
                    .cloned()
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
            evidence: Vec::new(),
            capabilities: Vec::new(),
            diagnostics,
            coverage_gaps: if project.config.source_contains_any.is_empty() {
                Vec::new()
            } else {
                vec!["directory sources were text-filtered; files without a configured term were not analyzed".to_owned()]
            },
            requested_imports: BTreeSet::new(),
            capability_producer_files: BTreeSet::new(),
            current_choice: None,
            current_reachability: Reachability::Reachable,
            call_depth: 0,
            active_captures: Vec::new(),
            reverse_evaluations: 0,
            reverse_budget_active: false,
            reverse_budget_reported: false,
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
            let entry_file = self
                .snapshot
                .files
                .iter()
                .find(|file| file.path == entry_path)
                .with_context(|| {
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
                .function
                .params
                .len();

            for props in self.entry_input_combinations(&entry.export)? {
                let choice = choice_label(&props);
                self.current_choice = Some(choice.clone());
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
                            },
                        )
                    })
                    .collect();
                let mut arguments = Vec::with_capacity(parameter_count.max(1));
                if parameter_count > 0 {
                    arguments.push(TrackedValue {
                        value: AbstractValue::Record(record),
                        evidence: None,
                        choice: Some(choice),
                    });
                    arguments.extend(
                        (1..parameter_count)
                            .map(|_| TrackedValue::unknown("unconfigured_entry_argument")),
                    );
                }
                let returned = self.call_function(&entry_key, arguments);
                self.render(returned);
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
        let Some(file) = self
            .snapshot
            .files
            .iter()
            .find(|file| file.file_id == module)
        else {
            return;
        };
        for resolution in &self.snapshot.resolutions {
            if resolution.importer == file.path
                && let Some(dependency) = resolution
                    .resolved_path
                    .as_ref()
                    .and_then(|path| self.snapshot.files.iter().find(|file| file.path == *path))
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
                .snapshot
                .files
                .iter()
                .find(|file| file.file_id == file_id)
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
        let (file_id, binding) = self.globals_ir[index].clone();
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

    fn request_import_for_local(&mut self, file_id: FileId, local: &str) {
        let Some(file) = self
            .snapshot
            .files
            .iter()
            .find(|file| file.file_id == file_id)
        else {
            return;
        };
        let Some(import) = file
            .flow
            .imports
            .iter()
            .find(|import| import.local == local && !import.type_only)
        else {
            return;
        };
        let Some(path) = self
            .snapshot
            .resolutions
            .iter()
            .find(|resolution| {
                resolution.importer == file.path
                    && resolution.specifier == import.module
                    && resolution.status == crate::link::ResolutionStatus::Resolved
            })
            .and_then(|resolution| resolution.resolved_path.as_ref())
        else {
            return;
        };
        if !self.snapshot.files.iter().any(|file| file.path == *path) {
            self.requested_imports.insert(path.clone());
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
        if !self.coverage_gaps.contains(&gap) {
            self.coverage_gaps.push(gap);
        }
    }

    fn call_function(&mut self, key: &FunctionKey, arguments: Vec<TrackedValue>) -> TrackedValue {
        if self.call_depth >= MAX_CALL_DEPTH {
            self.mark_values_unresolved(
                arguments.iter(),
                "call depth budget exhausted",
                self.functions.get(key).map_or_else(
                    || fallback_span(key.file_id),
                    |def| def.function.span.clone(),
                ),
            );
            return TrackedValue::unknown("call_depth_budget_exhausted");
        }
        let Some(definition) = self.functions.get(key).cloned() else {
            return TrackedValue::unknown(format!("missing_function:{}", key.name));
        };
        self.call_depth += 1;
        let mut environment = self.module_environment(key.file_id);
        for (index, pattern) in definition.function.params.iter().enumerate() {
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
        let returned = self.execute_statements(
            &definition.function.body,
            &mut environment,
            definition.key.file_id,
        );
        self.active_captures.pop();
        self.call_depth -= 1;
        let returned = returned.unwrap_or_else(|| TrackedValue::plain(AbstractValue::Undefined));
        if returns_capability_data(&returned) && self.snapshot.files.iter().any(|file| {
            file.file_id == key.file_id
                && file.flow.exports.iter().any(|export| matches!(export, crate::ir::FlowExport::Local { local, type_only: false, .. } if local == &key.name))
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
                            let right =
                                self.execute_branch(alternate, &mut alternate_environment, file_id);
                            match (left, right) {
                                (Some(left), Some(right)) => {
                                    return Some(TrackedValue::plain(AbstractValue::Union(vec![
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
                                    return Some(TrackedValue::plain(AbstractValue::Union(vec![
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
                                    return Some(TrackedValue::plain(AbstractValue::Union(vec![
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
        let Some(previous) = environment.get(name).cloned() else {
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
                value,
                evidence: Some(evidence),
                choice: previous.choice,
            },
        );
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
            environment.insert(
                name,
                TrackedValue {
                    value: AbstractValue::Union(values.into()),
                    evidence: Some(evidence),
                    choice: self.current_choice.clone(),
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
        let Some(previous) = environment.get(name).cloned() else {
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
        let affected_capabilities =
            !capability_ids(&previous).is_empty() || !capability_ids(&value).is_empty();
        if affected_capabilities {
            self.mark_values_unresolved(
                [&previous, &value].into_iter(),
                "record property mutation may be observed through an alias",
                span.clone(),
            );
            self.coverage_gaps.push(format!(
                "record aliasing around mutation at file {} bytes {}..{}",
                span.file_id.0, span.start, span.end
            ));
        }
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
        fields.insert(property.to_owned(), value);
        environment.insert(
            name.clone(),
            TrackedValue {
                value: AbstractValue::Record(fields),
                evidence: Some(evidence),
                choice: previous.choice,
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
            FlowPatternKind::Object { fields } => {
                for field in fields {
                    let selected = self.read_property(
                        value.clone(),
                        &field.source_property,
                        field.target.span.clone(),
                        relation,
                    );
                    self.bind_pattern(&field.target, selected, environment, relation);
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
        match &expression.kind {
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
                if !module_binding {
                    return TrackedValue::unknown(format!("unresolved_local_identifier:{name}"));
                }
                let resolution = self.symbol_linker.resolve_binding(file_id, name);
                if resolution == ValueResolution::Missing
                    && self
                        .snapshot
                        .files
                        .iter()
                        .find(|file| file.file_id == file_id)
                        .is_some_and(|file| {
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
                TrackedValue::plain(AbstractValue::Record(values))
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
                    None => TrackedValue::plain(AbstractValue::Union(vec![
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
                    _ => TrackedValue::plain(AbstractValue::Union(vec![
                        self.eval(consequent, environment, file_id),
                        self.eval(alternate, environment, file_id),
                    ])),
                }
            }
            FlowExpressionKind::Call { callee, arguments } => {
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
                        "useState" => {
                            // The initializer only describes the first render. A later render
                            // can observe a setter update, including one scheduled by an effect.
                            // Keep the initial value and an unknown later value so guards in
                            // callbacks cannot silently suppress possible invocations.
                            let initial = arguments.first().map_or_else(
                                || TrackedValue::plain(AbstractValue::Undefined),
                                |argument| self.eval(argument, environment, file_id),
                            );
                            return TrackedValue::plain(AbstractValue::Array(vec![
                                TrackedValue::plain(AbstractValue::Union(vec![
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
                    value: AbstractValue::Closure(ClosureValue {
                        body: body.clone(),
                        params: params.clone(),
                        environment: captured,
                        file_id,
                    }),
                    evidence,
                    choice: self.current_choice.clone(),
                }
            }
            FlowExpressionKind::JsxElement { tag, props } => {
                self.create_element(tag, props, expression, environment, file_id)
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
        }
    }

    fn known_hook_call(&self, file_id: FileId, callee: &FlowExpression) -> Option<&'static str> {
        let file = self
            .snapshot
            .files
            .iter()
            .find(|file| file.file_id == file_id)?;
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
        let file = self
            .snapshot
            .files
            .iter()
            .find(|file| file.file_id == file_id)?;
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
        match callee.value {
            AbstractValue::ModelFunction => self.call_model(&arguments, span),
            AbstractValue::Capability(capability) => {
                let projected_arguments = arguments.iter().map(query_value).collect::<Vec<_>>();
                if self.capabilities.get(capability).is_some_and(|state| {
                    state.invocations.iter().any(|invocation| {
                        self.evidence[invocation.evidence.0 as usize].span == span
                            && invocation
                                .arguments
                                .iter()
                                .map(query_value)
                                .collect::<Vec<_>>()
                                == projected_arguments
                    })
                }) {
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
                TrackedValue::unknown("unknown_call_result")
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
        }
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
                for value in values {
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
            .map(|elements| TrackedValue::plain(AbstractValue::Array(elements)));
        let first = values
            .next()
            .unwrap_or_else(|| TrackedValue::plain(AbstractValue::Array(Vec::new())));
        if let Some(second) = values.next() {
            TrackedValue::plain(AbstractValue::Union(
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
                    .into_iter()
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
                TrackedValue::plain(AbstractValue::Array(mapped))
            }
            AbstractValue::Union(values) => TrackedValue::plain(AbstractValue::Union(
                values
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
                    self.coverage_gaps.push(format!(
                        "array map receiver is unknown ({reason}) at file {} bytes {}..{}",
                        span.file_id.0, span.start, span.end
                    ));
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
                TrackedValue::plain(AbstractValue::Array(vec![mapped]))
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
                for (index, element) in elements.into_iter().enumerate() {
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
                    .map(|elements| TrackedValue::plain(AbstractValue::Array(elements)));
                let first = alternatives
                    .next()
                    .unwrap_or_else(|| TrackedValue::plain(AbstractValue::Array(Vec::new())));
                if let Some(second) = alternatives.next() {
                    TrackedValue::plain(AbstractValue::Union(
                        std::iter::once(first)
                            .chain(std::iter::once(second))
                            .chain(alternatives)
                            .collect(),
                    ))
                } else {
                    first
                }
            }
            AbstractValue::Union(values) => TrackedValue::plain(AbstractValue::Union(
                values
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
        let component = match tag {
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
                        values.extend(fields);
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
        if matches!(&component.value, AbstractValue::Unknown(_))
            && values
                .values()
                .any(|value| !capability_ids(value).is_empty())
            && let FlowJsxTag::Identifier { name, .. } = tag
        {
            self.request_import_for_local(file_id, name);
        }
        TrackedValue::plain(AbstractValue::Element(ElementValue {
            component: Box::new(component),
            props: values,
            span: expression.span.clone(),
        }))
    }

    fn render(&mut self, value: TrackedValue) {
        let element = match value.value {
            AbstractValue::Element(element) => element,
            AbstractValue::Array(values) | AbstractValue::Union(values) => {
                for value in values {
                    self.render(value);
                }
                return;
            }
            _ => return,
        };
        match &element.component.value {
            AbstractValue::Function(key) => {
                let props = element.props.clone();
                let argument = TrackedValue {
                    value: AbstractValue::Record(element.props),
                    evidence: element.component.evidence,
                    choice: element.component.choice.clone(),
                };
                let returned = self.call_function(key, vec![argument]);
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
            }
            AbstractValue::Union(components) => {
                for component in components.clone() {
                    self.render(TrackedValue::plain(AbstractValue::Element(ElementValue {
                        component: Box::new(component),
                        props: element.props.clone(),
                        span: element.span.clone(),
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
        returned
    }

    fn call_model(&mut self, arguments: &[TrackedValue], span: SourceSpan) -> TrackedValue {
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
            factory_arguments: arguments.to_vec(),
            reachability: self.current_reachability,
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
        };
        if let Some(index) = self.model.returned_index {
            let mut elements = vec![TrackedValue::unknown("unselected_return_element"); index + 1];
            elements[index] = capability;
            TrackedValue {
                value: AbstractValue::Array(elements),
                evidence: Some(origin),
                choice: self.current_choice.clone(),
            }
        } else {
            let mut returned = capability;
            for property in self.model.returned_property.iter().rev() {
                returned = TrackedValue {
                    value: AbstractValue::Record(BTreeMap::from([(property.clone(), returned)])),
                    evidence: Some(origin),
                    choice: self.current_choice.clone(),
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
            return TrackedValue::plain(AbstractValue::Union(
                values
                    .into_iter()
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

    fn seed_unreached_creations(&mut self) {
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

        for candidate in candidates {
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
                    .map_or(0, |definition| definition.function.params.len());
                let arguments = (0..parameter_count)
                    .map(|_| TrackedValue::unknown("unreached_function_parameter"))
                    .collect();
                let returned = self.call_function(key, arguments);
                self.render(returned);
            } else if let Some(binding) = self
                .snapshot
                .files
                .iter()
                .find(|file| file.file_id == candidate.file_id)
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
                            if self.snapshot.files.iter().any(|file| {
                                file.file_id == candidate.file_id
                                    && file.flow.exports.iter().any(|export| {
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
                    self.render(returned);
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
            }
            self.record_unreached_gap(&candidate.span);
        }
        self.current_choice = None;
        self.current_reachability = Reachability::Reachable;
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
            .and_then(|key| self.functions.get(key))
            .map(|definition| definition.function.body.clone())
            .unwrap_or_default();
        let globals = self
            .snapshot
            .files
            .iter()
            .find(|file| file.file_id == candidate.file_id)
            .map(|file| file.flow.globals.clone())
            .unwrap_or_default();
        for _ in 0..8 {
            let before = names.len();
            extend_binding_dependencies(&statements, &mut names);
            for binding in &globals {
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
                .snapshot
                .files
                .iter()
                .find(|file| file.file_id == file_id)
                .and_then(|file| {
                    file.flow
                        .imports
                        .iter()
                        .find(|import| import.local == name)
                        .map(|import| (file, import))
                })
                .and_then(|(file, import)| {
                    self.snapshot
                        .resolutions
                        .iter()
                        .find(|resolution| {
                            resolution.importer == file.path
                                && resolution.specifier == import.module
                        })
                        .and_then(|resolution| resolution.resolved_path.as_ref())
                        .map(|path| (path, import.imported.as_str()))
                })
                .and_then(|(path, imported)| {
                    self.snapshot
                        .files
                        .iter()
                        .find(|file| file.path == *path)
                        .map(|file| (file.file_id, file.flow.globals.clone(), imported.to_owned()))
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
            let Some(file) = self.snapshot.files.iter().find(|file| &file.path == path) else {
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
                    self.snapshot.resolutions.iter().any(|resolution| {
                        resolution.importer == file.path
                            && resolution.specifier == import.module
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
            let functions = file.flow.functions.clone();
            let globals = file.flow.globals.clone();
            for function in functions {
                let mut references = ClosureReferences::default();
                for statement in &function.body {
                    collect_statement_references(statement, &mut references);
                }
                if references.names.is_disjoint(&imported_names) {
                    continue;
                }
                if file_len > MAX_REVERSE_IMPORTER_SOURCE_BYTES {
                    self.record_coverage_gap(
                        "reverse importer source exceeds exploration size budget",
                        &function.span,
                    );
                    continue;
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
                let returned = self.call_function(&key, arguments);
                self.render(returned);
                self.reverse_budget_active = false;
            }
            for binding in globals {
                let mut references = ClosureReferences::default();
                collect_expression_references(&binding.value, &mut references);
                if references.names.is_disjoint(&imported_names) {
                    continue;
                }
                let environment = self.module_environment(file_id);
                let returned = self.eval(&binding.value, &environment, file_id);
                self.render(returned);
            }
        }
        if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
            eprintln!("query reverse importer functions seeded: {seeded}");
        }
        self.current_choice = None;
        self.current_reachability = Reachability::Reachable;
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
        let Some(file) = self
            .snapshot
            .files
            .iter()
            .find(|file| file.file_id == candidate.file_id)
        else {
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
        if !self.coverage_gaps.contains(&gap) {
            self.coverage_gaps.push(gap);
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
        let creations = self
            .capabilities
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
                let invocations = capability
                    .invocations
                    .iter()
                    .map(|invocation| QueryInvocation {
                        evidence_id: format!("E{}", invocation.evidence.0),
                        callsite: self.evidence[invocation.evidence.0 as usize].span.clone(),
                        location: self
                            .query_location(&self.evidence[invocation.evidence.0 as usize].span),
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
                    })
                    .collect();
                Some(QueryCreation {
                    creation_id: format!("{snapshot_prefix}-Q{index}"),
                    factory_callsite: capability.callsite.clone(),
                    factory_location: self.query_location(&capability.callsite),
                    reachability: capability.reachability,
                    choice: capability.choice.clone(),
                    factory_arguments,
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
        QueryReport {
            schema_version: 3,
            snapshot_id: self.snapshot.snapshot_id.clone(),
            config_hash: self.snapshot.config_hash.clone(),
            query_hash: query_hash.to_owned(),
            query_id: query.id.clone(),
            kind: query.kind,
            scope: query.scope,
            creations,
            evidence: self.evidence,
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
        let file = self
            .snapshot
            .files
            .iter()
            .find(|file| file.file_id == span.file_id)?;
        let source = fs::read_to_string(&file.path).ok()?;
        let (start_line, start_column) = line_column(&source, span.start);
        let (end_line, end_column) = line_column(&source, span.end);
        Some(QueryLocation {
            path: display_path(&self.project.root, &file.path),
            start_line,
            start_column,
            end_line,
            end_column,
        })
    }
}

fn capability_ids(value: &TrackedValue) -> Vec<usize> {
    let mut ids = Vec::new();
    collect_capability_ids(value, &mut ids);
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn append_array_values(receiver: &AbstractValue, added: &[TrackedValue]) -> Option<AbstractValue> {
    match receiver {
        AbstractValue::Array(elements) => {
            let mut result = elements.clone();
            result.extend_from_slice(added);
            Some(AbstractValue::Array(result))
        }
        AbstractValue::Union(values) => Some(AbstractValue::Union(
            values
                .iter()
                .map(|value| {
                    Some(TrackedValue {
                        value: append_array_values(&value.value, added)?,
                        evidence: value.evidence,
                        choice: value.choice.clone(),
                    })
                })
                .collect::<Option<Vec<_>>>()?,
        )),
        _ => None,
    }
}

fn finite_array_parts(value: &AbstractValue) -> Option<Vec<Vec<TrackedValue>>> {
    match value {
        AbstractValue::Array(elements) => Some(vec![elements.clone()]),
        AbstractValue::Union(values) => {
            let mut parts = Vec::new();
            for value in values {
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
            Some(AbstractValue::Array(fields.values().cloned().collect()))
        }
        AbstractValue::Union(values) => Some(AbstractValue::Union(
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
            for value in values {
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
            for element in elements {
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
        AbstractValue::Unknown(reason) => format!("<{reason}>"),
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
        AbstractValue::Unknown(reason) => QueryValue::Unknown {
            reason: reason.clone(),
        },
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
        | AbstractValue::Intrinsic(_) => QueryValue::Unknown {
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
        AbstractValue::Unknown(_) => None,
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
                binding.value = AbstractValue::Union(narrowed);
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
        AbstractValue::Unknown(_) => None,
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
        AbstractValue::Unknown(_) => true,
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

fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn line_column(source: &str, offset: u32) -> (u32, u32) {
    let mut offset = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(source.len());
    while !source.is_char_boundary(offset) {
        offset = offset.saturating_sub(1);
    }
    let prefix = &source[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix, |(_, current_line)| current_line)
        .chars()
        .count()
        + 1;
    (
        u32::try_from(line).unwrap_or(u32::MAX),
        u32::try_from(column).unwrap_or(u32::MAX),
    )
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
