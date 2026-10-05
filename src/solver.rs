#![allow(clippy::needless_pass_by_value, clippy::too_many_lines)]

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt::Write as _,
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
    project::{ComponentConsumer, ComponentOpener, Project},
    queries::{AuditFinding, AuditReport, Conclusion, Coverage, FindingRef},
    query::{
        QueryBoundaryKind, QueryCallPathKind, QueryCallPathStep, QueryCallsiteInventory,
        QueryCallsiteValues, QueryComponentBoundary, QueryCreation, QueryGap, QueryInvocation,
        QueryLocation, QueryReport, QueryReverseImporter, QueryReverseImporterEvaluation,
        QueryScope, QuerySpec, QueryUnreachedCallsite, QueryUnreachedReason, QueryValue,
        Reachability,
    },
};

mod callback_uses;

const MAX_CALL_DEPTH: usize = 128;
/// Nested calls of one function. Recursion over known data, such as a menu tree, ends sooner;
/// recursion over unknown data would otherwise run to the call depth budget, repeating the same
/// exploration at every level.
const MAX_RECURSION_DEPTH: usize = 8;
const MAX_REVERSE_IMPORTER_EVALUATIONS: usize = 5_000;
const MAX_UNREACHED_RENDER_EVALUATIONS: usize = 5_000;
const MAX_ASSUMED_RENDER_EVALUATIONS: usize = 1_000_000;
const MAX_REVERSE_IMPORTER_SOURCE_BYTES: usize = 20_000;
const MAX_HEAP_ALTERNATIVES: usize = 32;
const MAX_RENDER_VISITS_PER_SITE: usize = 16;
/// Exact renders at one site stop later; paths through shared components multiply quickly, and
/// cutting an exact path loses creations that no assumption recovers.
const MAX_EXACT_RENDER_VISITS_PER_SITE: usize = 64;
const TEXT_FILTER_GAP: &str =
    "directory sources were text-filtered; files without a configured term were not analyzed";

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
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

/// What one heap write replaced, so an unknown branch can be rewound.
struct HeapUndo {
    id: u64,
    value: Option<Rc<AbstractValue>>,
    version: Option<u64>,
}

/// Heap entries one branch wrote, with their values at the end of the branch.
type HeapBranch = BTreeMap<u64, (Option<Rc<AbstractValue>>, Option<u64>)>;

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
    Record(Rc<RecordFields>),
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
        /// The file and local name of the unknown callee, so a root path that renders the result
        /// can request the callee's module.
        callee: Option<Rc<(FileId, String)>>,
        /// Index of the first argument that is a component.
        argument: usize,
    },
}

impl AbstractValue {
    fn open_record(fields: BTreeMap<String, TrackedValue>, reason: &str) -> Self {
        Self::Record(Rc::new(RecordFields {
            fields,
            open: Some(reason.to_owned()),
            unkeyed: Vec::new(),
        }))
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

/// Why a path below a configured root is only possible.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Assumption {
    /// An unmodeled component renders what it is given.
    UnmodeledComponent,
    /// JSX handed to an unmodeled call or unsupported expression is rendered.
    EscapedJsx,
    /// A component whose body is not fully modeled renders JSX it received but did not render
    /// in the model.
    UnrenderedJsx,
}

/// A function or module binding in the use graph.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum UseNode {
    Function(FunctionKey),
    /// An index into the module bindings.
    Global(usize),
}

/// How a use is reached when its containing code runs.
#[derive(Clone, Debug)]
enum UseContext {
    /// Evaluated whenever the containing code is.
    Direct,
    /// Inside a callback passed to the named call.
    CallArgument(String),
    /// Inside a function passed as the named JSX prop.
    PropCallback(String),
    /// Inside a local function.
    LocalCallback,
    /// A dynamic `import()`.
    LazyImport,
}

/// A reference found in the IR, at the call or JSX site that contains it.
struct UseSite<'e> {
    name: Option<&'e str>,
    module: Option<&'e str>,
    site: SourceSpan,
    context: UseContext,
}

/// One use of a node: the code that uses it, the site, and how the site is reached.
struct UseEdge {
    user: UseNode,
    site: SourceSpan,
    context: UseContext,
}

/// An uncalled callback run: closure file and start, reachability, choice, and the factory
/// results it carries.
type UncalledRun = (u32, u32, Reachability, Option<String>, Vec<usize>);

/// A render being remembered: under an assumption only its key, and on an exact path also what
/// it showed its ancestors.
enum RenderMemo {
    Assumed((Option<String>, String, u64), usize),
    Exact {
        key: (Option<String>, String, u64),
        truncations: usize,
        log_start: usize,
        uncertainty: usize,
    },
}

/// What an exact render showed its ancestors: the JSX it rendered and the uncertainty it met.
struct RenderReplay {
    marks: Vec<RenderMark>,
    uncertainty: usize,
}

/// Code exact paths from configured roots reached.
#[derive(Default)]
struct ExactReach {
    functions: BTreeSet<FunctionKey>,
    closures: BTreeMap<FileId, BTreeSet<u32>>,
    /// JSX and call sites evaluated, by file and start offset.
    evaluated: std::collections::HashSet<(u32, u32)>,
    /// Elements rendered.
    rendered: std::collections::HashSet<(u32, u32)>,
    /// Elements cut by the render visit budget.
    cut: std::collections::HashSet<(u32, u32)>,
    /// Components whose renders the budget cut somewhere.
    cut_components: BTreeSet<FunctionKey>,
}

/// Something a render can reach: a JSX element or closure site, or a function component.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum RenderMark {
    Site(u32, u32),
    Function(FunctionKey),
}

/// JSX a component received, checked after its body runs.
struct RenderTracking {
    log_start: usize,
    uncertainty: usize,
    targets: Vec<(RenderMark, TrackedValue)>,
}

#[derive(Clone)]
struct ClosureValue {
    body: FlowArrowBody,
    params: Vec<FlowPattern>,
    environment: Environment,
    file_id: FileId,
    span: SourceSpan,
    /// Whether anything in the model called this closure.
    called: std::cell::Cell<bool>,
}

/// Known properties of a record. A closed record, such as an object literal or JSX props, has
/// exactly these, so reading a missing property gives `undefined`. An open record, such as props
/// spread from an unknown value, may have others.
#[derive(Clone, Default)]
struct RecordFields {
    fields: BTreeMap<String, TrackedValue>,
    /// Why the record may have properties beyond `fields`.
    open: Option<String>,
    /// Values under keys that were not known, such as `[key]: value` with an unknown `key`.
    /// Any property may hold one of them.
    unkeyed: Vec<TrackedValue>,
}

impl std::ops::Deref for RecordFields {
    type Target = BTreeMap<String, TrackedValue>;

    fn deref(&self) -> &Self::Target {
        &self.fields
    }
}

impl std::ops::DerefMut for RecordFields {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.fields
    }
}

impl IntoIterator for RecordFields {
    type Item = (String, TrackedValue);
    type IntoIter = std::collections::btree_map::IntoIter<String, TrackedValue>;

    fn into_iter(self) -> Self::IntoIter {
        self.fields.into_iter()
    }
}

impl<'r> IntoIterator for &'r RecordFields {
    type Item = (&'r String, &'r TrackedValue);
    type IntoIter = std::collections::btree_map::Iter<'r, String, TrackedValue>;

    fn into_iter(self) -> Self::IntoIter {
        self.fields.iter()
    }
}

#[derive(Clone)]
struct ElementValue {
    component: Box<TrackedValue>,
    props: RecordFields,
    span: SourceSpan,
    trace: Vec<TraceStep>,
    /// The JSX tag that created the element, when it names a binding.
    tag: Option<Rc<TagOrigin>>,
}

/// A JSX tag naming a binding: `Name` or `Object.member` in a file.
struct TagOrigin {
    file_id: FileId,
    local: String,
    member: Option<String>,
}

impl TagOrigin {
    fn text(&self) -> String {
        self.member.as_ref().map_or_else(
            || self.local.clone(),
            |member| format!("{}.{member}", self.local),
        )
    }
}

impl BoundaryState {
    fn new(
        kind: QueryBoundaryKind,
        component: String,
        module: Option<String>,
        export: Option<String>,
        reason: String,
    ) -> Self {
        Self {
            kind,
            component,
            module,
            export,
            reason,
            entered_from_reachable: false,
            sites: Vec::new(),
            site_keys: BTreeSet::new(),
            forward_children: false,
            invoke_children: false,
            render_props: BTreeSet::new(),
            component_props: BTreeSet::new(),
            wrapper: None,
        }
    }

    /// A contract that would model the boundary, as TOML for the project definition.
    fn suggested_contract(&self) -> Option<String> {
        let quote =
            |value: &str| format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""));
        if let Some((module, export, argument)) = &self.wrapper {
            return Some(format!(
                "[[component_wrappers]]\nmodule = {}\nexport = {}\ncomponent_argument = {argument}\n",
                quote(module),
                quote(export)
            ));
        }
        if matches!(self.kind, QueryBoundaryKind::EscapedJsx) {
            return None;
        }
        let (Some(module), Some(export)) = (&self.module, &self.export) else {
            return None;
        };
        let list = |names: &BTreeSet<String>| {
            names
                .iter()
                .map(|name| quote(name))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut contract = format!(
            "[[component_consumers]]\nmodule = {}\nexport = {}\n",
            quote(module),
            quote(export)
        );
        if self.forward_children
            || (!self.invoke_children
                && self.render_props.is_empty()
                && self.component_props.is_empty())
        {
            contract.push_str("forward_children = true\n");
        }
        if self.invoke_children {
            contract.push_str("invoke_children = true\n");
        }
        if !self.render_props.is_empty() {
            let _ = writeln!(contract, "render_props = [{}]", list(&self.render_props));
        }
        if !self.component_props.is_empty() {
            let _ = writeln!(
                contract,
                "component_props = [{}]",
                list(&self.component_props)
            );
        }
        Some(contract)
    }
}

/// A component boundary seen on paths from configured roots, aggregated for the report.
struct BoundaryState {
    kind: QueryBoundaryKind,
    component: String,
    module: Option<String>,
    export: Option<String>,
    reason: String,
    entered_from_reachable: bool,
    sites: Vec<SourceSpan>,
    site_keys: BTreeSet<(u32, u32)>,
    forward_children: bool,
    invoke_children: bool,
    render_props: BTreeSet<String>,
    component_props: BTreeSet<String>,
    /// For an unknown higher-order component: the callee's module, export, and argument.
    wrapper: Option<(String, String, usize)>,
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
    /// Boundaries the creation was reached through.
    boundaries: Vec<Rc<str>>,
    /// Recorded invocations by a hash of their site and projected arguments.
    invocation_keys: std::collections::HashMap<u64, usize>,
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
    /// Further paths reaching this invocation with the same projected arguments.
    other_paths: usize,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TraceStep {
    kind: QueryCallPathKind,
    span: SourceSpan,
}

struct LocationSource {
    text: String,
    line_starts: Vec<usize>,
}

thread_local! {
    /// Sources read for locations, shared by every solver pass on this thread. A file's content
    /// hash is part of the key, so a rewritten file is read again.
    static LOCATION_SOURCES: RefCell<std::collections::HashMap<(std::path::PathBuf, String), Rc<LocationSource>>> =
        RefCell::new(std::collections::HashMap::new());
}

impl LocationSource {
    fn shared(path: &Path, content_hash: &str) -> Option<Rc<Self>> {
        let key = (path.to_path_buf(), content_hash.to_owned());
        if let Some(source) = LOCATION_SOURCES.with(|sources| sources.borrow().get(&key).cloned()) {
            return Some(source);
        }
        let source = Rc::new(Self::new(fs::read_to_string(path).ok()?));
        LOCATION_SOURCES.with(|sources| sources.borrow_mut().insert(key, Rc::clone(&source)));
        Some(source)
    }

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
    /// unknown components on the entry corridor. Listed in first-request order, so files met
    /// nearer the entry come first.
    pub root_requested_imports: Vec<std::path::PathBuf>,
    pub producer_paths: BTreeSet<std::path::PathBuf>,
    pub reachable_seed_callsites: Vec<SourceSpan>,
    /// Files whose importers the callsite walk needs, such as a hook whose callers are not
    /// parsed.
    pub importer_requests: BTreeSet<std::path::PathBuf>,
    /// Files the callsite walk needs, such as the module of a component the text filter skipped.
    pub walk_file_requests: BTreeSet<std::path::PathBuf>,
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
    use_chain: &BTreeSet<(FileId, String)>,
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
    if let Some(steps) = query.assumed_render_steps {
        solver.assumed_budget = steps;
    }
    solver.entry_corridor = entry_corridor.keys().cloned().collect();
    solver.use_chain = use_chain
        .iter()
        .map(|(file_id, name)| FunctionKey {
            file_id: *file_id,
            name: name.clone(),
        })
        .collect();
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
        let phase_start = Instant::now();
        solver.unreached_callsites = solver.explain_unreached_callsites();
        if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
            eprintln!(
                "query unreached callsites: {} ms",
                phase_start.elapsed().as_millis()
            );
        }
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
    let mut root_requested_imports = std::mem::take(&mut solver.root_requested_imports);
    root_requested_imports.append(&mut solver.root_requested_possible);
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
    solver.callsite_values = solver.callsite_values(query);
    solver.request_unparsed_argument_modules();
    let importer_requests = std::mem::take(&mut solver.importer_requests);
    let walk_file_requests = std::mem::take(&mut solver.walk_file_requests);
    if std::env::var_os("FOLLOWER_PROFILE_QUERY").is_some() {
        eprintln!(
            "query callsite values: {} ms",
            phase_start.elapsed().as_millis()
        );
    }
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
        importer_requests,
        walk_file_requests,
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
    /// Values that heap writes replaced while an unknown branch is open, newest last.
    heap_journal: Vec<HeapUndo>,
    open_heap_branches: usize,
    next_heap_id: u64,
    evidence: Vec<Evidence>,
    capabilities: Vec<CapabilityState>,
    diagnostics: Vec<String>,
    coverage_gaps: Vec<String>,
    coverage_gap_keys: BTreeSet<String>,
    query_gaps: Vec<(String, String, SourceSpan, Option<String>)>,
    query_gap_keys: BTreeSet<(String, Option<String>)>,
    location_sources: RefCell<BTreeMap<FileId, Rc<LocationSource>>>,
    /// Names each closure body references, by the closure's source span.
    closure_references: RefCell<std::collections::HashMap<SourceSpan, Rc<ClosureReferences>>>,
    locations: RefCell<std::collections::HashMap<SourceSpan, Option<QueryLocation>>>,
    requested_imports: BTreeSet<std::path::PathBuf>,
    root_requested_imports: Vec<std::path::PathBuf>,
    root_requested_paths: BTreeSet<std::path::PathBuf>,
    /// Root requests from possible paths, after `root_requested_imports` in priority.
    root_requested_possible: Vec<std::path::PathBuf>,
    entry_corridor: BTreeSet<std::path::PathBuf>,
    /// Files on the entry corridor plus factory hosts. Empty when the corridor is unknown.
    corridor_files: BTreeSet<FileId>,
    /// Functions found by the backward use walk. Empty when no walk ran.
    use_chain: BTreeSet<FunctionKey>,
    /// Closures by span, and whether their bodies name a function on the use chain.
    closures_toward_factory: std::collections::HashMap<(u32, u32), bool>,
    /// Components already explored under an assumption, keyed by choice, component, and props.
    assumed_component_renders: BTreeSet<(Option<String>, String, u64)>,
    /// What exact paths from configured roots reached, for explaining unreached callsites.
    reach: ExactReach,
    /// Exact renders already explored; an entry is empty while its render is still running.
    exact_renders:
        std::collections::HashMap<(Option<String>, String, u64), Option<Rc<RenderReplay>>>,
    /// Callbacks already scanned, by value, reachability, and choice. The values are kept so
    /// their addresses stay unique.
    scanned_callbacks: std::collections::HashSet<(usize, Reachability, Option<String>)>,
    scanned_callback_values: Vec<Rc<ClosureValue>>,
    /// The returned value of each exact creation, by callsite, choice, and argument values.
    interned_creations: std::collections::HashMap<u64, TrackedValue>,
    /// Factory callsites no exact or possible path reached, with the reason.
    unreached_callsites: Vec<QueryUnreachedCallsite>,
    /// Each factory callsite's values and the calls made with its result.
    callsite_values: Vec<QueryCallsiteValues>,
    /// Files whose importers the callsite walk needs.
    importer_requests: BTreeSet<std::path::PathBuf>,
    /// Files the callsite walk needs.
    walk_file_requests: BTreeSet<std::path::PathBuf>,
    /// The files behind each `unparsed_module:` value, by its reason.
    unparsed_modules: BTreeMap<String, BTreeSet<std::path::PathBuf>>,
    /// Calls of each function in progress.
    active_functions: std::collections::HashMap<FunctionKey, usize>,
    /// Closures carrying a factory result, in creation order, until the render that created
    /// them ends.
    pending_callbacks: Vec<Rc<ClosureValue>>,
    /// Uncalled callbacks already run, by closure site, reachability, choice, and the factory
    /// results they carry.
    uncalled_runs: std::collections::HashSet<UncalledRun>,
    /// Components returned by a configured wrapper, which may add props when rendering them.
    wrapped_components: BTreeSet<FunctionKey>,
    /// Counts budget stops that may have cut a render short.
    render_truncations: usize,
    /// Elements, closures, and function components rendered so far in this pass.
    render_log: Vec<RenderMark>,
    /// Counts operations the model could not follow.
    uncertainty_events: usize,
    /// The most recent of those operations, by event number, for explaining partial models.
    recent_uncertainty: VecDeque<(usize, Rc<str>, SourceSpan)>,
    uncertainty_reasons: std::collections::HashMap<String, Rc<str>>,
    assumed_evaluations: usize,
    assumed_budget: usize,
    assumed_budget_reported: bool,
    capability_producer_files: BTreeSet<FileId>,
    caller_producer_files: BTreeSet<FileId>,
    current_choice: Option<String>,
    current_reachability: Reachability,
    /// Whether exploration runs from a render root or render call the project declares, so what
    /// it reaches is declared rather than reached from an entry.
    declared_render: bool,
    /// During declared renders, the functions that lead to a factory callsite by static uses.
    factory_ancestors: std::collections::HashSet<FunctionKey>,
    assumed_renders: Vec<(SourceSpan, Assumption)>,
    /// Boundary keys for `assumed_renders`, one per entry.
    assumed_boundaries: Vec<Rc<str>>,
    boundaries: BTreeMap<Rc<str>, BoundaryState>,
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
            heap_journal: Vec::new(),
            open_heap_branches: 0,
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
            closure_references: RefCell::new(std::collections::HashMap::new()),
            locations: RefCell::new(std::collections::HashMap::new()),
            requested_imports: BTreeSet::new(),
            root_requested_imports: Vec::new(),
            root_requested_paths: BTreeSet::new(),
            root_requested_possible: Vec::new(),
            entry_corridor: BTreeSet::new(),
            corridor_files: BTreeSet::new(),
            use_chain: BTreeSet::new(),
            closures_toward_factory: std::collections::HashMap::new(),
            assumed_component_renders: BTreeSet::new(),
            reach: ExactReach::default(),
            interned_creations: std::collections::HashMap::new(),
            scanned_callbacks: std::collections::HashSet::new(),
            scanned_callback_values: Vec::new(),
            exact_renders: std::collections::HashMap::new(),
            unreached_callsites: Vec::new(),
            callsite_values: Vec::new(),
            importer_requests: BTreeSet::new(),
            walk_file_requests: BTreeSet::new(),
            unparsed_modules: BTreeMap::new(),
            active_functions: std::collections::HashMap::new(),
            pending_callbacks: Vec::new(),
            uncalled_runs: std::collections::HashSet::new(),
            wrapped_components: BTreeSet::new(),
            render_truncations: 0,
            render_log: Vec::new(),
            uncertainty_events: 0,
            recent_uncertainty: VecDeque::new(),
            uncertainty_reasons: std::collections::HashMap::new(),
            assumed_evaluations: 0,
            assumed_budget: MAX_ASSUMED_RENDER_EVALUATIONS,
            assumed_budget_reported: false,
            capability_producer_files: BTreeSet::new(),
            caller_producer_files: BTreeSet::new(),
            current_choice: None,
            current_reachability: Reachability::Reachable,
            declared_render: false,
            factory_ancestors: std::collections::HashSet::new(),
            assumed_renders: Vec::new(),
            assumed_boundaries: Vec::new(),
            boundaries: BTreeMap::new(),
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
                        value: AbstractValue::open_record(record, "unconfigured_entry_prop"),
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
                let pending = self.pending_callbacks.len();
                let returned = self.call_function(&entry_key, arguments);
                self.render(returned);
                self.run_uncalled_callbacks(pending);
                self.trace.clear();
            }
        }
        self.run_declared_renders()?;
        self.current_choice = None;
        Ok(())
    }

    /// Explores the render roots and render calls the project declares, as possible renders whose
    /// creations are declared rather than reached from an entry.
    fn run_declared_renders(&mut self) -> Result<()> {
        let config = &self.project.config;
        if config.render_roots.is_empty() && config.render_calls.is_empty() {
            return Ok(());
        }
        let mut starts = Vec::new();
        for root in &config.render_roots.clone() {
            let path = self.project.resolve_path(&root.module);
            let file = path
                .canonicalize()
                .ok()
                .and_then(|path| self.symbol_linker.file_at(&path))
                .with_context(|| {
                    format!("render root module was not indexed: {}", path.display())
                })?;
            let resolution = self
                .symbol_linker
                .resolve_exported_value(file.file_id, &root.export);
            if !matches!(resolution, ValueResolution::Resolved(_)) {
                bail!(
                    "render root export {} does not resolve in {}",
                    root.export,
                    root.module.display()
                );
            }
            // The path starts at the declared function or binding.
            let span = match &resolution {
                ValueResolution::Resolved(LinkedValue::Declaration(symbol)) => self
                    .functions
                    .get(&FunctionKey {
                        file_id: symbol.file_id,
                        name: symbol.name.clone(),
                    })
                    .map(|function| function.span.clone())
                    .or_else(|| {
                        self.global_bindings
                            .get(symbol)
                            .map(|&index| self.globals_ir[index].1.span.clone())
                    }),
                _ => None,
            }
            .unwrap_or(SourceSpan {
                file_id: file.file_id,
                start: 0,
                end: 0,
            });
            starts.push((resolution, span));
        }
        let calls = self.declared_render_calls();
        // No path from an entry leads here, so the backward walk's chain may not either; prune
        // by what statically leads to a callsite instead.
        self.factory_ancestors = self.factory_ancestors();
        self.prepare_globals(None);
        self.current_choice = Some("<declared>".to_owned());
        self.current_reachability = Reachability::Possible;
        self.declared_render = true;
        for (resolution, span) in starts {
            self.assumed_evaluations = 0;
            self.assumed_budget_reported = false;
            self.trace = vec![TraceStep {
                kind: QueryCallPathKind::DeclaredRender,
                span: span.clone(),
            }];
            let value = self.linked_value(resolution, &span);
            let pending = self.pending_callbacks.len();
            self.render_declared(&value, &span, 0);
            self.run_uncalled_callbacks(pending);
        }
        for (file_id, argument, span) in calls {
            self.assumed_evaluations = 0;
            self.assumed_budget_reported = false;
            self.trace = vec![TraceStep {
                kind: QueryCallPathKind::DeclaredRender,
                span: span.clone(),
            }];
            // Locals around the call are unknown; module bindings and imports resolve.
            let value = self.eval(&argument, &Environment::new(), file_id);
            let pending = self.pending_callbacks.len();
            self.render_declared(&value, &span, 0);
            self.run_uncalled_callbacks(pending);
        }
        self.trace.clear();
        self.declared_render = false;
        self.current_reachability = Reachability::Reachable;
        Ok(())
    }

    /// The arguments that calls of configured render calls render, in every parsed function and
    /// module binding.
    fn declared_render_calls(&self) -> Vec<(FileId, FlowExpression, SourceSpan)> {
        if self.project.config.render_calls.is_empty() {
            return Vec::new();
        }
        let bodies = self
            .functions
            .iter()
            .map(|(key, function)| (key.file_id, callback_uses::calls_in(&function.body, None)))
            .chain(self.globals_ir.iter().map(|(file_id, binding)| {
                (*file_id, callback_uses::calls_in(&[], Some(&binding.value)))
            }));
        let mut found = Vec::new();
        let mut seen = BTreeSet::new();
        for (file_id, calls) in bodies {
            let Some(file) = self.symbol_linker.file(file_id) else {
                continue;
            };
            for call in calls {
                let FlowExpressionKind::Call { callee, arguments } = &call.kind else {
                    continue;
                };
                let Some(model) = self.project.config.render_call(&file.flow, callee) else {
                    continue;
                };
                // A call inside a function is also inside the module binding or outer function
                // visited first; look at each call once.
                if !seen.insert((file_id, call.span.start, call.span.end)) {
                    continue;
                }
                for &index in &model.arguments {
                    if let Some(argument) = arguments.get(index) {
                        found.push((file_id, argument.clone(), call.span.clone()));
                    }
                }
            }
        }
        found
    }

    /// Renders what a declared render holds: a component renders with unknown props, a function
    /// runs with unknown arguments and what it returns renders, and records and arrays are
    /// searched.
    fn render_declared(&mut self, value: &TrackedValue, span: &SourceSpan, depth: usize) {
        if depth > 3 {
            return;
        }
        match &value.value {
            AbstractValue::Array(values) | AbstractValue::Union(values) => {
                for value in values.iter() {
                    self.render_declared(value, span, depth + 1);
                }
            }
            AbstractValue::Record(fields) => {
                for value in fields.fields.values() {
                    self.render_declared(value, span, depth + 1);
                }
            }
            AbstractValue::Function(_) | AbstractValue::Closure(_)
                if !self.is_component_like(value) =>
            {
                let parameters = match &value.value {
                    AbstractValue::Function(key) => self
                        .functions
                        .get(key)
                        .map_or(0, |function| function.params.len()),
                    AbstractValue::Closure(closure) => closure.params.len(),
                    _ => 0,
                };
                let arguments = (0..parameters.max(1))
                    .map(|_| TrackedValue::unknown("declared_render_argument"))
                    .collect();
                let returned = self.invoke_value(value.clone(), arguments, span.clone());
                self.render_assumed_prop(&returned, span);
            }
            _ => self.render_assumed_prop(value, span),
        }
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
            file_id,
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

    /// What a module-level name in `file_id` refers to.
    fn module_reference(
        &mut self,
        file_id: FileId,
        name: &str,
        resolution: ValueResolution,
        span: &SourceSpan,
    ) -> TrackedValue {
        if resolution == ValueResolution::Unresolved
            && let Some(value) = self.unparsed_module_value(file_id, name, span)
        {
            return value;
        }
        self.linked_value(resolution, span)
    }

    /// An import from a module that is not parsed, tied to the module so that a callsite argument
    /// holding it can request the module.
    fn unparsed_module_value(
        &mut self,
        file_id: FileId,
        local: &str,
        span: &SourceSpan,
    ) -> Option<TrackedValue> {
        let paths = self.symbol_linker.unparsed_link_targets(file_id, local);
        if paths.is_empty() {
            return None;
        }
        let specifier = self
            .symbol_linker
            .file(file_id)?
            .flow
            .imports
            .iter()
            .find(|import| import.local == local)?
            .module
            .clone();
        let reason = format!("{UNPARSED_MODULE}{specifier}");
        self.unparsed_modules
            .entry(reason.clone())
            .or_default()
            .extend(paths);
        self.record_coverage_gap("unresolved value linkage", span);
        Some(TrackedValue::unknown(reason))
    }

    /// Requests the modules behind unknown values that reached a callsite's arguments, so the next
    /// round knows them.
    fn request_unparsed_argument_modules(&mut self) {
        fn collect<'v>(value: &'v QueryValue, reasons: &mut BTreeSet<&'v str>) {
            match value {
                QueryValue::Unknown { reason } if reason.starts_with(UNPARSED_MODULE) => {
                    reasons.insert(reason);
                }
                QueryValue::Array { elements: values } | QueryValue::Alternatives { values } => {
                    for value in values {
                        collect(value, reasons);
                    }
                }
                _ => {}
            }
        }
        let mut reasons = BTreeSet::new();
        for callsite in &self.callsite_values {
            for value in callsite
                .factory_arguments
                .values()
                .chain(callsite.possible_elements.values())
                .flatten()
            {
                collect(value, &mut reasons);
            }
        }
        for reason in reasons {
            if let Some(paths) = self.unparsed_modules.get(reason) {
                self.walk_file_requests.extend(paths.iter().cloned());
            }
        }
    }

    /// What a read with an unknown key gives on a known object: any of its values, or none.
    fn read_any_property(&mut self, object: &TrackedValue) -> Option<TrackedValue> {
        let object = self.materialize(object);
        if is_unparsed_module_value(&object) {
            return Some(object);
        }
        let values = match &object.value {
            AbstractValue::Record(fields)
                if fields.len() + fields.unkeyed.len() <= MAX_ANY_PROPERTY_VALUES =>
            {
                let mut values = fields
                    .values()
                    .chain(&fields.unkeyed)
                    .cloned()
                    .collect::<Vec<_>>();
                values.push(match fields.open.as_deref() {
                    None | Some(COMPUTED_KEY) => TrackedValue::plain(AbstractValue::Undefined),
                    Some(_) => TrackedValue::unknown("unknown_computed_property"),
                });
                values
            }
            AbstractValue::Array(elements) if elements.len() <= MAX_ANY_PROPERTY_VALUES => {
                let mut values = elements.to_vec();
                values.push(TrackedValue::plain(AbstractValue::Undefined));
                values
            }
            AbstractValue::Union(alternatives) => alternatives
                .iter()
                .map(|alternative| self.read_any_property(alternative))
                .collect::<Option<Vec<_>>>()?,
            _ => return None,
        };
        Some(TrackedValue::plain(AbstractValue::union(values)))
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

    /// Requests a file for the root phase. Requests from exact paths come before those from
    /// possible paths, so a limited root budget parses what exact paths need first.
    fn request_root_import(&mut self, path: &std::path::Path) {
        if self.current_reachability == Reachability::Reachable {
            if self.root_requested_paths.insert(path.to_path_buf()) {
                self.root_requested_imports.push(path.to_path_buf());
            } else if let Some(index) = self
                .root_requested_possible
                .iter()
                .position(|requested| requested == path)
            {
                let path = self.root_requested_possible.remove(index);
                self.root_requested_imports.push(path);
            }
        } else if self.root_requested_paths.insert(path.to_path_buf()) {
            self.root_requested_possible.push(path.to_path_buf());
        }
    }

    fn request_import_for_local(&mut self, file_id: FileId, local: &str) {
        for path in self.symbol_linker.unparsed_link_targets(file_id, local) {
            if self.current_reachability != Reachability::Unknown {
                self.request_root_import(&path);
            }
            self.requested_imports.insert(path);
        }
    }

    /// Requests the files an unknown component's linkage needs. Off the entry corridor this only
    /// feeds the general expansion when `general` is set; root paths follow corridor files, which
    /// can lead toward factory hosts, and leave other components to the possible-render
    /// assumption.
    fn request_component_import(&mut self, file_id: FileId, local: &str, general: bool) {
        for path in self.symbol_linker.unparsed_link_targets(file_id, local) {
            let on_corridor = self.entry_corridor.contains(&path);
            if (on_corridor || general) && self.current_reachability != Reachability::Unknown {
                self.request_root_import(&path);
            }
            if general || on_corridor {
                self.requested_imports.insert(path);
            }
        }
    }

    fn request_imported_callee(&mut self, file_id: FileId, callee: &FlowExpression) {
        if let Some(local) = imported_callee_local(callee) {
            self.request_import_for_local(file_id, local);
        }
    }

    /// Counts one operation the model could not follow and remembers it briefly.
    fn note_uncertainty(&mut self, reason: &str, span: &SourceSpan) {
        const RECENT: usize = 4096;
        self.uncertainty_events += 1;
        let reason = if let Some(reason) = self.uncertainty_reasons.get(reason) {
            Rc::clone(reason)
        } else {
            let interned: Rc<str> = Rc::from(reason);
            self.uncertainty_reasons
                .insert(reason.to_owned(), Rc::clone(&interned));
            interned
        };
        if self.recent_uncertainty.len() == RECENT {
            self.recent_uncertainty.pop_front();
        }
        self.recent_uncertainty
            .push_back((self.uncertainty_events, reason, span.clone()));
    }

    /// Up to three distinct operations the model could not follow since event `start`.
    fn uncertainty_since(&self, start: usize) -> Vec<(Rc<str>, SourceSpan)> {
        let mut seen = BTreeSet::new();
        self.recent_uncertainty
            .iter()
            .filter(|(index, ..)| *index > start)
            .filter(|(_, reason, _)| seen.insert(Rc::clone(reason)))
            .take(3)
            .map(|(_, reason, span)| (Rc::clone(reason), span.clone()))
            .collect()
    }

    fn record_coverage_gap(&mut self, reason: &str, span: &SourceSpan) {
        self.note_uncertainty(reason, span);
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

    /// Records a JSX or call site evaluated on an exact path.
    fn note_exact_site(&mut self, span: &SourceSpan) {
        if self.current_reachability == Reachability::Reachable {
            self.reach.evaluated.insert((span.file_id.0, span.start));
        }
    }

    fn call_function(&mut self, key: &FunctionKey, arguments: Vec<TrackedValue>) -> TrackedValue {
        if self.current_reachability == Reachability::Reachable {
            self.reach.functions.insert(key.clone());
        }
        if self.call_depth >= MAX_CALL_DEPTH {
            self.render_truncations += 1;
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
        // A cut here depends only on the calls above it in the same function, so unlike the call
        // depth budget it does not stop enclosing renders from being remembered.
        let active = self.active_functions.entry(key.clone()).or_default();
        if *active >= MAX_RECURSION_DEPTH {
            self.record_coverage_gap("recursion depth budget exhausted", &function.span);
            self.mark_values_unresolved(
                arguments.iter(),
                "recursion depth budget exhausted",
                function.span.clone(),
            );
            return TrackedValue::unknown("recursion_depth_budget_exhausted");
        }
        *active += 1;
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
                key.file_id,
            );
        }
        self.active_captures.push(BTreeSet::new());
        let returned = self.execute_statements(&function.body, &mut environment, key.file_id);
        self.active_captures.pop();
        self.call_depth -= 1;
        if let Some(active) = self.active_functions.get_mut(key) {
            *active -= 1;
        }
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
                        file_id,
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
                            let mark = self.open_heap_branch();
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
                            let left_heap = self.rewind_heap_branch(mark);
                            let right =
                                self.execute_branch(alternate, &mut alternate_environment, file_id);
                            let right_heap = self.rewind_heap_branch(mark);
                            self.join_heap_branches(left_heap, right_heap, span);
                            // A branch that throws renders nothing and does not continue, so the
                            // other branch decides how the code goes on.
                            match (left, right) {
                                (Some(left), Some(right)) if is_thrown(&left) => {
                                    return Some(right);
                                }
                                (Some(left), Some(right)) if is_thrown(&right) => {
                                    return Some(left);
                                }
                                (Some(left), None) if is_thrown(&left) => {
                                    *environment = alternate_environment;
                                }
                                (None, Some(right)) if is_thrown(&right) => {
                                    *environment = consequent_environment;
                                }
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
                FlowStatement::Throw { value, .. } => {
                    self.eval(value, environment, file_id);
                    return Some(TrackedValue::unknown(THROWN_EXCEPTION));
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
        let Some((name, path)) = local_path(object) else {
            return false;
        };
        if !environment.contains_key(&name) {
            return false;
        }
        // Other methods that change an array in place leave its value unknown.
        if ARRAY_MUTATORS.contains(&property.as_str()) && property != "push" {
            for argument in arguments {
                self.eval(argument, environment, file_id);
            }
            self.record_coverage_gap(
                "array changed in place by an unmodeled method",
                &expression.span,
            );
            self.write_path(
                &name,
                &path,
                TrackedValue::unknown("array_changed_in_place"),
                environment,
                expression.span.clone(),
            );
            return true;
        }
        if property != "push" {
            return false;
        }
        if !path.is_empty() {
            // `record.items.push(value)` appends to the array at that property.
            let previous = self.eval(object, environment, file_id);
            let previous = self.materialize(&previous);
            let added = arguments
                .iter()
                .map(|argument| self.eval(argument, environment, file_id))
                .collect::<Vec<_>>();
            let value = if let Some(value) = append_array_values(&previous.value, &added) {
                TrackedValue {
                    value,
                    evidence: previous.evidence,
                    choice: previous.choice.clone(),
                    heap_id: previous.heap_id,
                }
            } else {
                self.mark_values_unresolved(
                    std::iter::once(&previous).chain(added.iter()),
                    "array push receiver is not a finite local array",
                    expression.span.clone(),
                );
                self.record_coverage_gap(
                    "array push receiver is not a finite local array",
                    &expression.span,
                );
                TrackedValue::unknown("array_push_receiver_not_finite")
            };
            self.write_path(&name, &path, value, environment, expression.span.clone());
            return true;
        }
        let name = &name;
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
            self.bump_heap(id, value);
        }
        true
    }

    /// Writes a value at a property path under a local binding, as for `record.a.b = value`,
    /// rebuilding the records on the path and keeping their heap identities current. A path
    /// through something other than known records leaves the binding unresolved.
    fn write_path(
        &mut self,
        name: &str,
        path: &[String],
        value: TrackedValue,
        environment: &mut Environment,
        span: SourceSpan,
    ) {
        let Some(previous) = environment.get(name).cloned() else {
            return;
        };
        let evidence = self.push_evidence(
            RelationKind::Mutation,
            "assign_record_path",
            span.clone(),
            previous
                .evidence
                .into_iter()
                .chain(value.evidence)
                .collect(),
            None,
            &format!("change {name}.{}", path.join(".")),
        );
        if let Some(mut updated) = self.replace_at_path(&previous, path, value) {
            updated.evidence = Some(evidence);
            environment.insert(name.to_owned(), updated);
        } else {
            self.mark_value_unresolved(&previous, "mutation path is not a known record", span);
            environment.insert(
                name.to_owned(),
                TrackedValue {
                    evidence: Some(evidence),
                    ..TrackedValue::unknown("mutation_path_not_a_record")
                },
            );
        }
    }

    fn replace_at_path(
        &mut self,
        current: &TrackedValue,
        path: &[String],
        value: TrackedValue,
    ) -> Option<TrackedValue> {
        let Some((first, rest)) = path.split_first() else {
            return Some(value);
        };
        let current = self.materialize(current);
        // After a branch the value may be one of several records; write into each.
        if let AbstractValue::Union(alternatives) = &current.value {
            let mut written = Vec::with_capacity(alternatives.len());
            for alternative in alternatives.iter() {
                written.push(self.replace_at_path(alternative, path, value.clone())?);
            }
            let updated = AbstractValue::union(written);
            if let Some(id) = current.heap_id {
                self.bump_heap(id, updated.clone());
            }
            return Some(TrackedValue {
                value: updated,
                ..current
            });
        }
        let AbstractValue::Record(mut fields) = current.value.clone() else {
            return None;
        };
        let child = fields
            .get(first)
            .cloned()
            .unwrap_or_else(|| TrackedValue::plain(AbstractValue::Undefined));
        let child = self.replace_at_path(&child, rest, value)?;
        if let Some(id) = child.heap_id {
            self.bump_heap(id, child.value.clone());
        }
        Rc::make_mut(&mut fields).insert(first.clone(), child);
        let updated = AbstractValue::Record(fields);
        if let Some(id) = current.heap_id {
            self.bump_heap(id, updated.clone());
        }
        Some(TrackedValue {
            value: updated,
            ..current
        })
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
        if matches!(previous.value, AbstractValue::Union(_)) {
            self.write_path(name, &[property.to_owned()], value, environment, span);
            return;
        }
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
            self.bump_heap(id, updated.clone());
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
        file_id: FileId,
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
                    self.bind_pattern(&field.target, selected, environment, relation, file_id);
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
                    self.bind_pattern(rest, remainder, environment, relation, file_id);
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
                        self.bind_pattern(target, selected, environment, relation, file_id);
                    }
                }
            }
            FlowPatternKind::Default { target, default } => {
                // The default applies only when the value is `undefined`.
                let value = match undefined_alternatives(&value) {
                    Some(Undefinedness::Never) => value,
                    Some(Undefinedness::Always) => self.eval(default, environment, file_id),
                    _ => {
                        let default = self.eval(default, environment, file_id);
                        let mut alternatives = Vec::new();
                        collect_defined_alternatives(&value, &mut alternatives);
                        alternatives.push(default);
                        TrackedValue {
                            evidence: value.evidence,
                            ..TrackedValue::plain(AbstractValue::union(alternatives))
                        }
                    }
                };
                self.bind_pattern(target, value, environment, relation, file_id);
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
                self.module_reference(file_id, name, resolution, &expression.span)
            }
            FlowExpressionKind::Record { fields } => {
                let mut record = RecordFields::default();
                for field in fields {
                    let key = field.computed.as_ref().map(|key| {
                        let key = self.eval(key, environment, file_id);
                        property_key(&self.materialize(&key).value)
                    });
                    let value = self.eval(&field.value, environment, file_id);
                    if field.spread {
                        let value = self.materialize(&value);
                        spread_into_record(&mut record, &value);
                    } else {
                        match key {
                            Some(Some(key)) => {
                                record.insert(key, value);
                            }
                            Some(None) => add_unkeyed_value(&mut record, value),
                            None => {
                                record.insert(field.property.clone(), value);
                            }
                        }
                    }
                }
                TrackedValue::plain(AbstractValue::Record(Rc::new(record)))
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
                let property = self.materialize(&property);
                if let Some(keys) = property_keys(&property.value) {
                    let mut values = keys
                        .iter()
                        .map(|key| {
                            self.read_property(
                                object.clone(),
                                key,
                                expression.span.clone(),
                                RelationKind::KeySelection,
                            )
                        })
                        .collect::<Vec<_>>();
                    return if values.len() == 1 {
                        values.pop().expect("one value")
                    } else {
                        TrackedValue::plain(AbstractValue::union(values))
                    };
                }
                // An unknown key reads any of a known object's values.
                if let Some(value) = self.read_any_property(&object) {
                    return value;
                }
                self.mark_value_unresolved(
                    &object,
                    "computed property is not a finite string",
                    expression.span.clone(),
                );
                TrackedValue::unknown("unknown_computed_property")
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
                    None => {
                        // `a && b` yields `a` only when `a` is falsy and `a || b` only when it is
                        // truthy, so that side keeps its truthiness even when its value is not
                        // known: `unknown && false` is falsy.
                        let left = match operator {
                            FlowLogicalOperator::And => TrackedValue {
                                value: AbstractValue::Unknown(FALSY_OPERAND.to_owned()),
                                ..left
                            },
                            FlowLogicalOperator::Or => TrackedValue {
                                value: AbstractValue::Unknown(TRUTHY_OPERAND.to_owned()),
                                ..left
                            },
                            FlowLogicalOperator::Coalesce => left,
                        };
                        TrackedValue::plain(AbstractValue::union(vec![
                            left,
                            self.eval(right, environment, file_id),
                        ]))
                    }
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
                self.note_exact_site(&expression.span);
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
                    // The wrapper may pass props of its own to the component.
                    if let AbstractValue::Function(key) = &component.value {
                        self.wrapped_components.insert(key.clone());
                    }
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
                // A configured factory's members are configured components, as `Stack.Screen` for
                // `const Stack = createStack()`.
                let members = self
                    .symbol_linker
                    .file(file_id)
                    .map_or_else(Vec::new, |file| {
                        self.project
                            .config
                            .component_factory_members(&file.flow, callee)
                    });
                if !members.is_empty() {
                    let fields = members
                        .into_iter()
                        .filter_map(|model| {
                            Some((
                                model.member.clone()?,
                                TrackedValue::plain(AbstractValue::ConfiguredComponent(
                                    model.clone(),
                                )),
                            ))
                        })
                        .collect();
                    return TrackedValue::plain(AbstractValue::open_record(
                        fields,
                        "configured_component_factory",
                    ));
                }
                if let Some(file) = self.symbol_linker.file(file_id)
                    && let Some(property) = self
                        .project
                        .config
                        .lazy_factory_property(&file.flow, callee)
                    && let Some(module) = lazy_component_import(&file.flow, expression, property)
                {
                    return self
                        .module_export_value(file_id, module, "default", &expression.span)
                        .unwrap_or_else(|| TrackedValue::unknown("unresolved_lazy_component"));
                }
                if let Some(opener) = self
                    .symbol_linker
                    .file(file_id)
                    .and_then(|file| self.project.config.component_opener(&file.flow, callee))
                    .cloned()
                {
                    return self.open_component(
                        &opener,
                        expression,
                        arguments,
                        environment,
                        file_id,
                    );
                }
                // `Object.freeze(value)` gives the value itself.
                if let FlowExpressionKind::StaticMember { object, property } = &callee.kind
                    && property == "freeze"
                    && matches!(&object.kind, FlowExpressionKind::Identifier { name, .. } if name == "Object")
                    && !environment.contains_key("Object")
                    && let Some(value) = arguments.first()
                {
                    return self.eval(value, environment, file_id);
                }
                // `Promise.resolve(value)` is the value, as `await` gives it.
                if let FlowExpressionKind::StaticMember { object, property } = &callee.kind
                    && property == "resolve"
                    && matches!(&object.kind, FlowExpressionKind::Identifier { name, .. } if name == "Promise")
                    && !environment.contains_key("Promise")
                {
                    return arguments.first().map_or_else(
                        || TrackedValue::plain(AbstractValue::Undefined),
                        |value| self.eval(value, environment, file_id),
                    );
                }
                // `import('./Panel').then((module) => module.Panel)` calls the callback with the
                // module, as `await` gives it.
                if let FlowExpressionKind::StaticMember { object, property } = &callee.kind
                    && property == "then"
                    && matches!(object.kind, FlowExpressionKind::DynamicImport { .. })
                    && let Some(callback) = arguments.first()
                {
                    let module = self.eval(object, environment, file_id);
                    let callback = self.eval(callback, environment, file_id);
                    return self.invoke_value(callback, vec![module], expression.span.clone());
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
                if let Some(api) = self.react_api_call(file_id, callee, environment) {
                    return self.eval_react_api(
                        api,
                        arguments,
                        environment,
                        file_id,
                        &expression.span,
                    );
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
                                        member: None,
                                        curried: false,
                                    },
                                ))
                            };
                            return TrackedValue::plain(AbstractValue::open_record(
                                BTreeMap::from([
                                    ("Provider".to_owned(), component("Provider", true, false)),
                                    ("Consumer".to_owned(), component("Consumer", false, true)),
                                ]),
                                "unmodeled_context_property",
                            ));
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
                // Membership in a known array, or a set built from one.
                if let FlowExpressionKind::StaticMember { object, property } = &callee.kind
                    && matches!(property.as_str(), "includes" | "has")
                    && let [needle] = arguments.as_slice()
                    && is_plain_read(object)
                {
                    let receiver = self.eval(object, environment, file_id);
                    if matches!(
                        receiver.value,
                        AbstractValue::Array(_) | AbstractValue::Union(_)
                    ) {
                        let needle = self.eval(needle, environment, file_id);
                        if let Some(found) = array_membership(&receiver, &needle) {
                            return TrackedValue::plain(AbstractValue::Boolean(found));
                        }
                    }
                }
                let callee_value = self.eval(callee, environment, file_id);
                let arguments = arguments
                    .iter()
                    .map(|argument| self.eval(argument, environment, file_id))
                    .collect::<Vec<_>>();
                if matches!(&callee_value.value, AbstractValue::Unknown(_))
                    && arguments.iter().any(contains_capability)
                {
                    self.request_imported_callee(file_id, callee);
                }
                let mut result =
                    self.invoke_value(callee_value, arguments, expression.span.clone());
                if let AbstractValue::AssumedWrapper { callee: source, .. } = &mut result.value
                    && let Some(local) = imported_callee_local(callee)
                {
                    *source = Some(Rc::new((file_id, local.to_owned())));
                }
                result
            }
            FlowExpressionKind::Arrow { params, body } => {
                let body_references = self.body_references(&expression.span, body);
                let references = if params.iter().any(|param| {
                    matches!(
                        param.kind,
                        FlowPatternKind::Default { .. }
                            | FlowPatternKind::Object { .. }
                            | FlowPatternKind::Array { .. }
                    )
                }) {
                    let mut references = (*body_references).clone();
                    for param in params {
                        collect_pattern_references(param, &mut references);
                    }
                    Rc::new(references)
                } else {
                    body_references
                };
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
                for name in references.modules.iter().cloned() {
                    if let std::collections::btree_map::Entry::Vacant(entry) = captured.entry(name)
                    {
                        let resolution = self.symbol_linker.resolve_binding(file_id, entry.key());
                        let value = self.module_reference(
                            file_id,
                            entry.key(),
                            resolution,
                            &expression.span,
                        );
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
                let closure = Rc::new(ClosureValue {
                    body: body.clone(),
                    params: params.clone(),
                    environment: captured,
                    file_id,
                    span: expression.span.clone(),
                    called: std::cell::Cell::new(false),
                });
                let value = TrackedValue {
                    value: AbstractValue::Closure(Rc::clone(&closure)),
                    evidence,
                    choice: self.current_choice.clone(),
                    heap_id: None,
                };
                if self.model.scan_callback_bodies
                    && (contains_capability(&value)
                        || self.closure_leads_toward_factory(file_id, &expression.span, body))
                {
                    self.pending_callbacks.push(closure);
                }
                value
            }
            FlowExpressionKind::JsxElement { tag, props } => {
                self.note_exact_site(&expression.span);
                self.create_element(tag, props, expression, environment, file_id)
            }
            // An awaited value is the value itself, so `import()` gives the module's namespace.
            FlowExpressionKind::DynamicImport { module } => self
                .module_namespace(file_id, module, &expression.span)
                .unwrap_or_else(|| TrackedValue::unknown("unmodeled_dynamic_import")),
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
                self.render_escaped_jsx(
                    references.iter().filter_map(|name| environment.get(name)),
                    &expression.span,
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
            self.write_heap(id, Rc::new(value.value.clone()), 0);
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

    fn write_heap(&mut self, id: u64, value: Rc<AbstractValue>, version: u64) {
        let value = self.heap.insert(id, value);
        let version = self.heap_versions.insert(id, version);
        if self.open_heap_branches > 0 {
            self.heap_journal.push(HeapUndo { id, value, version });
        }
    }

    fn bump_heap(&mut self, id: u64, value: AbstractValue) {
        let version = self.heap_versions.get(&id).map_or(1, |version| version + 1);
        self.write_heap(id, Rc::new(value), version);
    }

    fn clear_heap(&mut self) {
        debug_assert_eq!(self.open_heap_branches, 0);
        self.heap.clear();
        self.heap_versions.clear();
        self.heap_journal.clear();
    }

    fn open_heap_branch(&mut self) -> usize {
        self.open_heap_branches += 1;
        self.heap_journal.len()
    }

    /// Returns the entries one branch changed and restores the heap to where the branch began.
    fn rewind_heap_branch(&mut self, mark: usize) -> HeapBranch {
        let mut changed = BTreeMap::new();
        for undo in &self.heap_journal[mark..] {
            changed.entry(undo.id).or_insert_with(|| {
                (
                    self.heap.get(&undo.id).cloned(),
                    self.heap_versions.get(&undo.id).copied(),
                )
            });
        }
        while self.heap_journal.len() > mark {
            let undo = self.heap_journal.pop().expect("journal entry");
            match undo.value {
                Some(value) => self.heap.insert(undo.id, value),
                None => self.heap.remove(&undo.id),
            };
            match undo.version {
                Some(version) => self.heap_versions.insert(undo.id, version),
                None => self.heap_versions.remove(&undo.id),
            };
        }
        changed
    }

    /// Joins two branches that started from the current heap. Only entries a branch wrote can
    /// differ from the start, so the join visits those alone.
    fn join_heap_branches(&mut self, left: HeapBranch, right: HeapBranch, span: &SourceSpan) {
        self.open_heap_branches -= 1;
        let ids = left
            .keys()
            .chain(right.keys())
            .copied()
            .collect::<BTreeSet<_>>();
        for id in ids {
            let initial = self.heap.get(&id).cloned();
            let baseline = self.heap_versions.get(&id).copied().unwrap_or(0);
            let side = |branch: &HeapBranch| match branch.get(&id) {
                Some((value, version)) => (value.clone(), version.unwrap_or(baseline)),
                None => (initial.clone(), baseline),
            };
            let (left_value, left_version) = side(&left);
            let (right_value, right_version) = side(&right);
            let joined = match (&left_value, &right_value) {
                (Some(left), Some(right))
                    if left_version != baseline || right_version != baseline =>
                {
                    let mut alternatives = Vec::new();
                    collect_heap_alternatives(left, &mut alternatives);
                    collect_heap_alternatives(right, &mut alternatives);
                    Rc::new(if alternatives.len() > MAX_HEAP_ALTERNATIVES {
                        self.record_coverage_gap("heap alternative budget exhausted", span);
                        AbstractValue::Unknown("heap_alternative_budget_exhausted".to_owned())
                    } else {
                        AbstractValue::union(alternatives)
                    })
                }
                (Some(value), _) | (_, Some(value)) => value.clone(),
                (None, None) => continue,
            };
            self.write_heap(
                id,
                joined,
                left_version.max(right_version)
                    + u64::from(left_version != baseline || right_version != baseline),
            );
        }
    }

    /// Whether `local` in `file_id` is imported from one of `modules`, and under which name.
    fn imported_from(&self, file_id: FileId, local: &str, modules: &[&str]) -> Option<String> {
        self.symbol_linker
            .file(file_id)?
            .flow
            .imports
            .iter()
            .find_map(|import| {
                (import.local == local
                    && !import.type_only
                    && modules.contains(&import.module.as_str()))
                .then(|| import.imported.clone())
            })
    }

    /// Recognizes React element and children APIs, `createPortal`, and `Object.assign` calls.
    fn react_api_call(
        &self,
        file_id: FileId,
        callee: &FlowExpression,
        environment: &Environment,
    ) -> Option<&'static str> {
        let element_api = |name: &str| match name {
            "createElement" => Some("createElement"),
            "cloneElement" => Some("cloneElement"),
            "isValidElement" => Some("isValidElement"),
            _ => None,
        };
        let children_api = |name: &str| match name {
            "only" => Some("Children.only"),
            "toArray" => Some("Children.toArray"),
            "map" => Some("Children.map"),
            "forEach" => Some("Children.forEach"),
            "count" => Some("Children.count"),
            _ => None,
        };
        let namespace = |name: &str, module: &[&str]| {
            self.imported_from(file_id, name, module)
                .is_some_and(|imported| imported == "*" || imported == "default")
        };
        match &callee.kind {
            FlowExpressionKind::Identifier { name, .. } => {
                if let Some(imported) = self.imported_from(file_id, name, &["react"]) {
                    return element_api(&imported);
                }
                if let Some(imported) = self.imported_from(
                    file_id,
                    name,
                    &["react/jsx-runtime", "react/jsx-dev-runtime"],
                ) {
                    return matches!(imported.as_str(), "jsx" | "jsxs" | "jsxDEV").then_some("jsx");
                }
                (self.imported_from(file_id, name, &["react-dom"]).as_deref()
                    == Some("createPortal"))
                .then_some("createPortal")
            }
            FlowExpressionKind::StaticMember { object, property } => match &object.kind {
                FlowExpressionKind::Identifier { name, .. } => {
                    if namespace(name, &["react"]) {
                        return element_api(property);
                    }
                    if self.imported_from(file_id, name, &["react"]).as_deref() == Some("Children")
                    {
                        return children_api(property);
                    }
                    if namespace(name, &["react-dom"]) && property == "createPortal" {
                        return Some("createPortal");
                    }
                    (name == "Object"
                        && property == "assign"
                        && !environment.contains_key("Object")
                        && self.symbol_linker.file(file_id).is_some_and(|file| {
                            file.flow
                                .imports
                                .iter()
                                .all(|import| import.local != "Object")
                        })
                        && self
                            .symbol_linker
                            .resolve_local_declaration(file_id, "Object")
                            .is_none())
                    .then_some("Object.assign")
                }
                FlowExpressionKind::StaticMember {
                    object: inner,
                    property: children,
                } if children == "Children" => match &inner.kind {
                    FlowExpressionKind::Identifier { name, .. } if namespace(name, &["react"]) => {
                        children_api(property)
                    }
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        }
    }

    /// Evaluates a recognized React API or `Object.assign` call.
    fn eval_react_api(
        &mut self,
        api: &str,
        arguments: &[FlowExpression],
        environment: &Environment,
        file_id: FileId,
        span: &SourceSpan,
    ) -> TrackedValue {
        let values = arguments
            .iter()
            .map(|argument| self.eval(argument, environment, file_id))
            .collect::<Vec<_>>();
        let argument = |index: usize| {
            values
                .get(index)
                .cloned()
                .unwrap_or_else(|| TrackedValue::plain(AbstractValue::Undefined))
        };
        match api {
            "createElement" | "jsx" => {
                let component = argument(0);
                let component = match &component.value {
                    AbstractValue::String(name) => {
                        TrackedValue::plain(AbstractValue::Intrinsic(name.clone()))
                    }
                    _ => component,
                };
                let mut props = match self.materialize(&argument(1)).value {
                    AbstractValue::Record(fields) => Rc::unwrap_or_clone(fields),
                    AbstractValue::Null | AbstractValue::Undefined => RecordFields::default(),
                    _ => RecordFields {
                        fields: BTreeMap::new(),
                        open: Some("create_element_props_unknown".to_owned()),
                        unkeyed: Vec::new(),
                    },
                };
                if api == "createElement" {
                    match &values.get(2..).unwrap_or_default() {
                        [] => {}
                        [child] => {
                            props.insert("children".to_owned(), child.clone());
                        }
                        children => {
                            props.insert(
                                "children".to_owned(),
                                TrackedValue::plain(AbstractValue::array(children.to_vec())),
                            );
                        }
                    }
                }
                TrackedValue::plain(AbstractValue::element(ElementValue {
                    component: Box::new(component),
                    props,
                    span: span.clone(),
                    trace: self.trace.clone(),
                    tag: None,
                }))
            }
            "cloneElement" => {
                let AbstractValue::Element(element) = argument(0).value else {
                    self.render_escaped_jsx(values.iter(), span);
                    return TrackedValue::unknown("clone_of_unknown_element");
                };
                let mut element = Rc::unwrap_or_clone(element);
                match self.materialize(&argument(1)).value {
                    AbstractValue::Record(fields) => {
                        element.props.open = element.props.open.take().or(fields.open.clone());
                        element.props.extend(
                            fields
                                .iter()
                                .map(|(name, value)| (name.clone(), value.clone())),
                        );
                    }
                    AbstractValue::Null | AbstractValue::Undefined => {}
                    _ => element.props.open = Some("clone_element_props_unknown".to_owned()),
                }
                match values.get(2..).unwrap_or_default() {
                    [] => {}
                    [child] => {
                        element.props.insert("children".to_owned(), child.clone());
                    }
                    children => {
                        element.props.insert(
                            "children".to_owned(),
                            TrackedValue::plain(AbstractValue::array(children.to_vec())),
                        );
                    }
                }
                element.span = span.clone();
                TrackedValue::plain(AbstractValue::element(element))
            }
            "isValidElement" => match argument(0).value {
                AbstractValue::Element(_) => TrackedValue::plain(AbstractValue::Boolean(true)),
                AbstractValue::Null
                | AbstractValue::Undefined
                | AbstractValue::String(_)
                | AbstractValue::Number(_)
                | AbstractValue::Boolean(_) => TrackedValue::plain(AbstractValue::Boolean(false)),
                _ => TrackedValue::unknown("is_valid_element_unknown"),
            },
            // A portal renders its children elsewhere in the document.
            "createPortal" | "Children.only" => argument(0),
            "Children.toArray" => {
                let mut children = Vec::new();
                flatten_children(&argument(0), &mut children);
                TrackedValue::plain(AbstractValue::array(children))
            }
            "Children.count" => {
                let mut children = Vec::new();
                if flatten_children(&argument(0), &mut children) {
                    TrackedValue::plain(AbstractValue::Number(
                        i64::try_from(children.len()).unwrap_or(i64::MAX),
                    ))
                } else {
                    TrackedValue::unknown("children_count_unknown")
                }
            }
            "Children.map" | "Children.forEach" => {
                let mut children = Vec::new();
                flatten_children(&argument(0), &mut children);
                let callback = argument(1);
                let results = children
                    .into_iter()
                    .enumerate()
                    .map(|(index, child)| {
                        self.invoke_value(
                            callback.clone(),
                            vec![
                                child,
                                TrackedValue::plain(AbstractValue::Number(
                                    i64::try_from(index).unwrap_or(i64::MAX),
                                )),
                            ],
                            span.clone(),
                        )
                    })
                    .collect::<Vec<_>>();
                if api == "Children.map" {
                    TrackedValue::plain(AbstractValue::array(results))
                } else {
                    TrackedValue::plain(AbstractValue::Undefined)
                }
            }
            "Object.assign" => {
                let target = self.materialize(&argument(0));
                match target.value {
                    AbstractValue::Record(fields) => {
                        let mut record = Rc::unwrap_or_clone(fields);
                        for source in values.iter().skip(1) {
                            let source = self.materialize(source);
                            spread_into_record(&mut record, &source);
                        }
                        let merged = AbstractValue::Record(Rc::new(record));
                        if let Some(id) = target.heap_id {
                            self.bump_heap(id, merged.clone());
                        }
                        TrackedValue {
                            value: merged,
                            ..target
                        }
                    }
                    // Statics assigned onto a component do not change how it renders.
                    AbstractValue::Function(_) | AbstractValue::Closure(_) => target,
                    _ => {
                        self.mark_values_unresolved(
                            values.iter(),
                            "Object.assign target is not a known record",
                            span.clone(),
                        );
                        TrackedValue::unknown("object_assign_unknown_target")
                    }
                }
            }
            _ => TrackedValue::unknown("unmodeled_react_api"),
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
                let key = {
                    use std::hash::{Hash, Hasher};
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    (&span, &projected_arguments).hash(&mut hasher);
                    hasher.finish()
                };
                // Another path to the same invocation with the same values adds nothing new
                // beyond its count; the first path stays as the example.
                if let Some(state) = self.capabilities.get_mut(capability)
                    && let Some(&index) = state.invocation_keys.get(&key)
                {
                    state.invocations[index].other_paths += 1;
                    self.trace.truncate(trace_len);
                    return TrackedValue::plain(AbstractValue::Undefined);
                }
                let call_path = self.trace_at(QueryCallPathKind::Invocation, &span);
                let evidence = self.push_evidence(
                    RelationKind::Invocation,
                    "invoke_capability",
                    span.clone(),
                    callee.evidence.into_iter().collect(),
                    Some(self.model.id.clone()),
                    "matching callback capability is invoked",
                );
                if let Some(state) = self.capabilities.get_mut(capability) {
                    state.invocation_keys.insert(key, state.invocations.len());
                    state.invocations.push(InvocationState {
                        evidence,
                        arguments: arguments.clone(),
                        call_path,
                        other_paths: 0,
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
                self.render_escaped_jsx(arguments.iter(), &span);
                if self.model.scan_callback_bodies {
                    for argument in &arguments {
                        self.scan_callback_bodies(argument, &span, 0);
                    }
                }
                let argument = arguments
                    .iter()
                    .position(|argument| self.is_component_like(argument));
                let wrapped = arguments
                    .iter()
                    .filter(|argument| self.is_component_like(argument))
                    .cloned()
                    .collect::<Vec<_>>();
                match argument {
                    Some(argument) => TrackedValue::plain(AbstractValue::AssumedWrapper {
                        reason: "unknown_call_result".to_owned(),
                        wrapped: Rc::new(wrapped),
                        callee: None,
                        argument,
                    }),
                    // What a function from a module that is not parsed returns stays tied to
                    // the module.
                    None if reason.starts_with(UNPARSED_MODULE) => TrackedValue::unknown(reason),
                    None => TrackedValue::unknown("unknown_call_result"),
                }
            }
            _ => {
                self.mark_values_unresolved(
                    std::iter::once(&callee).chain(arguments.iter()),
                    "value passed to unsupported call target",
                    span.clone(),
                );
                self.render_escaped_jsx(arguments.iter(), &span);
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

    /// Runs the closures created since `start` that carry a factory result and that nothing in
    /// the model called, such as event handlers in props, with unknown
    /// arguments. Whether they run at all is not known, so an exact path continues as possible.
    fn run_uncalled_callbacks(&mut self, start: usize) {
        if !self.model.scan_callback_bodies {
            self.pending_callbacks.truncate(start);
            return;
        }
        let reachability = self.current_reachability;
        if reachability == Reachability::Reachable {
            self.current_reachability = Reachability::Possible;
        }
        let mut index = start;
        while index < self.pending_callbacks.len() {
            let closure = Rc::clone(&self.pending_callbacks[index]);
            index += 1;
            if closure.called.get() {
                continue;
            }
            let value = TrackedValue::plain(AbstractValue::Closure(Rc::clone(&closure)));
            let key = (
                closure.span.file_id.0,
                closure.span.start,
                self.current_reachability,
                self.current_choice.clone(),
                self.value_capability_ids(&value),
            );
            if !self.uncalled_runs.insert(key) {
                continue;
            }
            let trace_len = self.trace.len();
            self.trace.push(TraceStep {
                kind: QueryCallPathKind::UncalledCallback,
                span: closure.span.clone(),
            });
            let arguments = closure
                .params
                .iter()
                .map(|_| TrackedValue::unknown("uncalled_callback_argument"))
                .collect();
            let returned = self.call_closure(&closure, arguments);
            // A render callback may hand the factory result to the JSX it returns.
            if contains_capability(&returned) {
                self.render(returned);
            }
            self.trace.truncate(trace_len);
        }
        self.pending_callbacks.truncate(start);
        self.current_reachability = reachability;
    }

    fn scan_callback_bodies(&mut self, value: &TrackedValue, span: &SourceSpan, depth: usize) {
        if depth >= 16 || !contains_capability(value) {
            return;
        }
        match &value.value {
            AbstractValue::Closure(closure) => {
                self.record_coverage_gap("callback body explored through opaque consumer", span);
                // The same callback handed down through many components is scanned once.
                let key = (
                    Rc::as_ptr(closure).addr(),
                    self.current_reachability,
                    self.current_choice.clone(),
                );
                if !self.scanned_callbacks.insert(key) {
                    return;
                }
                self.scanned_callback_values.push(Rc::clone(closure));
                // The consumer's arguments are not known; none of them is a missing argument.
                let arguments = closure
                    .params
                    .iter()
                    .map(|_| TrackedValue::unknown("scanned_callback_argument"))
                    .collect();
                let returned = self.call_closure(closure, arguments);
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
                        .cloned()
                })
                .or_else(|| {
                    let member = match tag {
                        FlowJsxTag::Member { property, .. } => Some(property.as_str()),
                        _ => None,
                    };
                    self.react_builtin_component(file_id, name, member)
                        .or_else(|| self.linked_component_consumer(file_id, name, member))
                }),
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
        let mut values = RecordFields::default();
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
                    let spread = self.materialize(&spread);
                    if let AbstractValue::Record(fields) = spread.value {
                        let fields = Rc::unwrap_or_clone(fields);
                        values.open = values.open.take().or(fields.open);
                        values.extend(fields.fields);
                    } else if !matches!(
                        spread.value,
                        AbstractValue::Null | AbstractValue::Undefined
                    ) {
                        values.open = Some("jsx_spread_of_unknown_value".to_owned());
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
            let renders_content = values
                .get("children")
                .is_some_and(|children| !is_primitive(&children.value))
                || values.contains_key("render")
                || values.contains_key("component");
            if let Some(local) = local {
                if matches!(tag, FlowJsxTag::Identifier { .. })
                    && values.values().any(contains_capability)
                {
                    self.request_import_for_local(file_id, local);
                } else if renders_content || self.current_reachability != Reachability::Unknown {
                    self.request_component_import(file_id, local, renders_content);
                }
            }
        }
        let tag = match tag {
            FlowJsxTag::Identifier {
                name,
                intrinsic: false,
                ..
            } => Some(Rc::new(TagOrigin {
                file_id,
                local: name.clone(),
                member: None,
            })),
            FlowJsxTag::Member {
                object, property, ..
            } => Some(Rc::new(TagOrigin {
                file_id,
                local: object.clone(),
                member: Some(property.clone()),
            })),
            _ => None,
        };
        TrackedValue::plain(AbstractValue::element(ElementValue {
            component: Box::new(component),
            props: values,
            span: expression.span.clone(),
            trace: self.trace.clone(),
            tag,
        }))
    }

    /// Applies a component's `defaultProps` as React does: a default replaces a prop that is
    /// missing or `undefined`. Defaults that are not a known record leave the props open.
    fn apply_default_props(&mut self, key: &FunctionKey, props: &mut RecordFields) {
        let Some(declaration) = self.symbol_linker.file(key.file_id).and_then(|file| {
            file.flow
                .default_props
                .iter()
                .find(|declaration| declaration.component == key.name)
        }) else {
            return;
        };
        let environment = self.module_environment(key.file_id);
        let defaults = self.eval(&declaration.value, &environment, key.file_id);
        let AbstractValue::Record(defaults) = self.materialize(&defaults).value else {
            props.open = Some("unmodeled_default_props".to_owned());
            return;
        };
        for (name, default) in defaults.iter() {
            let value = match props.get(name).map(undefined_alternatives) {
                None | Some(Some(Undefinedness::Always)) => default.clone(),
                Some(Some(Undefinedness::Never)) => continue,
                Some(None) => {
                    let mut alternatives = Vec::new();
                    collect_defined_alternatives(&props[name], &mut alternatives);
                    alternatives.push(default.clone());
                    TrackedValue::plain(AbstractValue::union(alternatives))
                }
            };
            props.insert(name.clone(), value);
        }
        if props.open.is_none() {
            props.open.clone_from(&defaults.open);
        }
    }

    /// React's built-in wrappers render their children: `Fragment`, `Suspense`, `StrictMode`, and
    /// `Profiler`, imported by name or read from the `react` module.
    fn react_builtin_component(
        &self,
        file_id: FileId,
        local: &str,
        member: Option<&str>,
    ) -> Option<ComponentConsumer> {
        let imported = self.imported_from(file_id, local, &["react"])?;
        let export = match member {
            Some(member) if imported == "*" || imported == "default" => member.to_owned(),
            Some(_) => return None,
            None => imported,
        };
        matches!(
            export.as_str(),
            "Fragment" | "Suspense" | "StrictMode" | "Profiler"
        )
        .then(|| ComponentConsumer {
            module: "react".to_owned(),
            export,
            forward_children: true,
            invoke_children: false,
            render_props: Vec::new(),
            render_callback_names: Vec::new(),
            component_props: Vec::new(),
            member: None,
            curried: false,
        })
    }

    /// Finds a configured consumer whose module and export the tag's import chain passes through,
    /// so a contract on a package export also covers barrels that re-export it.
    fn linked_component_consumer(
        &self,
        file_id: FileId,
        local: &str,
        member: Option<&str>,
    ) -> Option<crate::project::ComponentConsumer> {
        let consumers = &self.project.config.component_consumers;
        if consumers.is_empty() {
            return None;
        }
        let hops = self.symbol_linker.linked_exports(file_id, local);
        consumers
            .iter()
            .filter(|model| model.member.is_none())
            .find(|model| {
                hops.iter().any(|(module, export)| {
                    model.module == *module
                        && match member {
                            Some(member) if export == "*" => model.export == member,
                            Some(member) => model.export == format!("{export}.{member}"),
                            None => model.export == *export,
                        }
                })
            })
            .cloned()
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
        if self.current_reachability == Reachability::Reachable {
            self.reach
                .rendered
                .insert((element.span.file_id.0, element.span.start));
        }
        self.render_log
            .push(RenderMark::Site(element.span.file_id.0, element.span.start));
        if let AbstractValue::Function(key) = &element.component.value {
            self.render_log.push(RenderMark::Function(key.clone()));
        }
        if self.current_reachability == Reachability::Possible {
            if self.assumed_budget_exhausted() {
                return;
            }
            // Under an assumption, explore only what can lead toward a factory host: components
            // on a backward use chain (or, without one, in corridor files), or components handed
            // JSX or callbacks from the path above.
            if let AbstractValue::Function(key) = &element.component.value
                && !self.leads_toward_factory(key)
                && !element.props.values().any(|value| {
                    self.carries_render_content(value, 0) || contains_capability(value)
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
        // A shared wrapper renders different children at each use. Key its visits by the JSX it
        // receives so one busy wrapper cannot exhaust the budget for content passed elsewhere.
        let mut content = Vec::new();
        for value in element.props.values() {
            self.collect_render_content_sites(value, &mut content, 0);
        }
        content.sort_unstable();
        content.dedup();
        let identity = if content.is_empty() {
            identity
        } else {
            format!(
                "{identity}|{}",
                content
                    .iter()
                    .map(|(file, start)| format!("{file}:{start}"))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        };
        // Another assumed path to the same component with the same props would only repeat its
        // results, so explore each such render once. A render cut short by a budget is not
        // remembered, so a later path can still complete it, and props too deep to fingerprint
        // are always explored.
        let mut memo = None;
        // Unions only dispatch to their alternatives, which are remembered individually. Exact
        // renders are remembered too: creations are interned, so a repeated subtree with the same
        // props would only repeat its results, and it replays what it showed its ancestors.
        if self.current_reachability != Reachability::Unknown
            && !matches!(
                element.component.value,
                AbstractValue::Intrinsic(_) | AbstractValue::Union(_)
            )
        {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            let mut complete = true;
            for (name, value) in &element.props {
                std::hash::Hash::hash(name, &mut hasher);
                complete &= hash_value_fingerprint(&self.heap, value, &mut hasher, 0);
            }
            let fingerprint = std::hash::Hasher::finish(&hasher);
            let component = match &element.component.value {
                AbstractValue::Function(key) => format!("function:{}:{}", key.file_id.0, key.name),
                AbstractValue::Closure(closure) => {
                    format!("closure:{}:{}", closure.file_id.0, closure.span.start)
                }
                AbstractValue::ConfiguredComponent(model) => {
                    format!("configured:{}#{}", model.module, model.export)
                }
                AbstractValue::Unknown(_) | AbstractValue::AssumedWrapper { .. } => {
                    format!("unknown:{}:{}", element.span.file_id.0, element.span.start)
                }
                _ => format!(
                    "other:{}:{}:{}",
                    element.span.file_id.0,
                    element.span.start,
                    render_compact_value(&element.component)
                ),
            };
            let memo_key = (self.current_choice.clone(), component, fingerprint);
            if complete && self.current_reachability == Reachability::Possible {
                if !self.assumed_component_renders.insert(memo_key.clone()) {
                    self.trace = previous_trace;
                    return;
                }
                memo = Some(RenderMemo::Assumed(memo_key, self.render_truncations));
            } else if complete {
                match self.exact_renders.get(&memo_key) {
                    Some(Some(replay)) => {
                        let replay = Rc::clone(replay);
                        self.render_log.extend(replay.marks.iter().cloned());
                        self.uncertainty_events += replay.uncertainty;
                        self.trace = previous_trace;
                        return;
                    }
                    // The same render is already running further up this path.
                    Some(None) => {
                        self.trace = previous_trace;
                        return;
                    }
                    None => {
                        self.exact_renders.insert(memo_key.clone(), None);
                        memo = Some(RenderMemo::Exact {
                            key: memo_key,
                            truncations: self.render_truncations,
                            log_start: self.render_log.len(),
                            uncertainty: self.uncertainty_events,
                        });
                    }
                }
            }
        }
        // Assumed renders have their own budget so they cannot crowd out exact paths.
        let key = (
            self.current_choice.clone(),
            self.current_reachability == Reachability::Possible,
            element.span.file_id,
            element.span.start,
            identity,
        );
        let budget = if self.current_reachability == Reachability::Reachable {
            MAX_EXACT_RENDER_VISITS_PER_SITE
        } else {
            MAX_RENDER_VISITS_PER_SITE
        };
        let visits = self.render_visits.entry(key).or_default();
        *visits += 1;
        if *visits > budget {
            if self.current_reachability == Reachability::Reachable {
                self.reach
                    .cut
                    .insert((element.span.file_id.0, element.span.start));
                if let AbstractValue::Function(key) = &element.component.value {
                    self.reach.cut_components.insert(key.clone());
                }
            }
            self.render_truncations += 1;
            match &memo {
                Some(RenderMemo::Assumed(key, _)) => {
                    self.assumed_component_renders.remove(key);
                }
                Some(RenderMemo::Exact { key, .. }) => {
                    self.exact_renders.remove(key);
                }
                None => {}
            }
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
                let pending = self.pending_callbacks.len();
                let props = element.props.clone();
                let tracking = self.begin_render_tracking(&props);
                let mut received = element.props;
                self.apply_default_props(key, &mut received);
                if received.open.is_none() && self.wrapped_components.contains(key) {
                    received.open = Some("props_from_configured_wrapper".to_owned());
                }
                let argument = TrackedValue {
                    value: AbstractValue::Record(Rc::new(received)),
                    evidence: element.component.evidence,
                    choice: element.component.choice.clone(),
                    heap_id: None,
                };
                let returned = self.call_function(key, vec![argument]);
                self.render(returned);
                self.finish_render_tracking(
                    tracking,
                    &element.span,
                    element.tag.as_deref(),
                    &format!("{}:{}", key.file_id.0, key.name),
                    &props,
                );
                if self.model.scan_callback_bodies {
                    for prop in props.values() {
                        self.scan_callback_bodies(prop, &element.span, 0);
                    }
                }
                self.run_uncalled_callbacks(pending);
            }
            AbstractValue::Closure(closure) => {
                let pending = self.pending_callbacks.len();
                let props = element.props.clone();
                let tracking = self.begin_render_tracking(&props);
                let argument = TrackedValue::plain(AbstractValue::Record(Rc::new(element.props)));
                let returned = self.invoke_value(
                    TrackedValue::plain(AbstractValue::Closure(closure.clone())),
                    vec![argument],
                    element.span.clone(),
                );
                self.render(returned);
                self.finish_render_tracking(
                    tracking,
                    &element.span,
                    element.tag.as_deref(),
                    &format!("{}:{}", closure.file_id.0, closure.span.start),
                    &props,
                );
                if self.model.scan_callback_bodies {
                    for prop in props.values() {
                        self.scan_callback_bodies(prop, &element.span, 0);
                    }
                }
                self.run_uncalled_callbacks(pending);
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
                        // It may return the element, or a component to render, as a screen's
                        // `getComponent` does.
                        self.render_assumed_prop(&returned, &element.span);
                    }
                }
                for prop in &model.component_props {
                    if let Some(component) = element.props.get(prop) {
                        self.render(TrackedValue::plain(AbstractValue::element(ElementValue {
                            component: Box::new(component.clone()),
                            props: RecordFields {
                                fields: BTreeMap::new(),
                                open: Some("props_from_configured_component".to_owned()),
                                unkeyed: Vec::new(),
                            },
                            span: element.span.clone(),
                            trace: self.trace.clone(),
                            tag: None,
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
                    if let Some(tag) = element.tag.clone() {
                        self.request_component_factory(&tag);
                    }
                    self.render_through_unmodeled_component(&element, &[]);
                }
            }
            AbstractValue::AssumedWrapper {
                reason,
                wrapped,
                callee,
                ..
            } => {
                // A root path renders this wrapper, so its module may model it exactly.
                if let Some(callee) = callee
                    && self.current_reachability != Reachability::Unknown
                {
                    for path in self
                        .symbol_linker
                        .unparsed_link_targets(callee.0, &callee.1)
                    {
                        self.request_root_import(&path);
                        self.requested_imports.insert(path);
                    }
                }
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
                        tag: element.tag.clone(),
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
        match memo {
            Some(RenderMemo::Assumed(key, truncations))
                if self.render_truncations != truncations =>
            {
                self.assumed_component_renders.remove(&key);
            }
            Some(RenderMemo::Exact {
                key,
                truncations,
                log_start,
                uncertainty,
            }) => {
                if self.render_truncations == truncations {
                    // Ancestors may watch JSX that arrived in props or inside captured closures.
                    let marks = self.render_log[log_start..]
                        .iter()
                        .cloned()
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    let replay = RenderReplay {
                        marks,
                        uncertainty: self.uncertainty_events - uncertainty,
                    };
                    self.exact_renders.insert(key, Some(Rc::new(replay)));
                } else {
                    self.exact_renders.remove(&key);
                }
            }
            _ => {}
        }
        self.trace = previous_trace;
    }

    /// Requests the module of the call that initializes a rendered component, as in
    /// `const Panel = createPanel(...)`, so a parsed factory can model the component.
    fn request_component_factory(&mut self, tag: &TagOrigin) {
        let ValueResolution::Resolved(LinkedValue::Declaration(symbol)) =
            self.symbol_linker.resolve_binding(tag.file_id, &tag.local)
        else {
            return;
        };
        let Some(index) = self.global_bindings.get(&symbol).copied() else {
            return;
        };
        let (file_id, binding) = self.globals_ir[index];
        let FlowExpressionKind::Call { callee, .. } = &binding.value.kind else {
            return;
        };
        let Some(local) = imported_callee_local(callee) else {
            return;
        };
        for path in self.symbol_linker.unparsed_link_targets(file_id, local) {
            self.request_root_import(&path);
            self.requested_imports.insert(path);
        }
    }

    /// Builds, for every function and module binding, the code that uses it and where.
    fn use_graph(&self) -> std::collections::HashMap<UseNode, Vec<UseEdge>> {
        let mut graph = std::collections::HashMap::<UseNode, Vec<UseEdge>>::new();
        let node_for = |symbol: &LinkedSymbol| {
            // A name destructured from a namespace re-exports that module's declaration.
            let target = self.destructured_namespace_export(symbol);
            let symbol = target.as_ref().unwrap_or(symbol);
            let key = FunctionKey {
                file_id: symbol.file_id,
                name: symbol.name.clone(),
            };
            if self.functions.contains_key(&key) {
                Some(UseNode::Function(key))
            } else {
                self.global_bindings
                    .get(symbol)
                    .map(|index| UseNode::Global(*index))
            }
        };
        let mut nodes = Vec::new();
        for file in &self.snapshot.files {
            for function in &file.flow.functions {
                let mut uses = Vec::new();
                collect_uses_in_statements(&function.body, &UseContext::Direct, None, &mut uses);
                nodes.push((
                    file,
                    UseNode::Function(FunctionKey {
                        file_id: file.file_id,
                        name: function.name.clone(),
                    }),
                    uses,
                ));
            }
        }
        for (index, (file_id, binding)) in self.globals_ir.iter().enumerate() {
            let Some(file) = self.symbol_linker.file(*file_id) else {
                continue;
            };
            let mut uses = Vec::new();
            // A component defined as a function value runs its body when rendered.
            match &binding.value.kind {
                FlowExpressionKind::Arrow { body, .. } => {
                    collect_uses_in_body(body, &UseContext::Direct, None, &mut uses);
                }
                _ => collect_uses(&binding.value, &UseContext::Direct, None, &mut uses),
            }
            nodes.push((file, UseNode::Global(index), uses));
        }
        for (file, user, uses) in nodes {
            for usage in uses {
                let target = if let Some(name) = usage.name {
                    match self.symbol_linker.resolve_binding(file.file_id, name) {
                        ValueResolution::Resolved(LinkedValue::Declaration(symbol)) => {
                            node_for(&symbol)
                        }
                        _ => None,
                    }
                } else {
                    usage.module.and_then(|module| {
                        let path = self
                            .symbol_linker
                            .import_resolutions(&file.path)
                            .filter(|resolution| resolution.specifier == module)
                            .find_map(|resolution| resolution.resolved_path.as_ref())?;
                        let target = self.symbol_linker.file_at(path)?;
                        match self
                            .symbol_linker
                            .resolve_exported_value(target.file_id, "default")
                        {
                            ValueResolution::Resolved(LinkedValue::Declaration(symbol)) => {
                                node_for(&symbol)
                            }
                            _ => None,
                        }
                    })
                };
                if let Some(target) = target
                    && target != user
                {
                    graph.entry(target).or_default().push(UseEdge {
                        user: user.clone(),
                        site: usage.site,
                        context: usage.context,
                    });
                }
            }
        }
        graph
    }

    /// For a module binding such as `const Page = load({ promise: () => import('./Page') })`, the
    /// loader call and a `lazy_component_factories` entry that would follow it.
    fn lazy_loader_contract(&self, node: &UseNode) -> Option<(String, String)> {
        let UseNode::Global(index) = node else {
            return None;
        };
        let (file_id, binding) = self.globals_ir[*index];
        let FlowExpressionKind::Call { callee, arguments } = &binding.value.kind else {
            return None;
        };
        let FlowExpressionKind::Record { fields } = &arguments.first()?.kind else {
            return None;
        };
        let property = fields.iter().find_map(|field| {
            let FlowExpressionKind::Arrow { body, .. } = &field.value.kind else {
                return None;
            };
            let returned = match body {
                FlowArrowBody::Expression { expression } => Some(expression.as_ref()),
                FlowArrowBody::Statements { statements } => {
                    statements.iter().find_map(|statement| match statement {
                        FlowStatement::Return {
                            value: Some(value), ..
                        } => Some(value),
                        _ => None,
                    })
                }
            }?;
            matches!(returned.kind, FlowExpressionKind::DynamicImport { .. })
                .then(|| field.property.clone())
        })?;
        let local = imported_callee_local(callee)?;
        let import = self
            .symbol_linker
            .file(file_id)?
            .flow
            .imports
            .iter()
            .find(|import| import.local == local && !import.type_only)?;
        let quote =
            |value: &str| format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""));
        Some((
            callee_text(callee),
            format!(
                "[[lazy_component_factories]]\nmodule = {}\nexport = {}\npromise_property = {}\n",
                quote(&import.module),
                quote(&import.imported),
                quote(&property)
            ),
        ))
    }

    /// Whether exact exploration ran a node: a function was called, or a closure defined in a
    /// module binding was.
    fn node_explored(&self, node: &UseNode) -> bool {
        match node {
            UseNode::Function(key) => self.reach.functions.contains(key),
            UseNode::Global(index) => {
                let (file_id, binding) = self.globals_ir[*index];
                self.reach.closures.get(&file_id).is_some_and(|starts| {
                    starts
                        .range(binding.span.start..binding.span.end)
                        .next()
                        .is_some()
                })
            }
        }
    }

    fn describe_node(&self, node: &UseNode) -> String {
        let (name, span) = match node {
            UseNode::Function(key) => (
                key.name.clone(),
                self.functions
                    .get(key)
                    .map(|function| function.span.clone()),
            ),
            UseNode::Global(index) => {
                let (_, binding) = self.globals_ir[*index];
                (
                    pattern_names(&binding.pattern).join(", "),
                    Some(binding.span.clone()),
                )
            }
        };
        span.and_then(|span| self.query_location(&span))
            .map_or(name.clone(), |location| {
                format!("{name} ({}:{})", location.path, location.start_line)
            })
    }

    /// Why exact exploration did not follow a use inside explored code.
    fn classify_unfollowed_use(
        &self,
        site: &SourceSpan,
        context: &UseContext,
    ) -> (QueryUnreachedReason, String) {
        let key = (site.file_id.0, site.start);
        if self.reach.cut.contains(&key) {
            return (
                QueryUnreachedReason::RenderBudget,
                "the render visit budget stopped exact exploration at this use".to_owned(),
            );
        }
        match context {
            UseContext::CallArgument(callee) => (
                QueryUnreachedReason::Callback,
                format!("the use is inside a callback passed to {callee}, which was not invoked"),
            ),
            UseContext::PropCallback(prop) => (
                QueryUnreachedReason::PropCallback,
                format!("the use is inside a function passed as JSX prop {prop}, which was not invoked"),
            ),
            UseContext::LocalCallback => (
                QueryUnreachedReason::LocalCallback,
                "the use is inside a local function that was not called".to_owned(),
            ),
            UseContext::LazyImport => (
                QueryUnreachedReason::LazyImport,
                "the use is a dynamic import() that the model does not follow".to_owned(),
            ),
            UseContext::Direct if self.reach.rendered.contains(&key) => (
                QueryUnreachedReason::ComponentNotFollowed,
                "the element was rendered, but the component's body was not explored".to_owned(),
            ),
            UseContext::Direct if self.reach.evaluated.contains(&key) => (
                QueryUnreachedReason::CreatedNotRendered,
                "the use was evaluated, but the JSX was not rendered or the callee was not followed"
                    .to_owned(),
            ),
            UseContext::Direct => (
                QueryUnreachedReason::BranchNotTaken,
                "the explored code did not evaluate this use".to_owned(),
            ),
        }
    }

    /// Explains each matching factory callsite that no exact or possible path reached: the
    /// nearest user explored exactly and why exploration did not follow the use inside it.
    fn explain_unreached_callsites(&self) -> Vec<QueryUnreachedCallsite> {
        const SEARCH_LIMIT: usize = 4_000;
        let candidates = self
            .factory_candidates()
            .into_iter()
            .filter(|candidate| self.expression_matches_model(candidate.file_id, &candidate.callee))
            .filter(|candidate| {
                !self.capabilities.iter().any(|capability| {
                    capability.callsite == candidate.span
                        && capability.reachability != Reachability::Unknown
                })
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Vec::new();
        }
        let graph = self.use_graph();
        let mut explained = Vec::new();
        for candidate in candidates {
            let start = candidate.enclosing_function.clone().map_or_else(
                || {
                    self.globals_ir
                        .iter()
                        .position(|(file_id, binding)| {
                            *file_id == candidate.file_id
                                && binding.span.start <= candidate.span.start
                                && candidate.span.end <= binding.span.end
                        })
                        .map(UseNode::Global)
                },
                |key| Some(UseNode::Function(key)),
            );
            let mut result = QueryUnreachedCallsite {
                location: self.query_location(&candidate.span),
                enclosing: start.as_ref().map(|node| self.describe_node(node)),
                reason: QueryUnreachedReason::NoExploredAncestor,
                detail: "no user of this code was explored exactly within the parsed files"
                    .to_owned(),
                explored_ancestor: None,
                blocking_site: None,
                chain: Vec::new(),
                suggested_contract: None,
            };
            let Some(start) = start else {
                explained.push(result);
                continue;
            };
            if self.node_explored(&start) {
                // The callsite's own code ran; find how the callsite sits inside it.
                let context = graph
                    .values()
                    .flatten()
                    .find(|edge| edge.user == start && edge.site == candidate.span)
                    .map_or(UseContext::Direct, |edge| edge.context.clone());
                let (reason, detail) = self.classify_unfollowed_use(&candidate.span, &context);
                result.reason = reason;
                result.detail = detail;
                result.explored_ancestor = Some(self.describe_node(&start));
                result.blocking_site = self.query_location(&candidate.span);
                explained.push(result);
                continue;
            }
            let mut previous = std::collections::HashMap::<UseNode, Option<UseNode>>::new();
            previous.insert(start.clone(), None);
            let mut queue = VecDeque::from([start.clone()]);
            let mut found = None;
            let mut tops = Vec::new();
            while let Some(node) = queue.pop_front() {
                if previous.len() > SEARCH_LIMIT {
                    break;
                }
                let edges = graph.get(&node).map_or(&[][..], Vec::as_slice);
                if edges.is_empty() && tops.len() < 3 {
                    tops.push(node.clone());
                }
                if let Some(edge) = edges.iter().find(|edge| self.node_explored(&edge.user)) {
                    found = Some((node, edge));
                    break;
                }
                for edge in edges {
                    if !previous.contains_key(&edge.user) {
                        previous.insert(edge.user.clone(), Some(node.clone()));
                        queue.push_back(edge.user.clone());
                    }
                }
            }
            if let Some((node, edge)) = found {
                let mut chain_nodes = Vec::new();
                let mut current = Some(node.clone());
                while let Some(step) = current {
                    current = previous.get(&step).cloned().flatten();
                    chain_nodes.push(step);
                }
                chain_nodes.reverse();
                let chain = chain_nodes
                    .iter()
                    .map(|step| self.describe_node(step))
                    .collect::<Vec<_>>();
                let (reason, mut detail) = self.classify_unfollowed_use(&edge.site, &edge.context);
                // An ancestor explored in one context may have been cut by the budget in the one
                // that leads here.
                let cut = std::iter::once(&edge.user)
                    .chain(&chain_nodes)
                    .filter_map(|node| match node {
                        UseNode::Function(key) if self.reach.cut_components.contains(key) => {
                            Some(key.name.clone())
                        }
                        _ => None,
                    })
                    .collect::<BTreeSet<_>>();
                if !cut.is_empty() {
                    detail = format!(
                        "{detail}; the render visit budget also stopped some renders of {}, which may be the path that leads here",
                        cut.into_iter().collect::<Vec<_>>().join(", ")
                    );
                }
                if let Some((callee, contract)) = self.lazy_loader_contract(&node) {
                    detail = format!(
                        "{detail}; the component comes from a lazy loader call to {callee}, which a \
                         lazy_component_factories entry would follow"
                    );
                    result.suggested_contract = Some(contract);
                }
                result.reason = reason;
                result.detail = detail;
                result.explored_ancestor = Some(self.describe_node(&edge.user));
                result.blocking_site = self.query_location(&edge.site);
                result.chain = chain;
            } else if !tops.is_empty() {
                result.detail = format!(
                    "no user of this code was explored exactly within the parsed files; the use chain ends at {}",
                    tops.iter()
                        .map(|node| self.describe_node(node))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            explained.push(result);
        }
        explained
    }

    /// Records one site of a boundary and returns its key.
    fn register_boundary(
        &mut self,
        key: String,
        span: &SourceSpan,
        from_reachable: bool,
        make: impl FnOnce() -> BoundaryState,
    ) -> Rc<str> {
        let key: Rc<str> = Rc::from(key);
        let state = self.boundaries.entry(Rc::clone(&key)).or_insert_with(make);
        state.entered_from_reachable |= from_reachable;
        if state.site_keys.insert((span.file_id.0, span.start)) && state.sites.len() < 10 {
            state.sites.push(span.clone());
        }
        key
    }

    /// The module specifier and export a tag imports, as written at the tag's file.
    fn tag_import(&self, tag: &TagOrigin) -> (Option<String>, Option<String>) {
        let Some(import) = self.symbol_linker.file(tag.file_id).and_then(|file| {
            file.flow
                .imports
                .iter()
                .find(|import| import.local == tag.local && !import.type_only)
        }) else {
            return (None, None);
        };
        let export = match &tag.member {
            Some(member) if import.imported == "*" => member.clone(),
            Some(member) => format!("{}.{member}", import.imported),
            None => import.imported.clone(),
        };
        (Some(import.module.clone()), Some(export))
    }

    /// Classifies an element whose component the model could not follow and records the site.
    fn unmodeled_boundary(&mut self, element: &ElementValue, from_reachable: bool) -> Rc<str> {
        let reason = match &element.component.value {
            AbstractValue::Unknown(reason) | AbstractValue::AssumedWrapper { reason, .. } => {
                reason.clone()
            }
            _ => "unmodeled component".to_owned(),
        };
        let wrapper = match &element.component.value {
            AbstractValue::AssumedWrapper {
                callee: Some(callee),
                argument,
                ..
            } => self
                .symbol_linker
                .file(callee.0)
                .and_then(|file| {
                    file.flow
                        .imports
                        .iter()
                        .find(|import| import.local == callee.1 && !import.type_only)
                })
                .map(|import| (import.module.clone(), import.imported.clone(), *argument)),
            _ => None,
        };
        // A linked value is keyed by its declaration, so every import of it aggregates.
        let mut identity = None;
        let (kind, module, export, component) = match element.tag.as_deref() {
            Some(tag) => {
                let (site_module, site_export) = self.tag_import(tag);
                let member = |export: &str| match &tag.member {
                    Some(member) if export == "*" => member.clone(),
                    Some(member) => format!("{export}.{member}"),
                    None => export.to_owned(),
                };
                if site_module.is_none() {
                    (QueryBoundaryKind::DynamicValue, None, None, tag.text())
                } else {
                    let unparsed = self
                        .symbol_linker
                        .unparsed_link_targets(tag.file_id, &tag.local);
                    let resolution = self.symbol_linker.resolve_binding(tag.file_id, &tag.local);
                    let last = self
                        .symbol_linker
                        .linked_exports(tag.file_id, &tag.local)
                        .last()
                        .cloned();
                    let at_last = |kind| {
                        last.as_ref().map_or(
                            (kind, site_module.clone(), site_export.clone(), tag.text()),
                            |(module, export)| {
                                (kind, Some(module.clone()), Some(member(export)), tag.text())
                            },
                        )
                    };
                    if !unparsed.is_empty() {
                        if unparsed.iter().any(|path| {
                            path.components()
                                .any(|part| part.as_os_str() == "node_modules")
                        }) {
                            at_last(QueryBoundaryKind::ExternalPackage)
                        } else {
                            at_last(QueryBoundaryKind::UnparsedSource)
                        }
                    } else if resolution == ValueResolution::Unresolved {
                        let bare =
                            last.as_ref().is_some_and(|(module, _)| {
                                !module.starts_with('.')
                                    && !module.starts_with('/')
                                    && self.project.config.import_aliases.keys().all(|alias| {
                                        !module.starts_with(alias.trim_end_matches('*'))
                                    })
                            });
                        at_last(if bare {
                            QueryBoundaryKind::ExternalPackage
                        } else {
                            QueryBoundaryKind::UnresolvedImport
                        })
                    } else {
                        identity = match resolution {
                            ValueResolution::Resolved(LinkedValue::Declaration(symbol)) => {
                                Some(format!("{}:{}", symbol.file_id.0, member(&symbol.name)))
                            }
                            ValueResolution::Resolved(LinkedValue::Namespace(file_id)) => {
                                Some(format!("{}:{}", file_id.0, member("*")))
                            }
                            _ => None,
                        };
                        (
                            QueryBoundaryKind::DynamicValue,
                            site_module,
                            site_export,
                            tag.text(),
                        )
                    }
                }
            }
            None => (
                QueryBoundaryKind::DynamicValue,
                None,
                None,
                "component value".to_owned(),
            ),
        };
        let key = match (&module, &export, &wrapper) {
            (_, _, Some((module, export, _))) => format!("wrapper:{module}#{export}"),
            _ if identity.is_some() => format!("{kind:?}:{}", identity.unwrap_or_default()),
            (Some(module), Some(export), None) => format!("{kind:?}:{module}#{export}"),
            _ => format!("{kind:?}:{}:{}", element.span.file_id.0, element.span.start),
        };
        let shape = contract_shape(&element.props, |value| self.is_component_like(value));
        let key = self.register_boundary(key, &element.span, from_reachable, || {
            let mut state = BoundaryState::new(kind, component, module, export, reason);
            state.wrapper = wrapper;
            state
        });
        if let Some(state) = self.boundaries.get_mut(&key) {
            state.forward_children |= shape.0;
            state.invoke_children |= shape.1;
            state.render_props.extend(shape.2);
            state.component_props.extend(shape.3);
        }
        key
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
        let boundary = self.unmodeled_boundary(element, previous == Reachability::Reachable);
        self.current_reachability = Reachability::Possible;
        self.assumed_renders
            .push((element.span.clone(), Assumption::UnmodeledComponent));
        self.assumed_boundaries.push(boundary);
        // The wrapper can add props of its own.
        let mut props = element.props.clone();
        props.open = Some("props_from_unmodeled_wrapper".to_owned());
        for component in wrapped {
            self.render(TrackedValue::plain(AbstractValue::element(ElementValue {
                component: Box::new(component.clone()),
                props: props.clone(),
                span: element.span.clone(),
                trace: self.trace.clone(),
                tag: None,
            })));
        }
        for value in element.props.values() {
            self.render_assumed_prop(value, &element.span);
        }
        self.assumed_renders.pop();
        self.assumed_boundaries.pop();
        self.current_reachability = previous;
    }

    /// Starts watching which received JSX a component renders. Only paths from configured roots
    /// are watched.
    fn begin_render_tracking(
        &self,
        props: &BTreeMap<String, TrackedValue>,
    ) -> Option<RenderTracking> {
        if self.current_reachability == Reachability::Unknown {
            return None;
        }
        let mut targets = Vec::new();
        for value in props.values() {
            self.collect_render_targets(value, &mut targets, 0);
        }
        (!targets.is_empty()).then_some(RenderTracking {
            log_start: self.render_log.len(),
            uncertainty: self.uncertainty_events,
            targets,
        })
    }

    fn collect_render_targets(
        &self,
        value: &TrackedValue,
        targets: &mut Vec<(RenderMark, TrackedValue)>,
        depth: usize,
    ) {
        if depth > 8 {
            return;
        }
        match &value.value {
            AbstractValue::Element(element) => targets.push((
                RenderMark::Site(element.span.file_id.0, element.span.start),
                value.clone(),
            )),
            AbstractValue::Closure(closure) if arrow_body_renders_jsx(&closure.body) => targets
                .push((
                    RenderMark::Site(closure.span.file_id.0, closure.span.start),
                    value.clone(),
                )),
            AbstractValue::Function(key) if self.is_component_like(value) => {
                targets.push((RenderMark::Function(key.clone()), value.clone()));
            }
            AbstractValue::Array(values) | AbstractValue::Union(values) => {
                for value in values.iter() {
                    self.collect_render_targets(value, targets, depth + 1);
                }
            }
            _ => {}
        }
    }

    /// After a component's body runs, renders received JSX it did not render if the body hit
    /// anything the model could not follow. A fully modeled body that drops JSX stays exact.
    fn finish_render_tracking(
        &mut self,
        tracking: Option<RenderTracking>,
        span: &SourceSpan,
        tag: Option<&TagOrigin>,
        component: &str,
        props: &RecordFields,
    ) {
        let Some(tracking) = tracking else {
            return;
        };
        if self.uncertainty_events == tracking.uncertainty {
            return;
        }
        let rendered = self.render_log[tracking.log_start..]
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let unrendered = tracking
            .targets
            .into_iter()
            .filter(|(mark, _)| !rendered.contains(mark))
            .map(|(_, value)| value)
            .collect::<Vec<_>>();
        if unrendered.is_empty() {
            return;
        }
        let previous = self.current_reachability;
        let (module, export) = tag.map_or((None, None), |tag| self.tag_import(tag));
        let key = format!("partial:{component}");
        let operations = if self.boundaries.contains_key(key.as_str()) {
            Vec::new()
        } else {
            self.uncertainty_since(tracking.uncertainty)
        };
        let operations = operations
            .into_iter()
            .map(|(reason, span)| {
                let location = self.query_location(&span).map_or_else(
                    || format!("file {} bytes {}", span.file_id.0, span.start),
                    |location| format!("{}:{}", location.path, location.start_line),
                );
                format!("{reason} at {location}")
            })
            .collect::<Vec<_>>();
        let boundary =
            self.register_boundary(key, span, previous == Reachability::Reachable, || {
                BoundaryState::new(
                    QueryBoundaryKind::PartiallyModeled,
                    tag.map_or_else(|| component.to_owned(), TagOrigin::text),
                    module,
                    export,
                    format!(
                        "component body reached an unmodeled operation and did not render JSX it \
                     received{}; a contract would replace the body in the model, so factory calls \
                     inside it would no longer be explored",
                        if operations.is_empty() {
                            String::new()
                        } else {
                            format!(" (first: {})", operations.join("; "))
                        }
                    ),
                )
            });
        let shape = contract_shape(props, |value| self.is_component_like(value));
        if let Some(state) = self.boundaries.get_mut(&boundary) {
            state.forward_children |= shape.0;
            state.invoke_children |= shape.1;
            state.render_props.extend(shape.2);
            state.component_props.extend(shape.3);
        }
        self.current_reachability = Reachability::Possible;
        self.assumed_renders
            .push((span.clone(), Assumption::UnrenderedJsx));
        self.assumed_boundaries.push(boundary);
        let trace_len = self.trace.len();
        self.trace.push(TraceStep {
            kind: QueryCallPathKind::AssumedRender,
            span: span.clone(),
        });
        for value in &unrendered {
            self.render_assumed_prop(value, span);
        }
        self.trace.truncate(trace_len);
        self.assumed_renders.pop();
        self.assumed_boundaries.pop();
        self.current_reachability = previous;
    }

    /// Renders JSX elements that leave the model at `span`, such as children handed to an unknown
    /// call. Only possible paths are explored from them. Components passed to such calls are left
    /// to the assumed-wrapper value the call returns.
    fn render_escaped_jsx<'b>(
        &mut self,
        values: impl Iterator<Item = &'b TrackedValue>,
        span: &SourceSpan,
    ) {
        if self.current_reachability == Reachability::Unknown {
            return;
        }
        let elements = values
            .filter(|value| contains_element(value, 0))
            .cloned()
            .collect::<Vec<_>>();
        if elements.is_empty() {
            return;
        }
        let previous = self.current_reachability;
        let boundary = self.register_boundary(
            format!("escaped:{}:{}", span.file_id.0, span.start),
            span,
            previous == Reachability::Reachable,
            || {
                BoundaryState::new(
                    QueryBoundaryKind::EscapedJsx,
                    "JSX passed to an unmodeled call".to_owned(),
                    None,
                    None,
                    "JSX handed to an unmodeled call or unsupported expression".to_owned(),
                )
            },
        );
        self.current_reachability = Reachability::Possible;
        self.assumed_renders
            .push((span.clone(), Assumption::EscapedJsx));
        self.assumed_boundaries.push(boundary);
        let trace_len = self.trace.len();
        self.trace.push(TraceStep {
            kind: QueryCallPathKind::AssumedRender,
            span: span.clone(),
        });
        for value in elements {
            self.render(value);
        }
        self.trace.truncate(trace_len);
        self.assumed_renders.pop();
        self.assumed_boundaries.pop();
        self.current_reachability = previous;
    }

    /// The value of a module's export, for a module specifier written in a file. A module that is
    /// not parsed is requested, and gives `None` like one that does not resolve.
    fn module_export_value(
        &mut self,
        file_id: FileId,
        module: &str,
        export: &str,
        span: &SourceSpan,
    ) -> Option<TrackedValue> {
        let AbstractValue::Namespace(module) = self.module_namespace(file_id, module, span)?.value
        else {
            return None;
        };
        let exported = self.symbol_linker.resolve_exported_value(module, export);
        Some(self.linked_value(exported, span))
    }

    /// The namespace of a module, for a module specifier written in a file. A module that is not
    /// parsed is requested, and gives `None` like one that does not resolve.
    fn module_namespace(
        &mut self,
        file_id: FileId,
        module: &str,
        span: &SourceSpan,
    ) -> Option<TrackedValue> {
        let target = self
            .symbol_linker
            .file(file_id)
            .and_then(|file| {
                self.symbol_linker
                    .import_resolutions(&file.path)
                    .find(|resolution| resolution.specifier == module)
            })
            .and_then(|resolution| resolution.resolved_path.clone());
        let Some(target) = target else {
            self.record_coverage_gap("module import did not resolve", span);
            return None;
        };
        if let Some(file) = self.symbol_linker.file_at(&target) {
            return Some(TrackedValue::plain(AbstractValue::Namespace(file.file_id)));
        }
        if self.current_reachability != Reachability::Unknown {
            self.request_root_import(&target);
        }
        self.requested_imports.insert(target);
        None
    }

    /// Explores what a configured opener renders, as a configured component's render props are:
    /// the component with its props, or what the render function returns. The opener's other
    /// arguments go to it as to an unknown call.
    fn open_component(
        &mut self,
        opener: &ComponentOpener,
        call: &FlowExpression,
        arguments: &[FlowExpression],
        environment: &Environment,
        file_id: FileId,
    ) -> TrackedValue {
        let values = arguments
            .iter()
            .map(|argument| self.eval(argument, environment, file_id))
            .collect::<Vec<_>>();
        if let Some(index) = opener.component_argument
            && let Some(argument) = arguments.get(index)
            && let Some(component) =
                self.opened_component(argument, &values[index], environment, file_id, &call.span)
        {
            let props = opener
                .props_argument
                .and_then(|index| values.get(index))
                .map(|props| {
                    opener.props_path.iter().fold(props.clone(), |value, step| {
                        self.read_property(
                            value,
                            step,
                            call.span.clone(),
                            RelationKind::ValueTransfer,
                        )
                    })
                });
            let props = match props.map(|props| self.materialize(&props).value) {
                Some(AbstractValue::Record(fields)) => Rc::unwrap_or_clone(fields),
                _ => RecordFields {
                    fields: BTreeMap::new(),
                    open: Some("props_from_component_opener".to_owned()),
                    unkeyed: Vec::new(),
                },
            };
            self.render(TrackedValue::plain(AbstractValue::element(ElementValue {
                component: Box::new(component),
                props,
                span: call.span.clone(),
                trace: self.trace.clone(),
                tag: None,
            })));
        }
        if let Some(render) = opener.render_argument.and_then(|index| values.get(index)) {
            let returned = self.invoke_value(
                render.clone(),
                vec![TrackedValue::unknown("component_opener_render_argument")],
                call.span.clone(),
            );
            // The function may return the element, or a component the opener renders.
            self.render_assumed_prop(&returned, &call.span);
        }
        let consumed = [
            opener.component_argument,
            opener.props_argument,
            opener.render_argument,
        ];
        let others = values
            .into_iter()
            .enumerate()
            .filter(|(index, _)| !consumed.contains(&Some(*index)))
            .map(|(_, value)| value)
            .collect();
        self.invoke_value(
            TrackedValue::unknown("configured_component_opener"),
            others,
            call.span.clone(),
        );
        TrackedValue::unknown("component_opener_result")
    }

    /// The component an opener's component argument holds: the component, or what an `import()`
    /// or a loader written at the call, or a loader called there, loads.
    fn opened_component(
        &mut self,
        argument: &FlowExpression,
        value: &TrackedValue,
        environment: &Environment,
        file_id: FileId,
        span: &SourceSpan,
    ) -> Option<TrackedValue> {
        if self.is_component_like(value) {
            return Some(value.clone());
        }
        // A loader that is called, as in `open(props.importer(), props)`, or passed itself.
        let loader = match &argument.kind {
            FlowExpressionKind::Call { callee, arguments } if arguments.is_empty() => {
                self.eval(callee, environment, file_id)
            }
            _ => value.clone(),
        };
        if let AbstractValue::Namespace(module) = loader.value {
            let exported = self.symbol_linker.resolve_exported_value(module, "default");
            return Some(self.linked_value(exported, span));
        }
        let (module_file, (module, export)) = match &loader.value {
            AbstractValue::Function(key) => (
                key.file_id,
                callback_uses::statements_module(&self.functions.get(key)?.body)?,
            ),
            AbstractValue::Closure(closure) => (
                closure.file_id,
                match &closure.body {
                    FlowArrowBody::Expression { expression } => {
                        callback_uses::returned_module(expression)?
                    }
                    FlowArrowBody::Statements { statements } => {
                        callback_uses::statements_module(statements)?
                    }
                },
            ),
            _ => (
                file_id,
                match &argument.kind {
                    FlowExpressionKind::DynamicImport { module } => {
                        (module.clone(), "default".to_owned())
                    }
                    _ => return None,
                },
            ),
        };
        self.module_export_value(module_file, &module, &export, span)
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
                        props: RecordFields {
                            fields: BTreeMap::new(),
                            open: Some("props_from_unmodeled_component".to_owned()),
                            unkeyed: Vec::new(),
                        },
                        span: span.clone(),
                        trace: self.trace.clone(),
                        tag: None,
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

    /// Whether a closure's body names a function on the use chain toward a factory callsite, such
    /// as a click handler that opens a modal holding one. Running it may reach the callsite.
    fn closure_leads_toward_factory(
        &mut self,
        file_id: FileId,
        span: &SourceSpan,
        body: &FlowArrowBody,
    ) -> bool {
        if self.use_chain.is_empty() {
            return false;
        }
        let key = (span.file_id.0, span.start);
        if let Some(&leads) = self.closures_toward_factory.get(&key) {
            return leads;
        }
        let leads = callback_uses::body_names(body).iter().any(|name| {
            match self.symbol_linker.resolve_binding(file_id, name) {
                ValueResolution::Resolved(LinkedValue::Declaration(symbol)) => {
                    self.use_chain.contains(&FunctionKey {
                        file_id: symbol.file_id,
                        name: symbol.name,
                    })
                }
                _ => false,
            }
        });
        self.closures_toward_factory.insert(key, leads);
        leads
    }

    fn leads_toward_factory(&self, key: &FunctionKey) -> bool {
        if self.declared_render && self.factory_ancestors.contains(key) {
            return true;
        }
        if !self.use_chain.is_empty() {
            return self.use_chain.contains(key) || self.corridor_files.is_empty();
        }
        self.corridor_files.is_empty() || self.corridor_files.contains(&key.file_id)
    }

    /// Counts one assumed-render step and reports when the per-root budget is exhausted.
    fn assumed_budget_exhausted(&mut self) -> bool {
        self.assumed_evaluations += 1;
        if self.assumed_evaluations <= self.assumed_budget {
            return false;
        }
        self.render_truncations += 1;
        if !self.assumed_budget_reported {
            self.assumed_budget_reported = true;
            if let Some((span, _)) = self.assumed_renders.first().cloned() {
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
                // Explore a possible later event call, which passes an event. React ignores its
                // return value.
                let arguments = closure
                    .params
                    .iter()
                    .take(1)
                    .map(|_| TrackedValue::unknown("intrinsic_event"))
                    .collect();
                self.call_closure(closure, arguments);
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
                        other_paths: 0,
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
        let stops = self.budget_stops();
        if self.current_reachability == Reachability::Reachable {
            self.reach
                .closures
                .entry(closure.span.file_id)
                .or_default()
                .insert(closure.span.start);
        }
        self.render_log
            .push(RenderMark::Site(closure.span.file_id.0, closure.span.start));
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
        let mut captures = self
            .body_references(&closure.span, &closure.body)
            .names
            .clone();
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
                closure.file_id,
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
        // A call a budget cut short may not have reached what the closure does.
        if self.budget_stops() == stops {
            closure.called.set(true);
        }
        returned
    }

    /// Grows whenever a budget stops an evaluation or render, so a caller can tell whether a
    /// call ran to completion.
    fn budget_stops(&self) -> usize {
        self.render_truncations
            + self
                .unreached_render_evaluations
                .saturating_sub(MAX_UNREACHED_RENDER_EVALUATIONS)
            + self
                .reverse_evaluations
                .saturating_sub(MAX_REVERSE_IMPORTER_EVALUATIONS)
    }

    fn call_model(&mut self, arguments: &[TrackedValue], span: SourceSpan) -> TrackedValue {
        let arguments = arguments
            .iter()
            .map(|value| self.materialize(value))
            .collect::<Vec<_>>();
        // Another exact path to the same callsite with the same choice and argument values is
        // the same creation; reusing it keeps the subtrees below identical, so they can be
        // remembered instead of explored again.
        let interning = (self.current_reachability == Reachability::Reachable).then(|| {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            (
                &span,
                &self.current_choice,
                arguments.iter().map(query_value).collect::<Vec<_>>(),
            )
                .hash(&mut hasher);
            hasher.finish()
        });
        if let Some(returned) = interning.and_then(|key| self.interned_creations.get(&key)) {
            return returned.clone();
        }
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
            let mut assumed = Vec::<(SourceSpan, Assumption)>::new();
            for assumption in &self.assumed_renders {
                if !assumed.contains(assumption) {
                    assumed.push(assumption.clone());
                }
            }
            for (span, assumption) in assumed {
                let (rule, summary, gap) = match assumption {
                    Assumption::UnmodeledComponent => (
                        "factory_callsite_reached_through_unmodeled_component",
                        "factory callsite is reached only if an unmodeled component renders it",
                        "render assumed through unmodeled component",
                    ),
                    Assumption::EscapedJsx => (
                        "factory_callsite_reached_through_escaped_jsx",
                        "factory callsite is reached only if JSX handed to an unmodeled operation is rendered",
                        "JSX passed to an unmodeled call",
                    ),
                    Assumption::UnrenderedJsx => (
                        "factory_callsite_reached_through_unrendered_jsx",
                        "factory callsite is reached only if a component whose body is not fully modeled renders JSX it received",
                        "JSX dropped by a partially modeled component",
                    ),
                };
                unresolved.push(self.push_evidence(
                    RelationKind::UnresolvedEscape,
                    rule,
                    span.clone(),
                    vec![origin],
                    Some(self.model.id.clone()),
                    summary,
                ));
                self.record_coverage_gap(gap, &span);
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
                other_paths: 0,
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
            factory_arguments: arguments.clone(),
            reachability: if self.declared_render
                && self.current_reachability != Reachability::Unknown
            {
                Reachability::Declared
            } else {
                self.current_reachability
            },
            reverse_importer: self.current_reverse_importer.clone(),
            registrations: Vec::new(),
            invocations,
            unresolved,
            assumptions,
            boundaries: if self.current_reachability == Reachability::Possible {
                let mut boundaries = self.assumed_boundaries.clone();
                boundaries.sort();
                boundaries.dedup();
                boundaries
            } else {
                Vec::new()
            },
            invocation_keys: std::collections::HashMap::new(),
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
        let returned = if let Some(index) = self.model.returned_index {
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
                    value: AbstractValue::open_record(
                        BTreeMap::from([(property.clone(), returned)]),
                        "unmodeled_factory_result_property",
                    ),
                    evidence: Some(origin),
                    choice: self.current_choice.clone(),
                    heap_id: None,
                };
            }
            returned
        };
        if let Some(key) = interning {
            self.interned_creations.insert(key, returned.clone());
        }
        returned
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
        // What a module that is not parsed holds stays tied to the module.
        if is_unparsed_module_value(&object) {
            return object;
        }
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
            // `value?.property` on a missing alternative is `undefined`; a plain read throws
            // instead, so what it gives is never used. Optional chains read like plain ones. A
            // missing value on its own stays unknown, since it often stands for one set later.
            return TrackedValue::plain(AbstractValue::union(
                values
                    .iter()
                    .cloned()
                    .map(|value| {
                        if matches!(value.value, AbstractValue::Null | AbstractValue::Undefined) {
                            TrackedValue::plain(AbstractValue::Undefined)
                        } else {
                            self.read_property(value, property, span.clone(), relation)
                        }
                    })
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
        let Some(mut value) = fields.get(property).cloned() else {
            // Prototype members are not modeled, so they stay unknown even on a closed record.
            if fields.open.is_none() && !is_object_prototype_member(property) {
                return TrackedValue::plain(AbstractValue::Undefined);
            }
            // A literal whose only unknown keys are computed holds one of their values or none.
            if fields.open.as_deref() == Some(COMPUTED_KEY) && !is_object_prototype_member(property)
            {
                let mut values = fields.unkeyed.clone();
                values.push(TrackedValue::plain(AbstractValue::Undefined));
                return TrackedValue::plain(AbstractValue::union(values));
            }
            self.mark_value_unresolved(&object, &format!("unknown property {property}"), span);
            return TrackedValue::unknown(format!("unknown_property:{property}"));
        };
        if !fields.unkeyed.is_empty() {
            // A computed key may have replaced the value.
            let mut values = vec![value];
            values.extend(fields.unkeyed.iter().cloned());
            value = TrackedValue::plain(AbstractValue::union(values));
        }
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
            self.clear_heap();
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
                let pending = self.pending_callbacks.len();
                let returned = self.call_function(key, arguments);
                self.render_unreached(returned, &candidate.span);
                self.run_unreached_callbacks(pending, &candidate.span);
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
                    let pending = self.pending_callbacks.len();
                    let returned = self.invoke_value(callable, arguments, candidate.span.clone());
                    if exported && returns_capability_data(&returned) {
                        self.capability_producer_files.insert(candidate.file_id);
                    }
                    self.render_unreached(returned, &candidate.span);
                    self.run_unreached_callbacks(pending, &candidate.span);
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

    /// Runs an unreached callsite's uncalled callbacks with their own render budget, so a large
    /// subtree rendered first does not leave none for the callsite's own handlers.
    fn run_unreached_callbacks(&mut self, pending: usize, span: &SourceSpan) {
        self.unreached_render_evaluations = 0;
        self.unreached_render_budget_reported = false;
        self.unreached_render_seed_span = Some(span.clone());
        self.unreached_render_budget_active = true;
        self.run_uncalled_callbacks(pending);
        self.unreached_render_budget_active = false;
        self.unreached_render_seed_span = None;
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
                self.clear_heap();
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
            self.clear_heap();
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
                    file_id,
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
                    | FlowStatement::Throw { span, .. }
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
                            file_id,
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
        self.note_uncertainty(reason, &span);
        let ids = self.values_capability_ids(values);
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
        self.values_capability_ids(std::iter::once(value))
    }

    /// Capabilities reachable from values, including through module globals of namespaces they
    /// hold. Namespaces are gathered first so the module graph and globals are walked once.
    fn values_capability_ids<'b>(
        &self,
        values: impl Iterator<Item = &'b TrackedValue>,
    ) -> Vec<usize> {
        if self.capabilities.is_empty() {
            return Vec::new();
        }
        let mut ids = Vec::new();
        let mut namespaces = BTreeSet::new();
        let mut visited = std::collections::HashSet::new();
        for value in values {
            collect_capability_ids_once(value, &mut ids, &mut namespaces, &mut visited);
        }
        if !namespaces.is_empty() {
            let mut modules = BTreeSet::new();
            for namespace in namespaces {
                self.module_initialization_order(namespace, &mut modules, &mut Vec::new());
            }
            for (symbol, value) in &self.globals {
                if modules.contains(&symbol.file_id) {
                    collect_capability_ids(value, &mut ids);
                }
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

    /// Boundaries that matter for the report: those possible creations depend on, and those an
    /// exact path reaches while handing them content to render. Most consequential first.
    fn component_boundaries(&self, query: &QuerySpec) -> Vec<QueryComponentBoundary> {
        use sha2::{Digest, Sha256};
        let mut counts = BTreeMap::<&str, (usize, usize)>::new();
        for capability in &self.capabilities {
            if !matches!(
                capability.reachability,
                Reachability::Possible | Reachability::Declared
            ) || (!query.report.include_non_invoked && capability.invocations.is_empty())
            {
                continue;
            }
            let sole = capability.boundaries.len() == 1;
            for boundary in &capability.boundaries {
                let entry = counts.entry(boundary).or_default();
                entry.0 += 1;
                entry.1 += usize::from(sole);
            }
        }
        let mut boundaries = self
            .boundaries
            .iter()
            .filter_map(|(key, state)| {
                let (affected, sole) = counts.get(key.as_ref()).copied().unwrap_or_default();
                let renders_content = state.forward_children
                    || state.invoke_children
                    || !state.render_props.is_empty()
                    || !state.component_props.is_empty()
                    || state.wrapper.is_some();
                (affected > 0 || (state.entered_from_reachable && renders_content)).then(|| {
                    QueryComponentBoundary {
                        boundary_id: format!(
                            "B{}",
                            &hex::encode(Sha256::digest(key.as_bytes()))[..16]
                        ),
                        kind: state.kind,
                        component: state.component.clone(),
                        module: state.module.clone(),
                        export: state.export.clone(),
                        reason: state.reason.clone(),
                        entered_from_reachable: state.entered_from_reachable,
                        site_count: state.site_keys.len(),
                        sites: state
                            .sites
                            .iter()
                            .filter_map(|span| self.query_location(span))
                            .collect(),
                        affected_creations: affected,
                        sole_blocker_creations: sole,
                        suggested_contract: state.suggested_contract(),
                    }
                })
            })
            .collect::<Vec<_>>();
        boundaries.sort_by(|left, right| {
            right
                .entered_from_reachable
                .cmp(&left.entered_from_reachable)
                .then(
                    right
                        .sole_blocker_creations
                        .cmp(&left.sole_blocker_creations),
                )
                .then(right.affected_creations.cmp(&left.affected_creations))
                .then_with(|| left.component.cmp(&right.component))
        });
        boundaries
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
                            other_paths: invocation.other_paths,
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
        let component_boundaries = self.component_boundaries(query);
        QueryReport {
            schema_version: 11,
            snapshot_id: self.snapshot.snapshot_id.clone(),
            config_hash: self.snapshot.config_hash.clone(),
            query_hash: query_hash.to_owned(),
            query_id: query.id.clone(),
            kind: query.kind,
            scope: query.scope,
            callsites: self.callsite_values,
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
            component_boundaries,
            unreached_callsites: self.unreached_callsites,
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

    /// Sources of the JSX a value carries. Elements and closures count with the JSX in their
    /// props and captures, so a render callback such as `(context) => renderPortal(context,
    /// children)`, or an element such as `<Inner {...props} />`, built inside a shared wrapper is
    /// told apart by the children each use passes.
    fn collect_render_content_sites(
        &self,
        value: &TrackedValue,
        sites: &mut Vec<(u32, u32)>,
        depth: usize,
    ) {
        if depth > 8 {
            return;
        }
        match &value.value {
            AbstractValue::Element(element) => {
                sites.push((element.span.file_id.0, element.span.start));
                for value in element.props.values() {
                    self.collect_render_content_sites(value, sites, depth + 1);
                }
            }
            AbstractValue::Closure(closure) => {
                sites.push((closure.span.file_id.0, closure.span.start));
                let references = self.body_references(&closure.span, &closure.body);
                for name in &references.names {
                    if let Some(captured) = closure.environment.get(name) {
                        self.collect_render_content_sites(captured, sites, depth + 1);
                    }
                }
            }
            AbstractValue::Array(values) | AbstractValue::Union(values) => {
                for value in values.iter() {
                    self.collect_render_content_sites(value, sites, depth + 1);
                }
            }
            _ => {}
        }
    }

    /// Names a closure body references. A source span identifies one closure, and the IR does not
    /// change during a pass, so the walk is done once per closure.
    fn body_references(&self, span: &SourceSpan, body: &FlowArrowBody) -> Rc<ClosureReferences> {
        if let Some(references) = self.closure_references.borrow().get(span) {
            return Rc::clone(references);
        }
        let mut references = ClosureReferences::default();
        collect_body_references(body, &mut references);
        let references = Rc::new(references);
        self.closure_references
            .borrow_mut()
            .insert(span.clone(), Rc::clone(&references));
        references
    }

    fn query_location(&self, span: &SourceSpan) -> Option<QueryLocation> {
        if let Some(location) = self.locations.borrow().get(span) {
            return location.clone();
        }
        let location = self.compute_location(span);
        self.locations
            .borrow_mut()
            .insert(span.clone(), location.clone());
        location
    }

    fn compute_location(&self, span: &SourceSpan) -> Option<QueryLocation> {
        let file = self.symbol_linker.file(span.file_id)?;
        if !self.location_sources.borrow().contains_key(&span.file_id) {
            let source = LocationSource::shared(&file.path, &file.content_hash)?;
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

/// Hashes the parts of a value that can change what a component renders or invokes.
fn hash_value_fingerprint(
    heap: &BTreeMap<u64, Rc<AbstractValue>>,
    value: &TrackedValue,
    hasher: &mut std::collections::hash_map::DefaultHasher,
    depth: usize,
) -> bool {
    use std::hash::Hash;
    if depth > 12 {
        "<deep>".hash(hasher);
        return false;
    }
    // A heap-backed value is read through its current heap cell.
    let current = value
        .heap_id
        .and_then(|id| heap.get(&id))
        .map(|current| TrackedValue::plain((**current).clone()));
    let value = current.as_ref().unwrap_or(value);
    let mut complete = true;
    match &value.value {
        AbstractValue::Null => "null".hash(hasher),
        AbstractValue::Undefined => "undefined".hash(hasher),
        AbstractValue::String(value) => ("s", value).hash(hasher),
        AbstractValue::Number(value) => ("n", value).hash(hasher),
        AbstractValue::Boolean(value) => ("b", value).hash(hasher),
        AbstractValue::EnumMember {
            enum_name,
            member_name,
            ..
        } => ("e", enum_name, member_name).hash(hasher),
        AbstractValue::Record(fields) => {
            ("record", &fields.open).hash(hasher);
            for (name, value) in fields.iter() {
                name.hash(hasher);
                complete &= hash_value_fingerprint(heap, value, hasher, depth + 1);
            }
        }
        AbstractValue::Array(values) | AbstractValue::Union(values) => {
            ("list", values.len()).hash(hasher);
            for value in values.iter() {
                complete &= hash_value_fingerprint(heap, value, hasher, depth + 1);
            }
        }
        AbstractValue::Function(key) => ("fn", key.file_id.0, &key.name).hash(hasher),
        AbstractValue::Namespace(file_id) => ("ns", file_id.0).hash(hasher),
        AbstractValue::ModelFunction => "model".hash(hasher),
        AbstractValue::Closure(closure) => {
            ("closure", closure.file_id.0, closure.span.start).hash(hasher);
            for (name, value) in &closure.environment {
                name.hash(hasher);
                complete &= hash_value_fingerprint(heap, value, hasher, depth + 1);
            }
        }
        AbstractValue::Capability(id) => ("cap", id).hash(hasher),
        AbstractValue::Element(element) => {
            ("el", element.span.file_id.0, element.span.start).hash(hasher);
            complete &= hash_value_fingerprint(heap, &element.component, hasher, depth + 1);
            for (name, value) in &element.props {
                name.hash(hasher);
                complete &= hash_value_fingerprint(heap, value, hasher, depth + 1);
            }
        }
        AbstractValue::Intrinsic(name) => ("intrinsic", name).hash(hasher),
        AbstractValue::ConfiguredComponent(model) => {
            ("cfg", &model.module, &model.export).hash(hasher);
        }
        AbstractValue::Unknown(reason) => ("unknown", reason).hash(hasher),
        AbstractValue::AssumedWrapper { wrapped, .. } => {
            "wrapper".hash(hasher);
            for value in wrapped.iter() {
                complete &= hash_value_fingerprint(heap, value, hasher, depth + 1);
            }
        }
    }
    complete
}

/// Collects source sites of JSX elements and render functions carried by a prop value.
fn contains_element(value: &TrackedValue, depth: usize) -> bool {
    match &value.value {
        AbstractValue::Element(_) => true,
        AbstractValue::Array(values) | AbstractValue::Union(values) => {
            depth < 8
                && values
                    .iter()
                    .any(|value| contains_element(value, depth + 1))
        }
        _ => false,
    }
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

/// Array methods that change the array they are called on.
const ARRAY_MUTATORS: &[&str] = &[
    "push",
    "unshift",
    "splice",
    "pop",
    "shift",
    "sort",
    "reverse",
    "fill",
    "copyWithin",
];

/// A local name and the static properties read from it, as in `record.items`.
fn local_path(expression: &FlowExpression) -> Option<(String, Vec<String>)> {
    match &expression.kind {
        FlowExpressionKind::Identifier {
            name,
            module_binding: false,
        } => Some((name.clone(), Vec::new())),
        FlowExpressionKind::StaticMember { object, property } => {
            let (name, mut path) = local_path(object)?;
            path.push(property.clone());
            Some((name, path))
        }
        _ => None,
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

/// The distinct arrays a value may be. Joined branches often repeat the same array, so repeats
/// count once toward the budget.
fn finite_array_parts(value: &AbstractValue) -> Option<Vec<Vec<TrackedValue>>> {
    fn collect(
        value: &AbstractValue,
        parts: &mut Vec<Vec<TrackedValue>>,
        seen: &mut BTreeSet<u64>,
    ) -> bool {
        match value {
            AbstractValue::Array(elements) => {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                std::hash::Hash::hash(&elements.len(), &mut hasher);
                let complete = elements.iter().all(|element| {
                    hash_value_fingerprint(&BTreeMap::new(), element, &mut hasher, 0)
                });
                if complete && !seen.insert(std::hash::Hasher::finish(&hasher)) {
                    return true;
                }
                parts.push(elements.to_vec());
                parts.len() <= 64
            }
            AbstractValue::Union(values) => values
                .iter()
                .all(|value| collect(&value.value, parts, seen)),
            _ => false,
        }
    }
    let mut parts = Vec::new();
    collect(value, &mut parts, &mut BTreeSet::new()).then_some(parts)
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

#[derive(Clone, Default)]
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
                if let Some(key) = &field.computed {
                    collect_expression_references(key, references);
                }
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
        FlowExpressionKind::Arrow { params, body } => {
            for param in params {
                collect_pattern_references(param, references);
            }
            collect_body_references(body, references);
        }
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

/// Collects references made by defaults inside a binding pattern.
fn collect_pattern_references(pattern: &FlowPattern, references: &mut ClosureReferences) {
    match &pattern.kind {
        FlowPatternKind::Object { fields, rest } => {
            for field in fields {
                collect_pattern_references(&field.target, references);
            }
            if let Some(rest) = rest {
                collect_pattern_references(rest, references);
            }
        }
        FlowPatternKind::Array { elements } => {
            for element in elements.iter().flatten() {
                collect_pattern_references(element, references);
            }
        }
        FlowPatternKind::Default { target, default } => {
            collect_pattern_references(target, references);
            collect_expression_references(default, references);
        }
        FlowPatternKind::Identifier { .. } | FlowPatternKind::Unsupported { .. } => {}
    }
}

fn collect_statement_references(statement: &FlowStatement, references: &mut ClosureReferences) {
    match statement {
        FlowStatement::Bind(binding) => {
            collect_pattern_references(&binding.pattern, references);
            collect_expression_references(&binding.value, references);
        }
        FlowStatement::Expression { value, .. }
        | FlowStatement::Throw { value, .. }
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
            // `items.push(value)` adds to `items`.
            FlowStatement::Expression { value, .. } => {
                if let FlowExpressionKind::Call { callee, arguments } = &value.kind
                    && let FlowExpressionKind::StaticMember { object, property } = &callee.kind
                    && ARRAY_MUTATORS.contains(&property.as_str())
                    && local_path(object).is_some_and(|(name, _)| names.contains(&name))
                {
                    for argument in arguments {
                        let mut references = ClosureReferences::default();
                        collect_expression_references(argument, &mut references);
                        names.extend(references.names);
                    }
                }
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

/// Whether a value holds any capability, stopping at the first one.
fn contains_capability(value: &TrackedValue) -> bool {
    fn walk(value: &TrackedValue, visited: &mut std::collections::HashSet<usize>) -> bool {
        match &value.value {
            AbstractValue::Capability(_) => true,
            AbstractValue::Record(fields) => {
                visited.insert(Rc::as_ptr(fields).addr())
                    && fields.values().any(|field| walk(field, visited))
            }
            AbstractValue::Array(elements) | AbstractValue::Union(elements) => {
                visited.insert(Rc::as_ptr(elements).addr())
                    && elements.iter().any(|element| walk(element, visited))
            }
            AbstractValue::Closure(closure) => {
                visited.insert(Rc::as_ptr(closure).addr())
                    && closure
                        .environment
                        .values()
                        .any(|captured| walk(captured, visited))
            }
            AbstractValue::Element(element) => {
                visited.insert(Rc::as_ptr(element).addr())
                    && (walk(&element.component, visited)
                        || element.props.values().any(|prop| walk(prop, visited)))
            }
            _ => false,
        }
    }
    walk(value, &mut std::collections::HashSet::new())
}

fn collect_capability_ids(value: &TrackedValue, ids: &mut Vec<usize>) {
    collect_capability_ids_once(
        value,
        ids,
        &mut BTreeSet::new(),
        &mut std::collections::HashSet::new(),
    );
}

/// Walks shared records, arrays, closures, and elements once each, since environments and
/// props often reference the same values many times.
fn collect_capability_ids_once(
    value: &TrackedValue,
    ids: &mut Vec<usize>,
    namespaces: &mut BTreeSet<FileId>,
    visited: &mut std::collections::HashSet<usize>,
) {
    match &value.value {
        AbstractValue::Capability(id) => ids.push(*id),
        AbstractValue::Namespace(file_id) => {
            namespaces.insert(*file_id);
        }
        AbstractValue::Record(fields) if visited.insert(Rc::as_ptr(fields).addr()) => {
            for field in fields.values() {
                collect_capability_ids_once(field, ids, namespaces, visited);
            }
        }
        AbstractValue::Array(elements) | AbstractValue::Union(elements)
            if visited.insert(Rc::as_ptr(elements).addr()) =>
        {
            for element in elements.iter() {
                collect_capability_ids_once(element, ids, namespaces, visited);
            }
        }
        AbstractValue::Closure(closure) if visited.insert(Rc::as_ptr(closure).addr()) => {
            for captured in closure.environment.values() {
                collect_capability_ids_once(captured, ids, namespaces, visited);
            }
        }
        AbstractValue::Element(element) if visited.insert(Rc::as_ptr(element).addr()) => {
            collect_capability_ids_once(&element.component, ids, namespaces, visited);
            for prop in element.props.values() {
                collect_capability_ids_once(prop, ids, namespaces, visited);
            }
        }
        _ => {}
    }
}

fn returns_capability_data(value: &TrackedValue) -> bool {
    match &value.value {
        AbstractValue::Capability(_) | AbstractValue::Closure(_) => contains_capability(value),
        AbstractValue::Record(fields) => fields.values().any(returns_capability_data),
        AbstractValue::Array(values) | AbstractValue::Union(values) => {
            values.iter().any(returns_capability_data)
        }
        _ => false,
    }
}

/// Copies a spread value's properties into a record literal under construction. A spread of a
/// value without known properties makes the record open, and properties set before it may have
/// been overwritten.
fn spread_into_record(record: &mut RecordFields, value: &TrackedValue) {
    match &value.value {
        AbstractValue::Null | AbstractValue::Undefined => {}
        AbstractValue::Record(fields) => {
            if let Some(reason) = &fields.open
                && reason != COMPUTED_KEY
            {
                widen_record_for_unknown_spread(record, reason);
            }
            record.extend(
                fields
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone())),
            );
            for value in &fields.unkeyed {
                add_unkeyed_value(record, value.clone());
            }
        }
        AbstractValue::Union(alternatives) => {
            // Each alternative spreads into its own copy; the record keeps every outcome.
            let before = record.clone();
            let outcomes = alternatives
                .iter()
                .map(|alternative| {
                    let mut outcome = before.clone();
                    spread_into_record(&mut outcome, alternative);
                    outcome
                })
                .collect::<Vec<_>>();
            let names = outcomes
                .iter()
                .flat_map(|outcome| outcome.keys().cloned())
                .collect::<BTreeSet<_>>();
            for name in names {
                let values = outcomes
                    .iter()
                    .map(|outcome| {
                        outcome
                            .get(&name)
                            .cloned()
                            .unwrap_or_else(|| TrackedValue::plain(AbstractValue::Undefined))
                    })
                    .collect::<Vec<_>>();
                let value = if values.iter().all(|value| same_value(value, &values[0])) {
                    values[0].clone()
                } else {
                    TrackedValue::plain(AbstractValue::union(values))
                };
                record.insert(name, value);
            }
            record.unkeyed = outcomes
                .iter()
                .flat_map(|outcome| outcome.unkeyed.iter().cloned())
                .collect();
            // A reason other than an unknown computed key wins, since it leaves more unknown.
            record.open = outcomes
                .iter()
                .filter_map(|outcome| outcome.open.clone())
                .max_by_key(|reason| reason != COMPUTED_KEY);
        }
        _ => widen_record_for_unknown_spread(record, "object_spread_of_unknown_value"),
    }
}

/// Starts the reason of a value imported from a module that is not parsed; the module specifier
/// follows.
const UNPARSED_MODULE: &str = "unparsed_module:";

fn is_unparsed_module_value(value: &TrackedValue) -> bool {
    matches!(&value.value, AbstractValue::Unknown(reason) if reason.starts_with(UNPARSED_MODULE))
}

/// Why a record literal is open when a computed key was not known.
const COMPUTED_KEY: &str = "computed_object_key";

/// The most values a read with an unknown key gathers from one object.
const MAX_ANY_PROPERTY_VALUES: usize = 64;

/// Adds `[key]: value` for a key that is not known: any property may now hold `value`.
fn add_unkeyed_value(record: &mut RecordFields, value: TrackedValue) {
    record.unkeyed.push(value);
    if record.open.is_none() {
        record.open = Some(COMPUTED_KEY.to_owned());
    }
}

/// The property name a key value selects.
fn property_key(value: &AbstractValue) -> Option<String> {
    match value {
        AbstractValue::String(value) => Some(value.clone()),
        AbstractValue::Number(value) | AbstractValue::EnumMember { value, .. } => {
            Some(value.to_string())
        }
        AbstractValue::Boolean(value) => Some(value.to_string()),
        AbstractValue::Null => Some("null".to_owned()),
        AbstractValue::Undefined => Some("undefined".to_owned()),
        _ => None,
    }
}

/// The property names a key may select, when each alternative is known.
fn property_keys(value: &AbstractValue) -> Option<Vec<String>> {
    fn collect(value: &AbstractValue, keys: &mut Vec<String>) -> bool {
        if let AbstractValue::Union(values) = value {
            return values.iter().all(|value| collect(&value.value, keys));
        }
        let Some(key) = property_key(value) else {
            return false;
        };
        if !keys.contains(&key) {
            keys.push(key);
        }
        keys.len() <= MAX_ANY_PROPERTY_VALUES
    }
    let mut keys = Vec::new();
    (collect(value, &mut keys) && !keys.is_empty()).then_some(keys)
}

fn widen_record_for_unknown_spread(record: &mut RecordFields, reason: &str) {
    for value in record.values_mut() {
        *value = TrackedValue::plain(AbstractValue::union(vec![
            value.clone(),
            TrackedValue::unknown(reason),
        ]));
    }
    record.open = Some(reason.to_owned());
}

/// Whether two alternatives are the same tracked value, so a union is not needed.
fn same_value(left: &TrackedValue, right: &TrackedValue) -> bool {
    let fingerprint = |value: &TrackedValue| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let complete = hash_value_fingerprint(&BTreeMap::new(), value, &mut hasher, 0);
        complete.then(|| std::hash::Hasher::finish(&hasher))
    };
    left.evidence == right.evidence
        && left.heap_id == right.heap_id
        && fingerprint(left).is_some_and(|left| Some(left) == fingerprint(right))
}

/// Properties every object inherits from `Object.prototype`.
fn is_object_prototype_member(property: &str) -> bool {
    matches!(
        property,
        "constructor"
            | "hasOwnProperty"
            | "isPrototypeOf"
            | "propertyIsEnumerable"
            | "toLocaleString"
            | "toString"
            | "valueOf"
            | "__proto__"
            | "__defineGetter__"
            | "__defineSetter__"
            | "__lookupGetter__"
            | "__lookupSetter__"
    )
}

enum Undefinedness {
    Never,
    Always,
}

/// Whether a value is `undefined`: `None` when it may or may not be.
fn undefined_alternatives(value: &TrackedValue) -> Option<Undefinedness> {
    match &value.value {
        AbstractValue::Undefined => Some(Undefinedness::Always),
        AbstractValue::Unknown(_) | AbstractValue::AssumedWrapper { .. } => None,
        AbstractValue::Union(values) => {
            let mut kinds = values.iter().map(undefined_alternatives);
            let first = kinds.next().flatten()?;
            kinds
                .all(|kind| {
                    matches!(
                        (&first, kind),
                        (Undefinedness::Never, Some(Undefinedness::Never))
                            | (Undefinedness::Always, Some(Undefinedness::Always))
                    )
                })
                .then_some(first)
        }
        _ => Some(Undefinedness::Never),
    }
}

/// Collects a value's alternatives other than `undefined`.
fn collect_defined_alternatives(value: &TrackedValue, alternatives: &mut Vec<TrackedValue>) {
    match &value.value {
        AbstractValue::Undefined => {}
        AbstractValue::Union(values) => {
            for value in values.iter() {
                collect_defined_alternatives(value, alternatives);
            }
        }
        _ => alternatives.push(value.clone()),
    }
}

/// Collects children as React's `Children` helpers see them: nested arrays are flattened and
/// `null`, `undefined`, and booleans are skipped. Returns whether the list is exact.
fn flatten_children(value: &TrackedValue, children: &mut Vec<TrackedValue>) -> bool {
    match &value.value {
        AbstractValue::Null | AbstractValue::Undefined | AbstractValue::Boolean(_) => true,
        AbstractValue::Array(values) => {
            // Every element is collected, so the walk must not stop at an inexact one.
            let mut exact = true;
            for value in values.iter() {
                exact &= flatten_children(value, children);
            }
            exact
        }
        AbstractValue::Union(_)
        | AbstractValue::Unknown(_)
        | AbstractValue::AssumedWrapper { .. } => {
            children.push(value.clone());
            false
        }
        _ => {
            children.push(value.clone());
            true
        }
    }
}

/// Calls that run a callback argument while the caller renders, so uses inside it are direct.
fn callee_runs_callback_now(callee: &str) -> bool {
    let method = callee.rsplit('.').next().unwrap_or(callee);
    matches!(
        method,
        "useMemo"
            | "useState"
            | "memo"
            | "forwardRef"
            | "map"
            | "flatMap"
            | "filter"
            | "forEach"
            | "reduce"
            | "find"
            | "some"
            | "every"
            | "only"
            | "toArray"
    )
}

fn callee_text(callee: &FlowExpression) -> String {
    match &callee.kind {
        FlowExpressionKind::Identifier { name, .. } => name.clone(),
        FlowExpressionKind::StaticMember { object, property } => {
            format!("{}.{property}", callee_text(object))
        }
        _ => "call".to_owned(),
    }
}

/// The context for code nested in `inner`: the outermost non-direct context wins, since that is
/// where exploration has to enter.
fn nested_context(outer: &UseContext, inner: UseContext) -> UseContext {
    if matches!(outer, UseContext::Direct) {
        inner
    } else {
        outer.clone()
    }
}

fn collect_uses_in_statements<'e>(
    statements: &'e [FlowStatement],
    context: &UseContext,
    site: Option<&SourceSpan>,
    uses: &mut Vec<UseSite<'e>>,
) {
    for statement in statements {
        match statement {
            FlowStatement::Bind(binding) => collect_uses(&binding.value, context, site, uses),
            FlowStatement::Expression { value, .. }
            | FlowStatement::Throw { value, .. }
            | FlowStatement::Assign { value, .. }
            | FlowStatement::Return {
                value: Some(value), ..
            } => collect_uses(value, context, site, uses),
            FlowStatement::If {
                test,
                consequent,
                alternate,
                ..
            } => {
                collect_uses(test, context, site, uses);
                collect_uses_in_statements(consequent, context, site, uses);
                collect_uses_in_statements(alternate, context, site, uses);
            }
            FlowStatement::Return { value: None, .. } | FlowStatement::Unsupported(_) => {}
        }
    }
}

fn collect_uses_in_body<'e>(
    body: &'e FlowArrowBody,
    context: &UseContext,
    site: Option<&SourceSpan>,
    uses: &mut Vec<UseSite<'e>>,
) {
    match body {
        FlowArrowBody::Expression { expression } => collect_uses(expression, context, site, uses),
        FlowArrowBody::Statements { statements } => {
            collect_uses_in_statements(statements, context, site, uses);
        }
    }
}

/// Collects references to bindings, attributed to the nearest enclosing call or JSX site.
fn collect_uses<'e>(
    expression: &'e FlowExpression,
    context: &UseContext,
    site: Option<&SourceSpan>,
    uses: &mut Vec<UseSite<'e>>,
) {
    let at = site.unwrap_or(&expression.span);
    let record = |name: &'e str, site: &SourceSpan, uses: &mut Vec<UseSite<'e>>| {
        uses.push(UseSite {
            name: Some(name),
            module: None,
            site: site.clone(),
            context: context.clone(),
        });
    };
    match &expression.kind {
        FlowExpressionKind::Identifier { name, .. } => record(name, at, uses),
        FlowExpressionKind::Call { callee, arguments } => {
            let call = &expression.span;
            match &callee.kind {
                FlowExpressionKind::Identifier { name, .. } => record(name, call, uses),
                FlowExpressionKind::StaticMember { object, .. } => {
                    collect_uses(object, context, Some(call), uses);
                }
                _ => collect_uses(callee, context, Some(call), uses),
            }
            let callee = callee_text(callee);
            for argument in arguments {
                if let FlowExpressionKind::Arrow { body, .. } = &argument.kind {
                    let inner = if callee_runs_callback_now(&callee) {
                        context.clone()
                    } else {
                        nested_context(context, UseContext::CallArgument(callee.clone()))
                    };
                    collect_uses_in_body(body, &inner, None, uses);
                } else {
                    collect_uses(argument, context, Some(call), uses);
                }
            }
        }
        FlowExpressionKind::JsxElement { tag, props } => {
            let element = &expression.span;
            match tag {
                FlowJsxTag::Identifier {
                    name,
                    intrinsic: false,
                    ..
                }
                | FlowJsxTag::Member { object: name, .. } => record(name, element, uses),
                _ => {}
            }
            for prop in props {
                match prop {
                    FlowJsxProp::Property { name, value, .. } => {
                        if let FlowExpressionKind::Arrow { body, .. } = &value.kind {
                            let inner =
                                nested_context(context, UseContext::PropCallback(name.clone()));
                            collect_uses_in_body(body, &inner, None, uses);
                        } else {
                            collect_uses(value, context, Some(element), uses);
                        }
                    }
                    FlowJsxProp::Spread { value, .. } => {
                        collect_uses(value, context, Some(element), uses);
                    }
                    FlowJsxProp::Unsupported(_) => {}
                }
            }
        }
        FlowExpressionKind::Arrow { body, .. } => {
            collect_uses_in_body(
                body,
                &nested_context(context, UseContext::LocalCallback),
                None,
                uses,
            );
        }
        FlowExpressionKind::DynamicImport { module } => uses.push(UseSite {
            name: None,
            module: Some(module),
            site: at.clone(),
            context: nested_context(context, UseContext::LazyImport),
        }),
        FlowExpressionKind::Record { fields } => {
            for field in fields {
                collect_uses(&field.value, context, site, uses);
            }
        }
        FlowExpressionKind::Array { elements } => {
            for element in elements {
                collect_uses(element, context, site, uses);
            }
        }
        FlowExpressionKind::Spread { value }
        | FlowExpressionKind::LogicalNot { value }
        | FlowExpressionKind::LooseNullEquality { value, .. } => {
            collect_uses(value, context, site, uses);
        }
        FlowExpressionKind::StaticMember { object, .. } => {
            collect_uses(object, context, site, uses);
        }
        FlowExpressionKind::ComputedMember { object, property } => {
            collect_uses(object, context, site, uses);
            collect_uses(property, context, site, uses);
        }
        FlowExpressionKind::StrictEquality { left, right, .. }
        | FlowExpressionKind::Logical { left, right, .. } => {
            collect_uses(left, context, site, uses);
            collect_uses(right, context, site, uses);
        }
        FlowExpressionKind::Conditional {
            test,
            consequent,
            alternate,
        } => {
            collect_uses(test, context, site, uses);
            collect_uses(consequent, context, site, uses);
            collect_uses(alternate, context, site, uses);
        }
        FlowExpressionKind::Unsupported { references, .. } => {
            for name in references {
                record(name, at, uses);
            }
        }
        FlowExpressionKind::Null
        | FlowExpressionKind::String { .. }
        | FlowExpressionKind::Number { .. }
        | FlowExpressionKind::NumericEnumMember { .. }
        | FlowExpressionKind::Boolean { .. } => {}
    }
}

/// How an unmodeled component's props would map onto a consumer contract: children to forward,
/// a function child to invoke, render-function props, and component props.
fn contract_shape(
    props: &RecordFields,
    is_component: impl Fn(&TrackedValue) -> bool,
) -> (bool, bool, BTreeSet<String>, BTreeSet<String>) {
    let mut forward = false;
    let mut invoke = false;
    let mut render_props = BTreeSet::new();
    let mut component_props = BTreeSet::new();
    for (name, value) in props {
        let callable = matches!(
            value.value,
            AbstractValue::Closure(_) | AbstractValue::Function(_)
        );
        if name == "children" {
            if callable {
                invoke = true;
            } else if contains_element(value, 0) {
                forward = true;
            }
        } else if matches!(&value.value, AbstractValue::Closure(closure) if arrow_body_renders_jsx(&closure.body))
        {
            render_props.insert(name.clone());
        } else if callable && is_component(value) {
            component_props.insert(name.clone());
        }
    }
    (forward, invoke, render_props, component_props)
}

/// The value a path that throws returns.
const THROWN_EXCEPTION: &str = "thrown_exception";

fn is_thrown(value: &TrackedValue) -> bool {
    matches!(&value.value, AbstractValue::Unknown(reason) if reason == THROWN_EXCEPTION)
}

/// The local binding a call goes through: `name(...)` or `name.member(...)`.
fn imported_callee_local(callee: &FlowExpression) -> Option<&str> {
    match &callee.kind {
        FlowExpressionKind::Identifier { name, .. } => Some(name),
        FlowExpressionKind::StaticMember { object, .. } => match &object.kind {
            FlowExpressionKind::Identifier { name, .. } => Some(name),
            _ => None,
        },
        _ => None,
    }
}

/// Strings and other scalars, such as JSX text children, cannot render components.
const fn is_primitive(value: &AbstractValue) -> bool {
    matches!(
        value,
        AbstractValue::Null
            | AbstractValue::Undefined
            | AbstractValue::String(_)
            | AbstractValue::Number(_)
            | AbstractValue::Boolean(_)
            | AbstractValue::EnumMember { .. }
    )
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
        // Each alternative projects on its own, so known ones stay visible beside unknown ones.
        AbstractValue::Union(values) => {
            let mut distinct = Vec::new();
            for value in values.iter().map(query_value) {
                let nested = match value {
                    QueryValue::Alternatives { values } => values,
                    value => vec![value],
                };
                for value in nested {
                    if !distinct.contains(&value) {
                        distinct.push(value);
                    }
                }
            }
            if distinct.len() == 1 {
                distinct.pop().expect("one alternative")
            } else {
                QueryValue::Alternatives { values: distinct }
            }
        }
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

/// Whether a known array holds a value, compared like `===`, when every comparison is decided
/// or one element matches.
fn array_membership(receiver: &TrackedValue, needle: &TrackedValue) -> Option<bool> {
    if let AbstractValue::Union(needles) = &needle.value {
        return same_known(
            needles
                .iter()
                .map(|needle| array_membership(receiver, needle)),
        );
    }
    match &receiver.value {
        AbstractValue::Array(elements) => {
            let mut undecided = false;
            for element in elements.iter() {
                match exact_equality(&element.value, &needle.value) {
                    Some(true) => return Some(true),
                    Some(false) => {}
                    None => undecided = true,
                }
            }
            (!undecided).then_some(false)
        }
        AbstractValue::Union(receivers) => same_known(
            receivers
                .iter()
                .map(|receiver| array_membership(receiver, needle)),
        ),
        _ => None,
    }
}

/// An identifier or a chain of property reads, which evaluates the same each time.
fn is_plain_read(expression: &FlowExpression) -> bool {
    match &expression.kind {
        FlowExpressionKind::Identifier { .. } => true,
        FlowExpressionKind::StaticMember { object, .. } => is_plain_read(object),
        _ => false,
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
            let wants_nullish = expected != *negated;
            narrow_binding(environment, name, |value| {
                nullish(value).is_none_or(|is_nullish| is_nullish == wants_nullish)
            });
        }
        FlowExpressionKind::Identifier { name, .. } => {
            narrow_binding(environment, name, |value| {
                truthy(value).is_none_or(|truthy| truthy == expected)
            });
        }
        _ => {}
    }
}

/// Keeps the alternatives of a local that a condition allows, looking through nested choices.
fn narrow_binding(
    environment: &mut Environment,
    name: &str,
    keep: impl Fn(&AbstractValue) -> bool,
) {
    fn leaves(value: &TrackedValue, out: &mut Vec<TrackedValue>) {
        if let AbstractValue::Union(values) = &value.value {
            for value in values.iter() {
                leaves(value, out);
            }
        } else {
            out.push(value.clone());
        }
    }
    let Some(binding) = environment.get_mut(name) else {
        return;
    };
    if !matches!(binding.value, AbstractValue::Union(_)) {
        return;
    }
    let mut alternatives = Vec::new();
    leaves(binding, &mut alternatives);
    let mut narrowed = alternatives
        .into_iter()
        .filter(|alternative| keep(&alternative.value))
        .collect::<Vec<_>>();
    if narrowed.len() == 1 {
        binding.value = narrowed.remove(0).value;
    } else if !narrowed.is_empty() {
        binding.value = AbstractValue::union(narrowed);
    }
}

fn truthy(value: &AbstractValue) -> Option<bool> {
    match value {
        AbstractValue::Null | AbstractValue::Undefined => Some(false),
        AbstractValue::Boolean(value) => Some(*value),
        AbstractValue::Number(value) => Some(*value != 0),
        AbstractValue::EnumMember { value, .. } => Some(*value != 0),
        AbstractValue::String(value) => Some(!value.is_empty()),
        AbstractValue::Unknown(reason) if reason == FALSY_OPERAND => Some(false),
        AbstractValue::Unknown(reason) if reason == TRUTHY_OPERAND => Some(true),
        AbstractValue::Unknown(_) | AbstractValue::AssumedWrapper { .. } => None,
        AbstractValue::Union(values) => same_known(values.iter().map(|value| truthy(&value.value))),
        _ => Some(true),
    }
}

/// The left operand of `&&` when it is the result: an unknown value known to be falsy.
const FALSY_OPERAND: &str = "falsy_operand";
/// The left operand of `||` when it is the result: an unknown value known to be truthy.
const TRUTHY_OPERAND: &str = "truthy_operand";

fn same_known(values: impl Iterator<Item = Option<bool>>) -> Option<bool> {
    let mut values = values;
    let first = values.next()??;
    values.all(|value| value == Some(first)).then_some(first)
}

fn value_is_uncertain(value: &TrackedValue) -> bool {
    match &value.value {
        AbstractValue::Unknown(_) | AbstractValue::AssumedWrapper { .. } => true,
        // Finite alternatives, such as arrays built in branches or a string chosen by a
        // condition, are known values.
        AbstractValue::Union(values) => {
            !(finite_array_alternatives(value) || literal_alternatives(value))
                || values.iter().any(value_is_uncertain)
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

/// Alternatives that are all known literals, such as the members of a string enum, which
/// project to their strings.
fn literal_alternatives(value: &TrackedValue) -> bool {
    match &value.value {
        AbstractValue::String(_)
        | AbstractValue::Number(_)
        | AbstractValue::Boolean(_)
        | AbstractValue::Null
        | AbstractValue::Undefined
        | AbstractValue::EnumMember { .. } => true,
        AbstractValue::Union(values) => values.iter().all(literal_alternatives),
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
        FlowStatement::Bind(FlowBinding { value, .. })
        | FlowStatement::Expression { value, .. }
        | FlowStatement::Throw { value, .. }
        | FlowStatement::Assign { value, .. } => {
            collect_import_uses(value, imported_names, uses);
        }
        FlowStatement::Return { value, .. } => {
            if let Some(value) = value {
                collect_import_uses(value, imported_names, uses);
            }
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
                if let Some(key) = &field.computed {
                    collect_import_uses(key, imported_names, uses);
                }
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
        FlowStatement::Expression { value, .. } | FlowStatement::Throw { value, .. } => {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_arrays_count_once_toward_the_spread_budget() {
        let listed = TrackedValue::plain(AbstractValue::array(vec![TrackedValue::plain(
            AbstractValue::String("listed".to_owned()),
        )]));
        let empty = TrackedValue::plain(AbstractValue::array(Vec::new()));
        // Joined branches that never changed the array repeat it past the budget of 64.
        let alternatives = (0..40)
            .flat_map(|_| [listed.clone(), empty.clone()])
            .collect::<Vec<_>>();
        let parts = finite_array_parts(&AbstractValue::union(alternatives)).expect("finite parts");
        assert_eq!(parts.len(), 2);
    }
}
