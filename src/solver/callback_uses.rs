//! Follows the selected factory result from each callsite to the calls made with it. The walk is
//! over the source, not over explored paths, so a call is reported with its arguments whether or
//! not an explored path executed it; explored invocations fill in arguments that depend on
//! context.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    rc::Rc,
};

use crate::{
    ids::FileId,
    ir::{
        FlowArrowBody, FlowAssignmentTarget, FlowExpression, FlowExpressionKind, FlowJsxProp,
        FlowJsxTag, FlowPattern, FlowPatternKind, FlowStatement, SourceSpan,
    },
    link::{LinkedSymbol, LinkedValue, ValueResolution, pattern_names},
    query::{
        QueryCallerValue, QueryCallsiteValues, QueryCapabilityCall, QueryCapabilityEscape,
        QueryCapabilityStatus, QueryCapabilityUse, QuerySpec, QueryValue, Reachability,
    },
};

use super::{
    Environment, FactoryCallCandidate, FunctionKey, Solver, TrackedValue, UseEdge, UseNode,
    callee_text, query_value,
};

/// Scopes and targets one callsite's walk may visit.
const WALK_LIMIT: usize = 400;
/// Scope changes from the callsite to a call.
const MAX_HOPS: usize = 8;
/// Callers whose arguments are evaluated for one unresolved factory argument.
const MAX_CALLERS: usize = 32;

/// A property name, or `[index]` for an array element, on the way from a value to the factory
/// result inside it.
type Step = String;

/// What the walk follows in one scope.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum Target {
    /// A binding, with the steps from its value to the factory result.
    Name(String, Vec<Step>),
    /// The expression at this span in the scope's file, with the steps from its value to the
    /// factory result. With a callee name, only a call to that name matches, since a use site
    /// can also be a call that receives the name as an argument.
    Site(u32, u32, Option<String>, Vec<Step>),
}

/// How an expression is used by the expression or statement that contains it.
#[derive(Clone)]
enum Role<'e> {
    /// A condition, an operand, or a discarded value.
    Other,
    Bind(&'e FlowPattern),
    Return,
    Assign(&'e FlowAssignmentTarget),
    Callee,
    Argument(usize),
    Prop(&'e str),
    PropSpread,
    Field(&'e str),
    FieldSpread,
    Element(usize),
    /// The object of a property read with this key, if the key is known.
    Member(Option<Step>),
    /// A value the parent passes on, such as a branch of `?:` or an operand of `??`.
    Through,
    /// The expression body of an arrow.
    ArrowBody,
}

#[derive(Clone)]
struct Frame<'e> {
    expression: &'e FlowExpression,
    role: Role<'e>,
}

/// What an occurrence of the followed value leads to.
enum Lead<'e> {
    Call {
        call: &'e FlowExpression,
        arguments: &'e [FlowExpression],
        context: Vec<String>,
        /// The frame of the innermost function around the call.
        arrow: Option<usize>,
    },
    /// Passed as an event handler to an intrinsic element, which calls it with an event.
    Handler {
        span: SourceSpan,
        prop: String,
        context: Vec<String>,
    },
    Same(Target),
    Enter {
        scopes: Vec<UseNode>,
        parameter: usize,
        steps: Vec<Step>,
        hop: String,
    },
    Callers(Vec<Step>),
    Escape {
        span: SourceSpan,
        detail: String,
        context: Vec<String>,
    },
    /// A use that is not a call, such as a comparison.
    Used,
    /// A read of something else in the value that holds the result.
    Unrelated,
}

/// A call found by the walk, before its arguments are evaluated.
struct FoundCall {
    span: SourceSpan,
    /// Each projected invocation argument, in projection order; `None` for the event an
    /// intrinsic element passes.
    arguments: Vec<Option<WrittenArgument>>,
    context: Vec<String>,
    via: Vec<String>,
    /// The call of the result inside the first wrapper this call goes through.
    inner: Option<SourceSpan>,
    /// Whether this call passes a wrapper's parameter on, so calls of the wrapper replace it.
    forwards: bool,
}

/// An argument as written at a call, with where to evaluate it.
#[derive(Clone)]
struct WrittenArgument {
    file_id: FileId,
    /// `None` when the call omits the argument and no default applies.
    expression: Option<FlowExpression>,
    /// Names bound in the scope that holds the argument; reading them depends on context.
    locals: Rc<BTreeSet<String>>,
    /// Whether the argument is a parameter of the function around the call.
    parameter: bool,
}

/// Where a projected invocation argument is in a call to the followed value. A wrapper such as
/// `(kind) => apply(kind)` moves it to the wrapper's parameter.
#[derive(Clone)]
enum ArgumentSource {
    /// The argument at this position, or the parameter's default when a call omits it.
    Position(usize, Option<WrittenArgument>),
    /// A value the wrapper writes itself.
    Written(WrittenArgument),
}

/// How calls of the followed value map to calls of the factory result.
struct Forward {
    sources: Vec<ArgumentSource>,
    /// The call of the result inside the first wrapper, where explored invocations are.
    inner: Option<SourceSpan>,
}

/// An occurrence's outcome in the walk loop.
enum Found {
    Call(FoundCall),
    /// A lead, the argument mapping it carries, and a hop to add to the path.
    Lead(OwnedLead, Rc<Forward>, Option<String>),
}

struct Walk {
    calls: Vec<FoundCall>,
    escapes: Vec<(SourceSpan, String, Vec<String>, Vec<String>)>,
    /// Whether the result was used anywhere after the factory callsite binds it.
    used: bool,
}

/// The parameters and body of a scope the walk can enter.
struct ScopeCode<'a> {
    file_id: FileId,
    params: &'a [FlowPattern],
    body: ScopeBody<'a>,
    name: String,
    /// Whether the scope is a function; a module binding may hold another value.
    callable: bool,
}

enum ScopeBody<'a> {
    Statements(&'a [FlowStatement]),
    Expression(&'a FlowExpression),
}

/// Hooks whose later arguments are dependency lists rather than values they use.
fn takes_dependencies(callee: &str) -> bool {
    let name = callee.rsplit('.').next().unwrap_or(callee);
    matches!(
        name,
        "useEffect"
            | "useLayoutEffect"
            | "useInsertionEffect"
            | "useCallback"
            | "useMemo"
            | "useImperativeHandle"
    )
}

/// Calls that return their first argument.
fn returns_first_argument(callee: &str) -> bool {
    let name = callee.rsplit('.').next().unwrap_or(callee);
    matches!(name, "useCallback" | "useEvent" | "useEffectEvent")
}

/// Calls that return what their first argument, a function, returns.
fn returns_callback_result(callee: &str) -> bool {
    let name = callee.rsplit('.').next().unwrap_or(callee);
    name == "useMemo"
}

/// The steps from the factory call's value to the selected result.
fn initial_steps(query: &QuerySpec) -> Vec<Step> {
    query.capability.returned_index.map_or_else(
        || query.capability.returned_property.clone(),
        |index| vec![format!("[{index}]")],
    )
}

/// The targets a pattern binds for a value whose result is at `steps`.
fn bind_targets(pattern: &FlowPattern, steps: &[Step], targets: &mut Vec<Target>) {
    match &pattern.kind {
        FlowPatternKind::Identifier { name } => {
            targets.push(Target::Name(name.clone(), steps.to_vec()));
        }
        FlowPatternKind::Default { target, .. } => bind_targets(target, steps, targets),
        FlowPatternKind::Object { fields, rest } => {
            let Some(first) = steps.first() else {
                return;
            };
            if let Some(field) = fields.iter().find(|field| &field.source_property == first) {
                bind_targets(&field.target, &steps[1..], targets);
            } else if let Some(rest) = rest {
                bind_targets(rest, steps, targets);
            }
        }
        FlowPatternKind::Array { elements } => {
            let Some(index) = steps
                .first()
                .and_then(|step| step.strip_prefix('['))
                .and_then(|step| step.strip_suffix(']'))
                .and_then(|index| index.parse::<usize>().ok())
            else {
                return;
            };
            if let Some(Some(element)) = elements.get(index) {
                bind_targets(element, &steps[1..], targets);
            }
        }
        FlowPatternKind::Unsupported { .. } => {}
    }
}

fn collect_pattern_locals(pattern: &FlowPattern, locals: &mut BTreeSet<String>) {
    locals.extend(pattern_names(pattern).into_iter().map(str::to_owned));
}

fn collect_statement_locals(statements: &[FlowStatement], locals: &mut BTreeSet<String>) {
    for statement in statements {
        match statement {
            FlowStatement::Bind(binding) => {
                collect_pattern_locals(&binding.pattern, locals);
                collect_expression_locals(&binding.value, locals);
            }
            FlowStatement::Return {
                value: Some(value), ..
            }
            | FlowStatement::Expression { value, .. }
            | FlowStatement::Assign { value, .. }
            | FlowStatement::Throw { value, .. } => collect_expression_locals(value, locals),
            FlowStatement::If {
                consequent,
                alternate,
                ..
            } => {
                collect_statement_locals(consequent, locals);
                collect_statement_locals(alternate, locals);
            }
            FlowStatement::Return { value: None, .. } | FlowStatement::Unsupported(_) => {}
        }
    }
}

/// Names bound by arrows nested in an expression, such as handler parameters.
fn collect_expression_locals(expression: &FlowExpression, locals: &mut BTreeSet<String>) {
    let mut children = Vec::new();
    expression_children(expression, &mut children);
    if let FlowExpressionKind::Arrow { params, body } = &expression.kind {
        for param in params {
            collect_pattern_locals(param, locals);
        }
        if let FlowArrowBody::Statements { statements } = body {
            collect_statement_locals(statements, locals);
        }
    }
    for child in children {
        collect_expression_locals(child, locals);
    }
}

/// The direct subexpressions of an expression, except an arrow's statements.
fn expression_children<'e>(expression: &'e FlowExpression, children: &mut Vec<&'e FlowExpression>) {
    match &expression.kind {
        FlowExpressionKind::Record { fields } => {
            children.extend(fields.iter().map(|field| &field.value));
        }
        FlowExpressionKind::Array { elements } => children.extend(elements),
        FlowExpressionKind::Spread { value }
        | FlowExpressionKind::LogicalNot { value }
        | FlowExpressionKind::LooseNullEquality { value, .. } => children.push(value),
        FlowExpressionKind::StaticMember { object, .. } => children.push(object),
        FlowExpressionKind::ComputedMember { object, property } => {
            children.push(object);
            children.push(property);
        }
        FlowExpressionKind::Call { callee, arguments } => {
            children.push(callee);
            children.extend(arguments);
        }
        FlowExpressionKind::StrictEquality { left, right, .. }
        | FlowExpressionKind::Logical { left, right, .. } => {
            children.push(left);
            children.push(right);
        }
        FlowExpressionKind::Conditional {
            test,
            consequent,
            alternate,
        } => {
            children.push(test);
            children.push(consequent);
            children.push(alternate);
        }
        FlowExpressionKind::Arrow {
            body: FlowArrowBody::Expression { expression },
            ..
        } => children.push(expression),
        FlowExpressionKind::JsxElement { props, .. } => {
            for prop in props {
                match prop {
                    FlowJsxProp::Property { value, .. } | FlowJsxProp::Spread { value, .. } => {
                        children.push(value);
                    }
                    FlowJsxProp::Unsupported(_) => {}
                }
            }
        }
        _ => {}
    }
}

/// Whether evaluating an expression without its local scope gives its value: it has no calls,
/// functions, or JSX, and reads no local names.
fn is_context_free(expression: &FlowExpression, locals: &BTreeSet<String>) -> bool {
    match &expression.kind {
        FlowExpressionKind::Identifier { name, .. } => !locals.contains(name),
        FlowExpressionKind::Call { .. }
        | FlowExpressionKind::Arrow { .. }
        | FlowExpressionKind::JsxElement { .. }
        | FlowExpressionKind::DynamicImport { .. }
        | FlowExpressionKind::Unsupported { .. } => false,
        _ => {
            let mut children = Vec::new();
            expression_children(expression, &mut children);
            children
                .into_iter()
                .all(|child| is_context_free(child, locals))
        }
    }
}

fn tag_text(tag: &FlowJsxTag) -> String {
    match tag {
        FlowJsxTag::Identifier { name, .. } => name.clone(),
        FlowJsxTag::Member {
            object, property, ..
        } => format!("{object}.{property}"),
        FlowJsxTag::Unsupported { syntax } => syntax.clone(),
    }
}

/// Describes the function an arrow frame starts, from how its parent uses it.
fn describe_arrow(frames: &[Frame<'_>], index: usize) -> String {
    let parent = index.checked_sub(1).map(|parent| frames[parent].expression);
    match (&frames[index].role, parent.map(|parent| &parent.kind)) {
        (Role::Prop(name), Some(FlowExpressionKind::JsxElement { tag, .. })) => {
            format!("the {name} prop of <{}>", tag_text(tag))
        }
        (Role::Argument(_), Some(FlowExpressionKind::Call { callee, .. })) => {
            format!("the callback passed to {}", callee_text(callee))
        }
        (Role::Field(name), _) => format!("the {name} property"),
        (Role::Bind(pattern), _) => pattern_names(pattern)
            .first()
            .map_or_else(|| "a local function".to_owned(), |name| (*name).to_owned()),
        _ => "a nested function".to_owned(),
    }
}

/// The functions enclosing frame `index`, innermost first.
fn context_of(frames: &[Frame<'_>], index: usize) -> Vec<String> {
    (0..index)
        .rev()
        .filter(|&frame| {
            matches!(
                frames[frame].expression.kind,
                FlowExpressionKind::Arrow { .. }
            )
        })
        .map(|frame| describe_arrow(frames, frame))
        .collect()
}

/// Calls `found` for every expression in statements, with the frames from the statement down.
fn visit_statements<'e>(
    statements: &'e [FlowStatement],
    frames: &mut Vec<Frame<'e>>,
    found: &mut dyn FnMut(&[Frame<'e>]),
) {
    for statement in statements {
        match statement {
            FlowStatement::Bind(binding) => {
                visit(&binding.value, Role::Bind(&binding.pattern), frames, found);
            }
            FlowStatement::Return {
                value: Some(value), ..
            } => visit(value, Role::Return, frames, found),
            FlowStatement::Expression { value, .. } | FlowStatement::Throw { value, .. } => {
                visit(value, Role::Other, frames, found);
            }
            FlowStatement::Assign { target, value, .. } => {
                visit(value, Role::Assign(target), frames, found);
                match target {
                    FlowAssignmentTarget::StaticMember { object, .. } => {
                        visit(object, Role::Other, frames, found);
                    }
                    FlowAssignmentTarget::ComputedMember { object, property } => {
                        visit(object, Role::Other, frames, found);
                        visit(property, Role::Other, frames, found);
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
                visit(test, Role::Other, frames, found);
                visit_statements(consequent, frames, found);
                visit_statements(alternate, frames, found);
            }
            FlowStatement::Return { value: None, .. } | FlowStatement::Unsupported(_) => {}
        }
    }
}

fn visit<'e>(
    expression: &'e FlowExpression,
    role: Role<'e>,
    frames: &mut Vec<Frame<'e>>,
    found: &mut dyn FnMut(&[Frame<'e>]),
) {
    frames.push(Frame { expression, role });
    found(frames);
    match &expression.kind {
        FlowExpressionKind::Record { fields } => {
            for field in fields {
                let role = if field.spread {
                    Role::FieldSpread
                } else {
                    Role::Field(&field.property)
                };
                visit(&field.value, role, frames, found);
            }
        }
        FlowExpressionKind::Array { elements } => {
            for (index, element) in elements.iter().enumerate() {
                visit(element, Role::Element(index), frames, found);
            }
        }
        FlowExpressionKind::Spread { value } => visit(value, Role::Through, frames, found),
        FlowExpressionKind::LogicalNot { value }
        | FlowExpressionKind::LooseNullEquality { value, .. } => {
            visit(value, Role::Other, frames, found);
        }
        FlowExpressionKind::StaticMember { object, property } => {
            visit(object, Role::Member(Some(property.clone())), frames, found);
        }
        FlowExpressionKind::ComputedMember { object, property } => {
            let key = match &property.kind {
                FlowExpressionKind::Number { value } => Some(format!("[{value}]")),
                FlowExpressionKind::String { value } => Some(value.clone()),
                _ => None,
            };
            visit(object, Role::Member(key), frames, found);
            visit(property, Role::Other, frames, found);
        }
        FlowExpressionKind::Call { callee, arguments } => {
            visit(callee, Role::Callee, frames, found);
            for (index, argument) in arguments.iter().enumerate() {
                visit(argument, Role::Argument(index), frames, found);
            }
        }
        FlowExpressionKind::StrictEquality { left, right, .. } => {
            visit(left, Role::Other, frames, found);
            visit(right, Role::Other, frames, found);
        }
        FlowExpressionKind::Logical { left, right, .. } => {
            visit(left, Role::Through, frames, found);
            visit(right, Role::Through, frames, found);
        }
        FlowExpressionKind::Conditional {
            test,
            consequent,
            alternate,
        } => {
            visit(test, Role::Other, frames, found);
            visit(consequent, Role::Through, frames, found);
            visit(alternate, Role::Through, frames, found);
        }
        FlowExpressionKind::Arrow { body, .. } => match body {
            FlowArrowBody::Expression { expression } => {
                visit(expression, Role::ArrowBody, frames, found);
            }
            FlowArrowBody::Statements { statements } => visit_statements(statements, frames, found),
        },
        FlowExpressionKind::JsxElement { props, .. } => {
            for prop in props {
                match prop {
                    FlowJsxProp::Property { name, value, .. } => {
                        visit(value, Role::Prop(name), frames, found);
                    }
                    FlowJsxProp::Spread { value, .. } => {
                        visit(value, Role::PropSpread, frames, found);
                    }
                    FlowJsxProp::Unsupported(_) => {}
                }
            }
        }
        _ => {}
    }
    frames.pop();
}

impl Solver<'_> {
    /// For each matching factory callsite, the values it was called with and the calls made with
    /// its selected result.
    pub(super) fn callsite_values(&mut self, query: &QuerySpec) -> Vec<QueryCallsiteValues> {
        let candidates = self
            .factory_candidates()
            .into_iter()
            .filter(|candidate| self.expression_matches_model(candidate.file_id, &candidate.callee))
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Vec::new();
        }
        let graph = self.use_graph();
        let mut results = Vec::new();
        for candidate in &candidates {
            let walk = self.walk_capability(candidate, query, &graph);
            results.push(self.summarize_callsite(candidate, query, walk, &graph));
        }
        results
    }

    /// The scope that holds a factory callsite.
    fn candidate_scope(&self, candidate: &FactoryCallCandidate) -> Option<UseNode> {
        candidate.enclosing_function.clone().map_or_else(
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
        )
    }

    fn scope_code(&self, scope: &UseNode) -> Option<ScopeCode<'_>> {
        match scope {
            UseNode::Function(key) => {
                let function = self.functions.get(key)?;
                Some(ScopeCode {
                    file_id: key.file_id,
                    params: &function.params,
                    body: ScopeBody::Statements(&function.body),
                    name: key.name.clone(),
                    callable: true,
                })
            }
            UseNode::Global(index) => {
                let (file_id, binding) = self.globals_ir.get(*index)?;
                let name = pattern_names(&binding.pattern)
                    .first()
                    .map_or_else(String::new, |name| (*name).to_owned());
                let mut value = &binding.value;
                // A component wrapped by `memo`, `forwardRef`, or a similar call.
                if let FlowExpressionKind::Call { arguments, .. } = &value.kind
                    && let Some(first) = arguments.first()
                    && matches!(first.kind, FlowExpressionKind::Arrow { .. })
                {
                    value = first;
                }
                match &value.kind {
                    FlowExpressionKind::Arrow { params, body } => Some(ScopeCode {
                        file_id: *file_id,
                        params,
                        body: match body {
                            FlowArrowBody::Statements { statements } => {
                                ScopeBody::Statements(statements)
                            }
                            FlowArrowBody::Expression { expression } => {
                                ScopeBody::Expression(expression)
                            }
                        },
                        name,
                        callable: true,
                    }),
                    _ => Some(ScopeCode {
                        file_id: *file_id,
                        params: &[],
                        body: ScopeBody::Expression(value),
                        name,
                        callable: false,
                    }),
                }
            }
        }
    }

    /// The scopes a linked declaration runs as: a function with any class methods that share
    /// its props, or a module binding.
    fn declaration_scopes(&self, symbol: &LinkedSymbol) -> Vec<UseNode> {
        let key = FunctionKey {
            file_id: symbol.file_id,
            name: symbol.name.clone(),
        };
        if self.functions.contains_key(&key) {
            let prefix = format!("{}.", key.name);
            let mut scopes = vec![UseNode::Function(key)];
            scopes.extend(
                self.functions
                    .keys()
                    .filter(|other| {
                        other.file_id == symbol.file_id && other.name.starts_with(&prefix)
                    })
                    .cloned()
                    .map(UseNode::Function),
            );
            return scopes;
        }
        let Some(&index) = self.global_bindings.get(symbol) else {
            return Vec::new();
        };
        self.lazy_scopes(index)
            .unwrap_or_else(|| vec![UseNode::Global(index)])
    }

    /// For a module binding built by a loader, such as `lazy(() => import('./Panel'))` or
    /// `load({ promise: () => import('./Panel') })`, the scopes of the component it loads.
    fn lazy_scopes(&self, index: usize) -> Option<Vec<UseNode>> {
        let (file_id, binding) = self.globals_ir.get(index)?;
        let FlowExpressionKind::Call { arguments, .. } = &binding.value.kind else {
            return None;
        };
        let (module, export) = arguments.iter().find_map(|argument| match &argument.kind {
            FlowExpressionKind::Arrow { .. } => loaded_module(argument),
            FlowExpressionKind::Record { fields } => {
                fields.iter().find_map(|field| loaded_module(&field.value))
            }
            _ => None,
        })?;
        let file = self.symbol_linker.file(*file_id)?;
        let path = self
            .symbol_linker
            .import_resolutions(&file.path)
            .filter(|resolution| resolution.specifier == module)
            .find_map(|resolution| resolution.resolved_path.as_ref())?;
        let target = self.symbol_linker.file_at(path)?;
        match self
            .symbol_linker
            .resolve_exported_value(target.file_id, &export)
        {
            ValueResolution::Resolved(LinkedValue::Declaration(symbol)) => {
                let scopes = self.declaration_scopes(&symbol);
                (!scopes.is_empty()).then_some(scopes)
            }
            _ => None,
        }
    }

    /// The scopes a name, or a namespace member, refers to from a file.
    fn resolve_scopes(&self, file_id: FileId, name: &str, member: Option<&str>) -> Vec<UseNode> {
        match (self.symbol_linker.resolve_binding(file_id, name), member) {
            (ValueResolution::Resolved(LinkedValue::Declaration(symbol)), None) => {
                self.declaration_scopes(&symbol)
            }
            (ValueResolution::Resolved(LinkedValue::Namespace(namespace)), Some(member)) => {
                match self.symbol_linker.resolve_exported_value(namespace, member) {
                    ValueResolution::Resolved(LinkedValue::Declaration(symbol)) => {
                        self.declaration_scopes(&symbol)
                    }
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }

    /// Follows an occurrence of the result at `frames[index]`, whose value holds the result at
    /// `steps`.
    fn follow<'e>(
        &self,
        file_id: FileId,
        frames: &[Frame<'e>],
        index: usize,
        steps: Vec<Step>,
    ) -> Lead<'e> {
        let frame = &frames[index];
        let parent = index.checked_sub(1).map(|parent| frames[parent].expression);
        match &frame.role {
            Role::Member(key) => match (steps.first(), key) {
                (Some(first), Some(key)) if first == key => {
                    self.follow(file_id, frames, index - 1, steps[1..].to_vec())
                }
                (Some(_), Some(_)) => Lead::Unrelated,
                _ => Lead::Used,
            },
            Role::Callee => match parent {
                Some(
                    call @ FlowExpression {
                        kind: FlowExpressionKind::Call { arguments, .. },
                        ..
                    },
                ) if steps.is_empty() => Lead::Call {
                    call,
                    arguments,
                    context: context_of(frames, index),
                    arrow: (0..index).rev().find(|&frame| {
                        matches!(
                            frames[frame].expression.kind,
                            FlowExpressionKind::Arrow { .. }
                        )
                    }),
                },
                _ => Lead::Unrelated,
            },
            Role::Argument(position) => {
                let Some(FlowExpression {
                    kind: FlowExpressionKind::Call { callee, .. },
                    span,
                }) = parent
                else {
                    return Lead::Used;
                };
                let text = callee_text(callee);
                if *position > 0 && takes_dependencies(&text) {
                    return Lead::Used;
                }
                if *position == 0 && returns_first_argument(&text) {
                    return self.follow(file_id, frames, index - 1, steps);
                }
                let scopes = match &callee.kind {
                    FlowExpressionKind::Identifier { name, .. } => {
                        self.resolve_scopes(file_id, name, None)
                    }
                    FlowExpressionKind::StaticMember { object, property } => match &object.kind {
                        FlowExpressionKind::Identifier { name, .. } => {
                            self.resolve_scopes(file_id, name, Some(property))
                        }
                        _ => Vec::new(),
                    },
                    _ => Vec::new(),
                };
                if scopes.is_empty() {
                    Lead::Escape {
                        span: span.clone(),
                        detail: format!("passed to {text}, which the walk does not follow"),
                        context: context_of(frames, index),
                    }
                } else {
                    Lead::Enter {
                        scopes,
                        parameter: *position,
                        steps,
                        hop: format!("argument {} of {text}", position + 1),
                    }
                }
            }
            Role::Prop(_) | Role::PropSpread => {
                let Some(FlowExpression {
                    kind: FlowExpressionKind::JsxElement { tag, .. },
                    span,
                }) = parent
                else {
                    return Lead::Used;
                };
                let prop = match &frame.role {
                    Role::Prop(name) => Some(*name),
                    _ => None,
                };
                let steps = prop.map_or(steps.clone(), |prop| {
                    std::iter::once(prop.to_owned())
                        .chain(steps.iter().cloned())
                        .collect()
                });
                let (scopes, intrinsic) = match tag {
                    FlowJsxTag::Identifier {
                        name,
                        intrinsic: true,
                        ..
                    } => (Vec::new(), Some(name.clone())),
                    FlowJsxTag::Identifier { name, .. } => {
                        (self.resolve_scopes(file_id, name, None), None)
                    }
                    FlowJsxTag::Member {
                        object, property, ..
                    } => (self.resolve_scopes(file_id, object, Some(property)), None),
                    FlowJsxTag::Unsupported { .. } => (Vec::new(), None),
                };
                if intrinsic.is_some() {
                    // An explored handler invocation is at the element, so the call is too.
                    return match (prop, steps.len()) {
                        (Some(prop), 1) if prop.starts_with("on") => Lead::Handler {
                            span: span.clone(),
                            prop: prop.to_owned(),
                            context: context_of(frames, index),
                        },
                        _ => Lead::Used,
                    };
                }
                let shown = prop.map_or_else(
                    || format!("spread props of <{}>", tag_text(tag)),
                    |prop| format!("prop {prop} of <{}>", tag_text(tag)),
                );
                if scopes.is_empty() {
                    Lead::Escape {
                        span: span.clone(),
                        detail: format!("{shown}, whose component the walk does not follow"),
                        context: context_of(frames, index),
                    }
                } else {
                    Lead::Enter {
                        scopes,
                        parameter: 0,
                        steps,
                        hop: shown,
                    }
                }
            }
            Role::Field(name) => self.follow(
                file_id,
                frames,
                index - 1,
                std::iter::once((*name).to_owned()).chain(steps).collect(),
            ),
            Role::Element(position) => self.follow(
                file_id,
                frames,
                index - 1,
                std::iter::once(format!("[{position}]"))
                    .chain(steps)
                    .collect(),
            ),
            Role::FieldSpread | Role::Through => self.follow(file_id, frames, index - 1, steps),
            Role::ArrowBody => self.follow_arrow_result(file_id, frames, index - 1, steps),
            Role::Return => {
                let arrow = (0..index).rev().find(|&frame| {
                    matches!(
                        frames[frame].expression.kind,
                        FlowExpressionKind::Arrow { .. }
                    )
                });
                match arrow {
                    Some(arrow) => self.follow_arrow_result(file_id, frames, arrow, steps),
                    None => Lead::Callers(steps),
                }
            }
            Role::Bind(pattern) => {
                // A pattern binds at most one name to the value at one step path.
                let mut targets = Vec::new();
                bind_targets(pattern, &steps, &mut targets);
                targets.pop().map_or(Lead::Unrelated, Lead::Same)
            }
            Role::Assign(target) => match target {
                FlowAssignmentTarget::Identifier { name } => {
                    Lead::Same(Target::Name(name.clone(), steps))
                }
                _ => Lead::Escape {
                    span: frame.expression.span.clone(),
                    detail: "stored in a property, which the walk does not follow".to_owned(),
                    context: context_of(frames, index),
                },
            },
            Role::Other => Lead::Used,
        }
    }

    /// Follows the value an arrow returns: only `useMemo` hands it on.
    fn follow_arrow_result<'e>(
        &self,
        file_id: FileId,
        frames: &[Frame<'e>],
        arrow: usize,
        steps: Vec<Step>,
    ) -> Lead<'e> {
        match (
            &frames[arrow].role,
            arrow
                .checked_sub(1)
                .map(|call| &frames[call].expression.kind),
        ) {
            (Role::Argument(0), Some(FlowExpressionKind::Call { callee, .. }))
                if returns_callback_result(&callee_text(callee)) =>
            {
                self.follow(file_id, frames, arrow - 1, steps)
            }
            _ => Lead::Used,
        }
    }

    /// Walks from a factory callsite to the calls made with its selected result.
    fn walk_capability(
        &self,
        candidate: &FactoryCallCandidate,
        query: &QuerySpec,
        graph: &HashMap<UseNode, Vec<UseEdge>>,
    ) -> Walk {
        let mut walk = Walk {
            calls: Vec::new(),
            escapes: Vec::new(),
            used: false,
        };
        let Some(start) = self.candidate_scope(candidate) else {
            return walk;
        };
        let direct = Rc::new(Forward {
            sources: query
                .capability
                .invocation_arguments
                .iter()
                .map(|projection| ArgumentSource::Position(projection.index, None))
                .collect(),
            inner: None,
        });
        let mut queue = VecDeque::from([(
            start,
            Target::Site(
                candidate.span.start,
                candidate.span.end,
                None,
                initial_steps(query),
            ),
            Vec::<String>::new(),
            direct,
        )]);
        let mut seen = HashSet::new();
        let mut locals_cache = HashMap::<UseNode, Rc<BTreeSet<String>>>::new();
        let mut recorded = HashSet::new();
        while let Some((scope, target, via, forward)) = queue.pop_front() {
            let inner = forward.inner.as_ref().map(span_key);
            if seen.len() >= WALK_LIMIT || !seen.insert((scope.clone(), target.clone(), inner)) {
                continue;
            }
            let Some(code) = self.scope_code(&scope) else {
                continue;
            };
            let locals = locals_cache
                .entry(scope.clone())
                .or_insert_with(|| Rc::new(self.scope_locals(&scope).unwrap_or_default()))
                .clone();
            let mut leads = Vec::new();
            let file_id = code.file_id;
            let mut found = |frames: &[Frame<'_>]| {
                let frame = frames.last().expect("visited frame");
                let expression = frame.expression;
                let steps = match (&target, &expression.kind) {
                    (
                        Target::Name(name, steps),
                        FlowExpressionKind::Identifier { name: seen, .. },
                    ) if seen == name => steps.clone(),
                    (Target::Site(start, end, callee, steps), kind)
                        if expression.span.start == *start
                            && expression.span.end == *end
                            && callee.as_ref().is_none_or(|callee| {
                                matches!(kind, FlowExpressionKind::Call { callee: called, .. }
                                    if callee_text(called).rsplit('.').next() == Some(callee))
                            }) =>
                    {
                        steps.clone()
                    }
                    _ => return,
                };
                let written = |expression: Option<&FlowExpression>| WrittenArgument {
                    file_id,
                    expression: expression.cloned(),
                    locals: locals.clone(),
                    parameter: false,
                };
                match self.follow(file_id, frames, frames.len() - 1, steps) {
                    Lead::Call {
                        call,
                        arguments,
                        context,
                        arrow,
                    } => {
                        let params = arrow.and_then(|arrow| match &frames[arrow].expression.kind {
                            FlowExpressionKind::Arrow { params, .. } => Some(params),
                            _ => None,
                        });
                        let mut sources = Vec::new();
                        let mut forwarded = false;
                        let mut values = Vec::new();
                        for source in &forward.sources {
                            let mut argument = match source {
                                ArgumentSource::Position(position, default) => {
                                    match (arguments.get(*position), default) {
                                        (Some(expression), _) => written(Some(expression)),
                                        (None, Some(default)) => default.clone(),
                                        (None, None) => written(None),
                                    }
                                }
                                ArgumentSource::Written(argument) => argument.clone(),
                            };
                            // A function that passes its own parameter on is a wrapper; calls of
                            // it give the value.
                            let parameter = match (&argument.expression, params) {
                                (
                                    Some(FlowExpression {
                                        kind: FlowExpressionKind::Identifier { name, .. },
                                        ..
                                    }),
                                    Some(params),
                                ) => parameter_position(params, name),
                                _ => None,
                            };
                            if let Some((position, default)) = parameter {
                                argument.parameter = true;
                                forwarded = true;
                                sources.push(ArgumentSource::Position(
                                    position,
                                    default.map(|default| written(Some(default))),
                                ));
                            } else {
                                sources.push(ArgumentSource::Written(argument.clone()));
                            }
                            values.push(Some(argument));
                        }
                        leads.push(Found::Call(FoundCall {
                            span: call.span.clone(),
                            arguments: values,
                            context,
                            via: via.clone(),
                            inner: forward.inner.clone(),
                            forwards: forwarded,
                        }));
                        if forwarded && let Some(arrow) = arrow {
                            let lead = self.follow(file_id, frames, arrow, Vec::new()).into_owned();
                            leads.push(Found::Lead(
                                lead,
                                Rc::new(Forward {
                                    sources,
                                    inner: Some(
                                        forward.inner.clone().unwrap_or_else(|| call.span.clone()),
                                    ),
                                }),
                                Some(format!("through {}", describe_arrow(frames, arrow))),
                            ));
                        }
                    }
                    Lead::Handler {
                        span,
                        prop,
                        mut context,
                    } => {
                        context.insert(0, format!("the {prop} handler of an intrinsic element"));
                        // The element passes an event as the first argument and nothing else.
                        let arguments = forward
                            .sources
                            .iter()
                            .map(|source| match source {
                                ArgumentSource::Position(0, _) => None,
                                ArgumentSource::Position(_, default) => {
                                    Some(default.clone().unwrap_or_else(|| written(None)))
                                }
                                ArgumentSource::Written(argument) => Some(argument.clone()),
                            })
                            .collect();
                        leads.push(Found::Call(FoundCall {
                            span,
                            arguments,
                            context,
                            via: via.clone(),
                            inner: forward.inner.clone(),
                            forwards: false,
                        }));
                    }
                    other => leads.push(Found::Lead(other.into_owned(), forward.clone(), None)),
                }
            };
            let mut frames = Vec::new();
            match code.body {
                ScopeBody::Statements(statements) => {
                    visit_statements(statements, &mut frames, &mut found);
                }
                ScopeBody::Expression(expression) => {
                    visit(expression, Role::Return, &mut frames, &mut found);
                }
            }
            let is_start = via.is_empty() && matches!(target, Target::Site(_, _, None, _));
            for found in leads {
                let (lead, forward, via) = match found {
                    Found::Call(call) => {
                        walk.used = true;
                        if recorded
                            .insert((span_key(&call.span), call.inner.as_ref().map(span_key)))
                        {
                            walk.calls.push(call);
                        }
                        continue;
                    }
                    Found::Lead(lead, forward, hop) => {
                        let mut via = via.clone();
                        via.extend(hop);
                        (lead, forward, via)
                    }
                };
                match lead {
                    OwnedLead::Same(next) => {
                        walk.used |= !is_start;
                        queue.push_back((scope.clone(), next, via, forward));
                    }
                    OwnedLead::Enter {
                        scopes,
                        parameter,
                        steps,
                        hop,
                    } => {
                        walk.used = true;
                        if via.len() >= MAX_HOPS {
                            continue;
                        }
                        let mut next_via = via.clone();
                        next_via.push(hop);
                        for next in scopes {
                            let Some(code) = self.scope_code(&next) else {
                                continue;
                            };
                            if !code.callable {
                                walk.escapes.push((
                                    self.scope_span(&next),
                                    format!(
                                        "{}, whose value {} is not a function the walk follows",
                                        next_via.last().map_or("", String::as_str),
                                        code.name
                                    ),
                                    Vec::new(),
                                    via.clone(),
                                ));
                                continue;
                            }
                            // A function without the parameter cannot read the result.
                            let Some(param) = code.params.get(parameter) else {
                                continue;
                            };
                            let mut targets = Vec::new();
                            bind_targets(param, &steps, &mut targets);
                            for target in targets {
                                queue.push_back((
                                    next.clone(),
                                    target,
                                    next_via.clone(),
                                    forward.clone(),
                                ));
                            }
                        }
                    }
                    OwnedLead::Callers(steps) => {
                        walk.used = true;
                        if via.len() >= MAX_HOPS {
                            continue;
                        }
                        let callee = code.name.rsplit('.').next().map(str::to_owned);
                        let edges = graph.get(&scope).map_or(&[][..], Vec::as_slice);
                        if edges.is_empty() {
                            walk.escapes.push((
                                self.scope_span(&scope),
                                format!(
                                    "returned by {}, whose callers are not in the parsed files",
                                    code.name
                                ),
                                Vec::new(),
                                via.clone(),
                            ));
                        }
                        for edge in edges {
                            let mut next_via = via.clone();
                            next_via.push(format!(
                                "returned by {} to {}",
                                code.name,
                                self.scope_code(&edge.user)
                                    .map_or_else(String::new, |user| user.name)
                            ));
                            queue.push_back((
                                edge.user.clone(),
                                Target::Site(
                                    edge.site.start,
                                    edge.site.end,
                                    callee.clone(),
                                    steps.clone(),
                                ),
                                next_via,
                                forward.clone(),
                            ));
                        }
                    }
                    OwnedLead::Escape {
                        span,
                        detail,
                        context,
                    } => {
                        walk.used = true;
                        walk.escapes.push((span, detail, context, via));
                    }
                    OwnedLead::Used => walk.used |= !is_start,
                    OwnedLead::Unrelated => {}
                }
            }
        }
        walk
    }

    fn scope_span(&self, scope: &UseNode) -> SourceSpan {
        match scope {
            UseNode::Function(key) => self.functions[key].span.clone(),
            UseNode::Global(index) => self.globals_ir[*index].1.span.clone(),
        }
    }

    /// Names a scope binds: its parameters and every binding in its body, nested functions
    /// included.
    fn scope_locals(&self, scope: &UseNode) -> Option<BTreeSet<String>> {
        let code = self.scope_code(scope)?;
        let mut locals = BTreeSet::new();
        for param in code.params {
            collect_pattern_locals(param, &mut locals);
        }
        match code.body {
            ScopeBody::Statements(statements) => collect_statement_locals(statements, &mut locals),
            ScopeBody::Expression(expression) => collect_expression_locals(expression, &mut locals),
        }
        Some(locals)
    }

    /// Evaluates a context-free argument with only module bindings.
    fn evaluate_context_free(
        &mut self,
        file_id: FileId,
        expression: &FlowExpression,
    ) -> QueryValue {
        query_value(&self.eval_with(file_id, expression, Vec::new()))
    }

    /// Evaluates an expression with module bindings and the given locals, without keeping the
    /// gaps the evaluation records: this is a report on the source, not an explored path.
    fn eval_with(
        &mut self,
        file_id: FileId,
        expression: &FlowExpression,
        locals: Vec<(String, TrackedValue)>,
    ) -> TrackedValue {
        let gaps = self.coverage_gaps.len();
        let query_gaps = self.query_gaps.len();
        let mut environment: Environment = self.module_environment(file_id);
        environment.extend(locals);
        let value = self.eval(expression, &environment, file_id);
        for gap in self.coverage_gaps.drain(gaps..) {
            self.coverage_gap_keys.remove(&gap);
        }
        for (gap, _, _, choice) in self.query_gaps.drain(query_gaps..) {
            self.query_gap_keys.remove(&(gap, choice));
        }
        value
    }

    /// For a local array built from literal elements and `push` calls, such as
    /// `const items = []; if (ready) items.push(Kind.A);`, the values it may contain.
    fn pushed_elements(
        &mut self,
        scope: &UseNode,
        argument: &FlowExpression,
    ) -> Option<Vec<QueryValue>> {
        let FlowExpressionKind::Identifier { name, .. } = &argument.kind else {
            return None;
        };
        let locals = self.scope_locals(scope)?;
        let (file_id, elements) = {
            let code = self.scope_code(scope)?;
            let mut elements = Vec::new();
            let mut bound = false;
            let mut reassigned = false;
            let mut found = |frames: &[Frame<'_>]| {
                let frame = frames.last().expect("visited frame");
                match (&frame.role, &frame.expression.kind) {
                    (
                        Role::Bind(FlowPattern {
                            kind: FlowPatternKind::Identifier { name: bound_name },
                            ..
                        }),
                        FlowExpressionKind::Array { elements: initial },
                    ) if bound_name == name => {
                        bound = true;
                        elements.extend(initial.iter().cloned());
                    }
                    (Role::Assign(FlowAssignmentTarget::Identifier { name: assigned }), _)
                        if assigned == name =>
                    {
                        reassigned = true;
                    }
                    (_, FlowExpressionKind::Call { callee, arguments }) => {
                        if let FlowExpressionKind::StaticMember { object, property } = &callee.kind
                            && property == "push"
                            && matches!(&object.kind, FlowExpressionKind::Identifier { name: pushed, .. } if pushed == name)
                        {
                            elements.extend(arguments.iter().cloned());
                        }
                    }
                    _ => {}
                }
            };
            let mut frames = Vec::new();
            match code.body {
                ScopeBody::Statements(statements) => {
                    visit_statements(statements, &mut frames, &mut found);
                }
                ScopeBody::Expression(expression) => {
                    visit(expression, Role::Return, &mut frames, &mut found);
                }
            }
            if !bound || reassigned || elements.is_empty() {
                return None;
            }
            (code.file_id, elements)
        };
        let mut values = Vec::new();
        for element in &elements {
            let value = if is_context_free(element, &locals)
                && !matches!(element.kind, FlowExpressionKind::Spread { .. })
            {
                self.evaluate_context_free(file_id, element)
            } else {
                QueryValue::Unknown {
                    reason: "element_depends_on_context".to_owned(),
                }
            };
            if !values.contains(&value) {
                values.push(value);
            }
        }
        Some(values)
    }

    /// For an argument that reads only the enclosing function's parameters, its value with
    /// what each caller passes for them, when the caller passes context-free values.
    fn values_from_callers(
        &mut self,
        scope: &UseNode,
        argument: &FlowExpression,
        graph: &HashMap<UseNode, Vec<UseEdge>>,
    ) -> Vec<QueryCallerValue> {
        let Some(locals) = self.scope_locals(scope) else {
            return Vec::new();
        };
        let mut read = BTreeSet::new();
        collect_read_names(argument, &mut read);
        let read = read.intersection(&locals).cloned().collect::<Vec<_>>();
        if read.is_empty() {
            return Vec::new();
        }
        let (file_id, callee, parameters) = {
            let Some(code) = self.scope_code(scope) else {
                return Vec::new();
            };
            // Each read name must come straight from a parameter.
            let mut parameters = Vec::new();
            for name in &read {
                let Some(binding) = code.params.iter().enumerate().find_map(|(index, param)| {
                    pattern_path(param, name).map(|steps| (index, steps))
                }) else {
                    return Vec::new();
                };
                parameters.push((name.clone(), binding));
            }
            (
                code.file_id,
                code.name.rsplit('.').next().unwrap_or_default().to_owned(),
                parameters,
            )
        };
        let callers = graph
            .get(scope)
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .map(|edge| (edge.user.clone(), edge.site.clone()))
            .take(MAX_CALLERS)
            .collect::<Vec<_>>();
        let mut results = Vec::new();
        for (user, site) in callers {
            let Some(caller_locals) = self.scope_locals(&user) else {
                continue;
            };
            let Some((caller_file, passed)) =
                self.passed_values(&user, &site, &callee, &parameters)
            else {
                continue;
            };
            if !passed
                .iter()
                .all(|(_, expression)| is_context_free(expression, &caller_locals))
            {
                continue;
            }
            let locals = passed
                .iter()
                .map(|(name, expression)| {
                    (
                        name.clone(),
                        self.eval_with(caller_file, expression, Vec::new()),
                    )
                })
                .collect();
            let value = query_value(&self.eval_with(file_id, argument, locals));
            if value_is_known(&value) {
                results.push(QueryCallerValue {
                    caller: self.query_location(&site),
                    value,
                });
            }
        }
        results
    }

    /// At a call or JSX site in a caller, the expression passed for each parameter binding.
    fn passed_values(
        &self,
        user: &UseNode,
        site: &SourceSpan,
        callee: &str,
        parameters: &[(String, (usize, Vec<Step>))],
    ) -> Option<(FileId, Vec<(String, FlowExpression)>)> {
        let code = self.scope_code(user)?;
        let mut at_site = None;
        let mut found = |frames: &[Frame<'_>]| {
            let expression = frames.last().expect("visited frame").expression;
            if at_site.is_none() && expression.span == *site {
                at_site = Some(expression.clone());
            }
        };
        let mut frames = Vec::new();
        match code.body {
            ScopeBody::Statements(statements) => {
                visit_statements(statements, &mut frames, &mut found);
            }
            ScopeBody::Expression(expression) => {
                visit(expression, Role::Return, &mut frames, &mut found);
            }
        }
        let at_site = at_site?;
        let mut passed = Vec::new();
        for (name, (index, steps)) in parameters {
            let (mut value, rest) = match &at_site.kind {
                FlowExpressionKind::Call {
                    callee: target,
                    arguments,
                } if callee_text(target).rsplit('.').next() == Some(callee) => {
                    (arguments.get(*index)?.clone(), &steps[..])
                }
                // A component's props are its first parameter, one prop per first step.
                FlowExpressionKind::JsxElement { props, .. } if *index == 0 => {
                    let prop = steps.first()?;
                    let value = props.iter().find_map(|candidate| match candidate {
                        FlowJsxProp::Property { name, value, .. } if name == prop => {
                            Some(value.clone())
                        }
                        _ => None,
                    })?;
                    (value, &steps[1..])
                }
                _ => return None,
            };
            for step in rest {
                value = record_field(&value, step)?;
            }
            passed.push((name.clone(), value));
        }
        Some((code.file_id, passed))
    }

    fn summarize_callsite(
        &mut self,
        candidate: &FactoryCallCandidate,
        query: &QuerySpec,
        walk: Walk,
        graph: &HashMap<UseNode, Vec<UseEdge>>,
    ) -> QueryCallsiteValues {
        // What exploration found at the callsite, copied out so arguments can be evaluated.
        let mut reachability = Reachability::Unknown;
        let mut contexts = 0;
        let mut explored_factory_arguments = Vec::new();
        let mut explored =
            BTreeMap::<(u32, u32), (SourceSpan, Vec<BTreeMap<String, QueryValue>>)>::new();
        for capability in self
            .capabilities
            .iter()
            .filter(|capability| capability.callsite == candidate.span)
        {
            contexts += 1;
            if rank(capability.reachability) < rank(reachability) {
                reachability = capability.reachability;
            }
            explored_factory_arguments.push(
                query
                    .factory_arguments
                    .iter()
                    .map(|projection| {
                        capability
                            .factory_arguments
                            .get(projection.index)
                            .map_or_else(
                                || QueryValue::Unknown {
                                    reason: "missing_argument".to_owned(),
                                },
                                query_value,
                            )
                    })
                    .collect::<Vec<_>>(),
            );
            for invocation in &capability.invocations {
                let span = &self.evidence[invocation.evidence.0 as usize].span;
                let arguments = query
                    .capability
                    .invocation_arguments
                    .iter()
                    .map(|projection| {
                        (
                            projection.label.clone(),
                            invocation
                                .arguments
                                .get(projection.index)
                                .map_or(QueryValue::Undefined, query_value),
                        )
                    })
                    .collect();
                explored
                    .entry((span.file_id.0, span.start))
                    .or_insert_with(|| (span.clone(), Vec::new()))
                    .1
                    .push(arguments);
            }
        }
        let mut factory_arguments = BTreeMap::<String, Vec<QueryValue>>::new();
        let candidate_locals = if contexts == 0 {
            self.candidate_scope(candidate)
                .and_then(|scope| self.scope_locals(&scope))
                .unwrap_or_default()
        } else {
            BTreeSet::new()
        };
        for (position, projection) in query.factory_arguments.iter().enumerate() {
            let mut values = Vec::new();
            if contexts == 0 {
                // No context explored the callsite; what it writes may still be known.
                values.push(match candidate.arguments.get(projection.index) {
                    Some(argument) if is_context_free(argument, &candidate_locals) => {
                        self.evaluate_context_free(candidate.file_id, argument)
                    }
                    Some(_) => QueryValue::Unknown {
                        reason: "callsite_not_explored".to_owned(),
                    },
                    None => QueryValue::Undefined,
                });
            }
            for arguments in &explored_factory_arguments {
                if !values.contains(&arguments[position]) {
                    values.push(arguments[position].clone());
                }
            }
            factory_arguments.insert(projection.label.clone(), values);
        }
        let mut calls = Vec::new();
        let mut covered = BTreeSet::new();
        // A call that passes a wrapper's parameter on is replaced by the calls of the wrapper.
        let wrapped = walk
            .calls
            .iter()
            .filter_map(|call| call.inner.as_ref().map(span_key))
            .collect::<BTreeSet<_>>();
        covered.extend(wrapped.iter().copied());
        for call in walk.calls {
            let key = span_key(&call.span);
            if call.forwards && wrapped.contains(&key) {
                continue;
            }
            // Explored invocations of a wrapper's calls are at the call inside the wrapper.
            let explored_key = call.inner.as_ref().map_or(key, span_key);
            let explored_arguments = explored.get(&explored_key).map(|(_, arguments)| arguments);
            if call.inner.is_none() && explored_arguments.is_some() {
                covered.insert(key);
            }
            let mut arguments = BTreeMap::<String, Vec<QueryValue>>::new();
            for (projection, argument) in query
                .capability
                .invocation_arguments
                .iter()
                .zip(&call.arguments)
            {
                let mut values = Vec::new();
                match argument {
                    Some(WrittenArgument {
                        expression: None, ..
                    }) => values.push(QueryValue::Undefined),
                    Some(WrittenArgument {
                        file_id,
                        expression: Some(expression),
                        locals,
                        ..
                    }) if is_context_free(expression, locals) => {
                        // A context-free value holds on every path.
                        let value = self.evaluate_context_free(*file_id, expression);
                        if value_is_known(&value) {
                            values.push(value);
                        }
                    }
                    _ => {}
                }
                if values.is_empty() && call.inner.is_none() {
                    for explored in explored_arguments.into_iter().flatten() {
                        if let Some(value) = explored.get(&projection.label)
                            && !values.contains(value)
                        {
                            values.push(value.clone());
                        }
                    }
                }
                if values.is_empty() {
                    values.push(QueryValue::Unknown {
                        reason: match argument {
                            None => "event_from_intrinsic_element",
                            Some(argument) if argument.parameter => {
                                "argument_is_a_parameter_of_the_enclosing_function"
                            }
                            Some(_) => "argument_depends_on_unexplored_context",
                        }
                        .to_owned(),
                    });
                }
                arguments.insert(projection.label.clone(), values);
            }
            let arguments_resolved = arguments.values().flatten().all(value_is_known);
            // A wrapper's call ran on an explored path if the call inside the wrapper saw its
            // values there.
            let explored = match (&call.inner, explored_arguments) {
                (None, explored) => explored.is_some(),
                (Some(_), Some(explored)) => {
                    arguments_resolved
                        && explored.iter().any(|invocation| {
                            arguments.iter().all(|(label, values)| {
                                invocation
                                    .get(label)
                                    .is_some_and(|value| values.contains(value))
                            })
                        })
                }
                (Some(_), None) => false,
            };
            calls.push(QueryCapabilityCall {
                location: self.query_location(&call.span),
                context: call.context,
                via: call.via,
                arguments,
                arguments_resolved,
                explored,
            });
        }
        // Explored invocations the walk did not reach, such as through a value it lost.
        for (key, (span, invocations)) in &explored {
            if covered.contains(key) {
                continue;
            }
            let mut arguments = BTreeMap::<String, Vec<QueryValue>>::new();
            for invocation in invocations {
                for (label, value) in invocation {
                    let values = arguments.entry(label.clone()).or_default();
                    if !values.contains(value) {
                        values.push(value.clone());
                    }
                }
            }
            let arguments_resolved = arguments.values().flatten().all(value_is_known);
            calls.push(QueryCapabilityCall {
                location: self.query_location(span),
                context: Vec::new(),
                via: vec!["found by exploration".to_owned()],
                arguments,
                arguments_resolved,
                explored: true,
            });
        }
        calls.sort_by(|left, right| {
            location_key(left.location.as_ref()).cmp(&location_key(right.location.as_ref()))
        });
        let escapes = walk
            .escapes
            .into_iter()
            .map(|(span, detail, context, via)| QueryCapabilityEscape {
                location: self.query_location(&span),
                detail,
                context,
                via,
            })
            .collect::<Vec<_>>();
        let status = if !calls.is_empty() {
            if calls.iter().all(|call| call.arguments_resolved) {
                QueryCapabilityStatus::Called
            } else {
                QueryCapabilityStatus::CalledWithUnknownArguments
            }
        } else if !escapes.is_empty() {
            QueryCapabilityStatus::Escapes
        } else if walk.used {
            QueryCapabilityStatus::NotCalled
        } else {
            QueryCapabilityStatus::Unused
        };
        let location = self.query_location(&candidate.span);
        let unreached_reason = self
            .unreached_callsites
            .iter()
            .find(|unreached| unreached.location == location)
            .map(|unreached| unreached.reason);
        let enclosing = self
            .candidate_scope(candidate)
            .map(|scope| self.describe_node(&scope));
        let factory_arguments_resolved = factory_arguments.values().flatten().all(value_is_known);
        // Where an argument is unresolved, what the source still says about it.
        let mut possible_elements = BTreeMap::new();
        let mut values_from_callers = BTreeMap::new();
        if let Some(scope) = self.candidate_scope(candidate) {
            for projection in &query.factory_arguments {
                if factory_arguments
                    .get(&projection.label)
                    .is_some_and(|values| values.iter().all(value_is_known))
                {
                    continue;
                }
                let Some(argument) = candidate.arguments.get(projection.index) else {
                    continue;
                };
                if let Some(elements) = self.pushed_elements(&scope, argument) {
                    possible_elements.insert(projection.label.clone(), elements);
                }
                let callers = self.values_from_callers(&scope, argument, graph);
                if !callers.is_empty() {
                    values_from_callers.insert(projection.label.clone(), callers);
                }
            }
        }
        QueryCallsiteValues {
            location,
            enclosing,
            reachability,
            unreached_reason,
            contexts,
            factory_arguments,
            factory_arguments_resolved,
            possible_elements,
            values_from_callers,
            capability: QueryCapabilityUse {
                status,
                calls,
                escapes,
            },
        }
    }
}

/// A lead without borrowed IR, for the walk queue.
enum OwnedLead {
    Same(Target),
    Enter {
        scopes: Vec<UseNode>,
        parameter: usize,
        steps: Vec<Step>,
        hop: String,
    },
    Callers(Vec<Step>),
    Escape {
        span: SourceSpan,
        detail: String,
        context: Vec<String>,
    },
    Used,
    Unrelated,
}

impl Lead<'_> {
    fn into_owned(self) -> OwnedLead {
        match self {
            Lead::Same(target) => OwnedLead::Same(target),
            Lead::Enter {
                scopes,
                parameter,
                steps,
                hop,
            } => OwnedLead::Enter {
                scopes,
                parameter,
                steps,
                hop,
            },
            Lead::Callers(steps) => OwnedLead::Callers(steps),
            Lead::Escape {
                span,
                detail,
                context,
            } => OwnedLead::Escape {
                span,
                detail,
                context,
            },
            Lead::Call { .. } | Lead::Handler { .. } | Lead::Used => OwnedLead::Used,
            Lead::Unrelated => OwnedLead::Unrelated,
        }
    }
}

/// The module and export a loader callback imports: `() => import('./Panel')` loads the default
/// export, and `() => import('./Panel').then((module) => module.Panel)` a named one.
fn loaded_module(callback: &FlowExpression) -> Option<(String, String)> {
    let FlowExpressionKind::Arrow { body, .. } = &callback.kind else {
        return None;
    };
    let returned = match body {
        FlowArrowBody::Expression { expression } => expression.as_ref(),
        FlowArrowBody::Statements { statements } => {
            statements.iter().find_map(|statement| match statement {
                FlowStatement::Return {
                    value: Some(value), ..
                } => Some(value),
                _ => None,
            })?
        }
    };
    match &returned.kind {
        FlowExpressionKind::DynamicImport { module } => {
            Some((module.clone(), "default".to_owned()))
        }
        FlowExpressionKind::Call { callee, arguments } => {
            let FlowExpressionKind::StaticMember { object, property } = &callee.kind else {
                return None;
            };
            let FlowExpressionKind::DynamicImport { module } = &object.kind else {
                return None;
            };
            if property != "then" {
                return None;
            }
            let FlowExpressionKind::Arrow {
                params,
                body: FlowArrowBody::Expression { expression },
            } = &arguments.first()?.kind
            else {
                return None;
            };
            let [
                FlowPattern {
                    kind: FlowPatternKind::Identifier { name: module_name },
                    ..
                },
            ] = params.as_slice()
            else {
                return None;
            };
            // `(module) => module.Panel` or `(module) => ({ default: module.Panel })`.
            let read = match &expression.kind {
                FlowExpressionKind::Record { fields } => {
                    &fields
                        .iter()
                        .find(|field| field.property == "default")?
                        .value
                }
                _ => expression.as_ref(),
            };
            match &read.kind {
                FlowExpressionKind::StaticMember { object, property } => match &object.kind {
                    FlowExpressionKind::Identifier { name, .. } if name == module_name => {
                        Some((module.clone(), property.clone()))
                    }
                    _ => None,
                },
                _ => None,
            }
        }
        _ => None,
    }
}

/// The steps from a parameter's value to the binding `name` in its pattern.
fn pattern_path(pattern: &FlowPattern, name: &str) -> Option<Vec<Step>> {
    match &pattern.kind {
        FlowPatternKind::Identifier { name: bound } => (bound == name).then(Vec::new),
        FlowPatternKind::Default { target, .. } => pattern_path(target, name),
        FlowPatternKind::Object { fields, .. } => fields.iter().find_map(|field| {
            pattern_path(&field.target, name).map(|rest| {
                std::iter::once(field.source_property.clone())
                    .chain(rest)
                    .collect()
            })
        }),
        FlowPatternKind::Array { elements } => {
            elements.iter().enumerate().find_map(|(index, element)| {
                let element = element.as_ref()?;
                pattern_path(element, name)
                    .map(|rest| std::iter::once(format!("[{index}]")).chain(rest).collect())
            })
        }
        FlowPatternKind::Unsupported { .. } => None,
    }
}

/// The value written for a property or index in a record or array literal.
fn record_field(value: &FlowExpression, step: &str) -> Option<FlowExpression> {
    match &value.kind {
        FlowExpressionKind::Record { fields } => fields
            .iter()
            .rev()
            .find(|field| !field.spread && field.property == step)
            .map(|field| field.value.clone()),
        FlowExpressionKind::Array { elements } => {
            let index = step
                .strip_prefix('[')?
                .strip_suffix(']')?
                .parse::<usize>()
                .ok()?;
            elements.get(index).cloned()
        }
        _ => None,
    }
}

/// Every name an expression reads.
fn collect_read_names(expression: &FlowExpression, names: &mut BTreeSet<String>) {
    if let FlowExpressionKind::Identifier { name, .. } = &expression.kind {
        names.insert(name.clone());
    }
    let mut children = Vec::new();
    expression_children(expression, &mut children);
    for child in children {
        collect_read_names(child, names);
    }
}

/// The position and default of the parameter that binds `name` directly.
fn parameter_position<'p>(
    params: &'p [FlowPattern],
    name: &str,
) -> Option<(usize, Option<&'p FlowExpression>)> {
    params
        .iter()
        .enumerate()
        .find_map(|(position, param)| match &param.kind {
            FlowPatternKind::Identifier { name: bound } if bound == name => Some((position, None)),
            FlowPatternKind::Default { target, default } => match &target.kind {
                FlowPatternKind::Identifier { name: bound } if bound == name => {
                    Some((position, Some(default.as_ref())))
                }
                _ => None,
            },
            _ => None,
        })
}

fn span_key(span: &SourceSpan) -> (u32, u32) {
    (span.file_id.0, span.start)
}

/// Orders reachability from strongest to weakest.
fn rank(reachability: Reachability) -> u8 {
    match reachability {
        Reachability::Reachable => 0,
        Reachability::Possible => 1,
        Reachability::Unknown => 2,
    }
}

fn value_is_known(value: &QueryValue) -> bool {
    match value {
        QueryValue::Unknown { .. } => false,
        QueryValue::Alternatives { values } | QueryValue::Array { elements: values } => {
            values.iter().all(value_is_known)
        }
        _ => true,
    }
}

fn location_key(location: Option<&crate::query::QueryLocation>) -> (String, u32, u32) {
    location.map_or_else(
        || (String::new(), 0, 0),
        |location| {
            (
                location.path.clone(),
                location.start_line,
                location.start_column,
            )
        },
    )
}
