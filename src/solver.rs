#![allow(clippy::needless_pass_by_value, clippy::too_many_lines)]

use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, bail};

use crate::{
    cache::Snapshot,
    evidence::{Evidence, RelationKind},
    ids::{EvidenceId, FileId},
    ir::{
        FlowArrowBody, FlowBinding, FlowExpression, FlowExpressionKind, FlowFunction, FlowJsxProp,
        FlowJsxTag, FlowPattern, FlowPatternKind, FlowStatement, SourceSpan,
    },
    models::{CallbackFactoryModel, CaptureSource, ModelEvidence, ModelValue, ModeledOperation},
    project::Project,
    queries::{AuditFinding, AuditReport, Conclusion, Coverage, FindingRef},
    query::{
        QueryCreation, QueryInvocation, QueryReport, QueryScope, QuerySpec, QueryValue,
        Reachability,
    },
};

const MAX_CALL_DEPTH: usize = 128;

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
struct ImportTarget {
    imported: String,
    module: String,
    resolved: Option<std::path::PathBuf>,
    type_only: bool,
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
    String(String),
    Record(BTreeMap<String, TrackedValue>),
    Array(Vec<TrackedValue>),
    Function(FunctionKey),
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
    callee: String,
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
) -> Result<QueryReport> {
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
    if project.config.entries.is_empty() {
        if query.scope == QueryScope::Reachable {
            bail!("a reachable query requires at least one configured entry point");
        }
        solver.prepare_globals();
    } else {
        solver.run()?;
    }
    if query.scope == QueryScope::AllCreations {
        solver.seed_unreached_creations();
    }
    Ok(solver.query_report(query, query_hash))
}

struct Solver<'a> {
    project: &'a Project,
    snapshot: &'a Snapshot,
    model: CallbackFactoryModel,
    functions: BTreeMap<FunctionKey, FunctionDef>,
    functions_by_name: BTreeMap<String, Vec<FunctionKey>>,
    imports: BTreeMap<(FileId, String), ImportTarget>,
    globals_ir: Vec<(FileId, FlowBinding)>,
    enum_globals: Environment,
    globals: Environment,
    evidence: Vec<Evidence>,
    capabilities: Vec<CapabilityState>,
    diagnostics: Vec<String>,
    coverage_gaps: Vec<String>,
    current_choice: Option<String>,
    current_reachability: Reachability,
    call_depth: usize,
}

impl<'a> Solver<'a> {
    fn new(
        project: &'a Project,
        snapshot: &'a Snapshot,
        model: CallbackFactoryModel,
    ) -> Result<Self> {
        let mut functions = BTreeMap::new();
        let mut functions_by_name: BTreeMap<String, Vec<FunctionKey>> = BTreeMap::new();
        let mut imports = BTreeMap::new();
        let mut globals_ir = Vec::new();
        let mut enum_globals = Environment::new();
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
                functions_by_name
                    .entry(function.name.clone())
                    .or_default()
                    .push(key.clone());
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

            for import in &file.flow.imports {
                let resolution = snapshot.resolutions.iter().find(|resolution| {
                    resolution.importer == file.path && resolution.specifier == import.module
                });
                imports.insert(
                    (file.file_id, import.local.clone()),
                    ImportTarget {
                        imported: import.imported.clone(),
                        module: import.module.clone(),
                        resolved: resolution
                            .and_then(|resolution| resolution.resolved_path.clone()),
                        type_only: import.type_only,
                    },
                );
            }

            let mut enums: BTreeMap<String, BTreeMap<String, TrackedValue>> = BTreeMap::new();
            for member in &file.enum_members {
                if let Some(value) = &member.string_value {
                    enums.entry(member.enum_name.clone()).or_default().insert(
                        member.member_name.clone(),
                        TrackedValue::plain(AbstractValue::String(value.clone())),
                    );
                }
            }
            for (name, members) in enums {
                enum_globals.insert(name, TrackedValue::plain(AbstractValue::Record(members)));
            }
        }

        for keys in functions_by_name.values_mut() {
            keys.sort();
        }

        if model.r#match.project != project.config.name {
            bail!(
                "model {} targets project {}, not {}",
                model.id,
                model.r#match.project,
                project.config.name
            );
        }

        Ok(Self {
            project,
            snapshot,
            model,
            functions,
            functions_by_name,
            imports,
            globals_ir,
            globals: enum_globals.clone(),
            enum_globals,
            evidence: Vec::new(),
            capabilities: Vec::new(),
            diagnostics,
            coverage_gaps: Vec::new(),
            current_choice: None,
            current_reachability: Reachability::Reachable,
            call_depth: 0,
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
            let entry_key = FunctionKey {
                file_id: entry_file.file_id,
                name: entry.export.clone(),
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
                self.prepare_globals();
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

    fn prepare_globals(&mut self) {
        self.globals = self.enum_globals.clone();
        for (file_id, binding) in self.globals_ir.clone() {
            let value = self.eval(&binding.value, &self.globals.clone(), file_id);
            let mut updated = self.globals.clone();
            self.bind_pattern(
                &binding.pattern,
                value,
                &mut updated,
                RelationKind::ValueTransfer,
            );
            self.globals = updated;
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
        let mut environment = self.globals.clone();
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
        let returned = self.execute_statements(
            &definition.function.body,
            &mut environment,
            definition.key.file_id,
        );
        self.call_depth -= 1;
        returned.unwrap_or_else(|| TrackedValue::plain(AbstractValue::Undefined))
    }

    fn execute_statements(
        &mut self,
        statements: &[FlowStatement],
        environment: &mut Environment,
        file_id: FileId,
    ) -> Option<TrackedValue> {
        for statement in statements {
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
                    self.eval(value, environment, file_id);
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
                FlowStatement::Unsupported(unsupported) => {
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
        match &expression.kind {
            FlowExpressionKind::String { value } => {
                TrackedValue::plain(AbstractValue::String(value.clone()))
            }
            FlowExpressionKind::Identifier { name } => {
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
                if self.identifier_matches_model(file_id, name) {
                    return TrackedValue::plain(AbstractValue::ModelFunction);
                }
                match self.functions_by_name.get(name) {
                    Some(keys) if keys.len() == 1 => {
                        TrackedValue::plain(AbstractValue::Function(keys[0].clone()))
                    }
                    Some(_) => TrackedValue::unknown(format!("ambiguous_function:{name}")),
                    None => TrackedValue::unknown(format!("unresolved_identifier:{name}")),
                }
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
            FlowExpressionKind::Array { elements } => TrackedValue::plain(AbstractValue::Array(
                elements
                    .iter()
                    .map(|element| self.eval(element, environment, file_id))
                    .collect(),
            )),
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
                let AbstractValue::String(property_name) = &property.value else {
                    self.mark_value_unresolved(
                        &object,
                        "computed property is not a finite string",
                        expression.span.clone(),
                    );
                    return TrackedValue::unknown("unknown_computed_property");
                };
                self.read_property(
                    object,
                    property_name,
                    expression.span.clone(),
                    RelationKind::KeySelection,
                )
            }
            FlowExpressionKind::Call { callee, arguments } => {
                let callee = self.eval(callee, environment, file_id);
                let arguments = arguments
                    .iter()
                    .map(|argument| self.eval(argument, environment, file_id))
                    .collect::<Vec<_>>();
                match callee.value {
                    AbstractValue::ModelFunction => {
                        self.call_model(&arguments, expression.span.clone())
                    }
                    AbstractValue::Capability(capability) => {
                        let evidence = self.push_evidence(
                            RelationKind::Invocation,
                            "invoke_capability",
                            expression.span.clone(),
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
                        self.emit_modeled_effects(capability, evidence, expression.span.clone());
                        TrackedValue::plain(AbstractValue::Undefined)
                    }
                    AbstractValue::Closure(closure) => self.call_closure(&closure, arguments),
                    AbstractValue::Function(key) => self.call_function(&key, arguments),
                    AbstractValue::Unknown(reason) => {
                        self.mark_values_unresolved(
                            arguments.iter(),
                            &format!("call through unknown target: {reason}"),
                            expression.span.clone(),
                        );
                        TrackedValue::unknown("unknown_call_result")
                    }
                    _ => {
                        self.mark_values_unresolved(
                            arguments.iter(),
                            "value passed to unsupported call target",
                            expression.span.clone(),
                        );
                        TrackedValue::unknown("unsupported_call_target")
                    }
                }
            }
            FlowExpressionKind::Arrow { params, body } => {
                let mut parents = environment
                    .values()
                    .filter(|value| !capability_ids(value).is_empty())
                    .filter_map(|value| value.evidence)
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
                        environment: environment.clone(),
                        file_id,
                    }),
                    evidence,
                    choice: self.current_choice.clone(),
                }
            }
            FlowExpressionKind::JsxElement { tag, props } => {
                self.create_element(tag, props, expression, environment, file_id)
            }
            FlowExpressionKind::Unsupported { syntax } => {
                self.mark_values_unresolved(
                    environment.values(),
                    &format!("unsupported expression: {syntax}"),
                    expression.span.clone(),
                );
                TrackedValue::unknown(syntax.clone())
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
            } => TrackedValue::plain(AbstractValue::Intrinsic(name.clone())),
            FlowJsxTag::Identifier {
                name,
                intrinsic: false,
            } => self.eval(
                &FlowExpression {
                    kind: FlowExpressionKind::Identifier { name: name.clone() },
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
        TrackedValue::plain(AbstractValue::Element(ElementValue {
            component: Box::new(component),
            props: values,
            span: expression.span.clone(),
        }))
    }

    fn render(&mut self, value: TrackedValue) {
        let AbstractValue::Element(element) = value.value else {
            return;
        };
        match &element.component.value {
            AbstractValue::Function(key) => {
                let argument = TrackedValue {
                    value: AbstractValue::Record(element.props),
                    evidence: element.component.evidence,
                    choice: element.component.choice.clone(),
                };
                let returned = self.call_function(key, vec![argument]);
                self.render(returned);
            }
            AbstractValue::Intrinsic(name) => {
                for (prop_name, handler) in element.props {
                    if prop_name == "onClick" {
                        self.register_and_explore_handler(name, &handler, &element.span);
                    }
                }
            }
            AbstractValue::Unknown(reason) => self.mark_values_unresolved(
                element.props.values(),
                &format!("element has unknown component target: {reason}"),
                element.span,
            ),
            _ => self.mark_values_unresolved(
                element.props.values(),
                "element target is not a component",
                element.span,
            ),
        }
    }

    fn register_and_explore_handler(
        &mut self,
        intrinsic: &str,
        handler: &TrackedValue,
        element_span: &SourceSpan,
    ) {
        let capability_ids = capability_ids(handler);
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
        for (index, pattern) in closure.params.iter().enumerate() {
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
        match &closure.body {
            FlowArrowBody::Expression { expression } => {
                self.eval(expression, &environment, closure.file_id)
            }
            FlowArrowBody::Statements { statements } => self
                .execute_statements(statements, &mut environment, closure.file_id)
                .unwrap_or_else(|| TrackedValue::plain(AbstractValue::Undefined)),
        }
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

    fn import_matches_model(&self, file_id: FileId, local: &str) -> bool {
        let Some(import) = self.imports.get(&(file_id, local.to_owned())) else {
            return false;
        };
        if import.type_only || import.imported != self.model.r#match.export {
            return false;
        }
        let expected = self
            .project
            .resolve_path(Path::new(&self.model.r#match.module));
        match (&import.resolved, expected.canonicalize()) {
            (Some(resolved), Ok(expected)) => *resolved == expected,
            _ => {
                import.module.trim_start_matches("./")
                    == Path::new(&self.model.r#match.module)
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .unwrap_or_default()
            }
        }
    }

    fn local_matches_model(&self, file_id: FileId, name: &str) -> bool {
        if name != self.model.r#match.export {
            return false;
        }
        let Some(file) = self
            .snapshot
            .files
            .iter()
            .find(|file| file.file_id == file_id)
        else {
            return false;
        };
        let expected = self
            .project
            .resolve_path(Path::new(&self.model.r#match.module));
        expected
            .canonicalize()
            .is_ok_and(|expected| file.path == expected)
    }

    fn identifier_matches_model(&self, file_id: FileId, name: &str) -> bool {
        self.import_matches_model(file_id, name) || self.local_matches_model(file_id, name)
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
            if !self.identifier_matches_model(candidate.file_id, &candidate.callee) {
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
            }
            if !self
                .capabilities
                .iter()
                .any(|capability| capability.callsite == candidate.span)
            {
                let environment = self.globals.clone();
                let arguments = candidate
                    .arguments
                    .iter()
                    .map(|argument| self.eval(argument, &environment, candidate.file_id))
                    .collect::<Vec<_>>();
                self.call_model(&arguments, candidate.span.clone());
            }
            self.record_unreached_gap(&candidate.span);
        }
        self.current_choice = None;
        self.current_reachability = Reachability::Reachable;
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
        let mut ids = values.flat_map(capability_ids).collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        for capability in ids {
            let evidence = self.push_evidence(
                RelationKind::UnresolvedEscape,
                "unsupported_reachable_operation",
                span.clone(),
                Vec::new(),
                None,
                reason,
            );
            if let Some(state) = self.capabilities.get_mut(capability) {
                state.unresolved.push(evidence);
            }
        }
    }

    fn mark_value_unresolved(&mut self, value: &TrackedValue, reason: &str, span: SourceSpan) {
        self.mark_values_unresolved(std::iter::once(value), reason, span);
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
                    reachability: capability.reachability,
                    choice: capability.choice.clone(),
                    factory_arguments,
                    capability_path: query.capability.returned_property.clone(),
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
            schema_version: 1,
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
}

fn capability_ids(value: &TrackedValue) -> Vec<usize> {
    let mut ids = Vec::new();
    collect_capability_ids(value, &mut ids);
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn collect_capability_ids(value: &TrackedValue, ids: &mut Vec<usize>) {
    match &value.value {
        AbstractValue::Capability(id) => ids.push(*id),
        AbstractValue::Record(fields) => {
            for field in fields.values() {
                collect_capability_ids(field, ids);
            }
        }
        AbstractValue::Array(elements) => {
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

fn render_compact_value(value: &TrackedValue) -> String {
    match &value.value {
        AbstractValue::String(value) => value.clone(),
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
        _ => "<symbolic>".to_owned(),
    }
}

fn query_value(value: &TrackedValue) -> QueryValue {
    match &value.value {
        AbstractValue::String(value) => QueryValue::String {
            value: value.clone(),
        },
        AbstractValue::Array(elements) => QueryValue::Array {
            elements: elements.iter().map(query_value).collect(),
        },
        AbstractValue::Undefined => QueryValue::Undefined,
        AbstractValue::Unknown(reason) => QueryValue::Unknown {
            reason: reason.clone(),
        },
        AbstractValue::Record(_) => QueryValue::Unknown {
            reason: "record_value".to_owned(),
        },
        AbstractValue::Function(_)
        | AbstractValue::ModelFunction
        | AbstractValue::Closure(_)
        | AbstractValue::Capability(_)
        | AbstractValue::Element(_)
        | AbstractValue::Intrinsic(_) => QueryValue::Unknown {
            reason: "non_data_value".to_owned(),
        },
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
            if let FlowExpressionKind::Identifier { name } = &callee.kind {
                candidates.push(FactoryCallCandidate {
                    file_id,
                    callee: name.clone(),
                    span: expression.span.clone(),
                    arguments: arguments.clone(),
                    enclosing_function: enclosing_function.cloned(),
                });
            }
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
        FlowExpressionKind::StaticMember { object, .. } => {
            collect_factory_calls(object, file_id, enclosing_function, candidates);
        }
        FlowExpressionKind::ComputedMember { object, property } => {
            collect_factory_calls(object, file_id, enclosing_function, candidates);
            collect_factory_calls(property, file_id, enclosing_function, candidates);
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
        FlowExpressionKind::String { .. }
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
        FlowStatement::Unsupported(_) => {}
    }
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
