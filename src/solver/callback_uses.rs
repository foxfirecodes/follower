//! Follows the selected factory result from each callsite to the calls made with it. The walk is
//! over the source, not over explored paths, so a call is reported with its arguments whether or
//! not an explored path executed it; explored invocations fill in arguments that depend on
//! context.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    path::PathBuf,
    rc::Rc,
};

use crate::{
    ids::FileId,
    ir::{
        FlowArrowBody, FlowAssignmentTarget, FlowExpression, FlowExpressionKind, FlowJsxProp,
        FlowJsxTag, FlowLogicalOperator, FlowPattern, FlowPatternKind, FlowStatement, SourceSpan,
    },
    link::{LinkedSymbol, LinkedValue, ValueResolution, pattern_names},
    query::{
        QueryCallerValue, QueryCallsiteValues, QueryCapabilityCall, QueryCapabilityEscape,
        QueryCapabilityStatus, QueryCapabilityUse, QuerySpec, QueryValue, Reachability,
    },
};

use crate::query::QueryCallPathKind;

use super::{
    AbstractValue, Environment, FactoryCallCandidate, FunctionKey, Solver, TrackedValue, UseEdge,
    UseNode, callee_text, query_value,
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

/// The site the walk entered a scope through: the scope that holds it, and the site the walk
/// entered that scope through, so a value spread on from the scope's own props can be found
/// where its caller wrote it.
struct Entered {
    user: UseNode,
    site: SourceSpan,
    parent: Option<Rc<Entered>>,
}

/// What the walk follows in one scope.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum Target {
    /// A binding, with the steps from its value to the factory result.
    Name(String, Vec<Step>),
    /// The expression at this span in the scope's file, with the steps from its value to the
    /// factory result. With a callee scope, only a call of that scope matches, since a use site
    /// can also be a call that receives the function as an argument.
    Site(u32, u32, Option<UseNode>, Vec<Step>),
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
        /// The call or element that passes the value, in the scope being left.
        site: SourceSpan,
    },
    /// Passed in the props a configured opener renders `component` with.
    Open {
        component: &'e FlowExpression,
        steps: Vec<Step>,
        span: SourceSpan,
        text: String,
        context: Vec<String>,
    },
    Callers(Vec<Step>),
    /// Stored in a class component's state, which its methods read as `this.state`.
    ClassState {
        class: String,
        steps: Vec<Step>,
    },
    /// Stored in a configured state library's store.
    Store {
        store: LinkedSymbol,
        steps: Vec<Step>,
    },
    /// Provided as a React context's value.
    Context {
        context: LinkedSymbol,
        steps: Vec<Step>,
    },
    /// Passed to a call of `root` at `path`, which may be a parameter whose callers pass a
    /// function.
    ParameterCall {
        root: String,
        path: Vec<Step>,
        position: usize,
        steps: Vec<Step>,
        span: SourceSpan,
        text: String,
        context: Vec<String>,
    },
    Escape {
        span: SourceSpan,
        detail: String,
        context: Vec<String>,
        /// Files outside the snapshot that following it needs, such as a component's module.
        unparsed: BTreeSet<PathBuf>,
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
    /// The last forwarding call the path went through, which this call replaces.
    through: Option<SourceSpan>,
    /// Whether this call passes a wrapper's parameter on, so calls of the wrapper replace it.
    forwards: bool,
    /// The conditions the call runs under, along the whole path from the callsite.
    guards: Rc<Vec<OwnedGuard>>,
    /// The caller site that supplied the callsite's arguments, when the path left the callsite's
    /// scope through a caller, such as the element that renders a wrapper with a function child.
    instance: Option<SourceSpan>,
}

/// A condition on the walk's path, with where to evaluate the values it compares with.
#[derive(Clone)]
struct OwnedGuard {
    file_id: FileId,
    test: FlowExpression,
    /// Whether the test holds or fails where the guarded code runs.
    holds: bool,
    locals: Rc<BTreeSet<String>>,
}

fn guard_key(guards: &[OwnedGuard]) -> Vec<(u32, u32, u32, bool)> {
    guards
        .iter()
        .map(|guard| {
            (
                guard.file_id.0,
                guard.test.span.start,
                guard.test.span.end,
                guard.holds,
            )
        })
        .collect()
}

/// Whether a test compares anything with `===`, so it may say which value runs the code.
fn has_equality(test: &FlowExpression) -> bool {
    match &test.kind {
        FlowExpressionKind::StrictEquality { .. } => true,
        FlowExpressionKind::Logical { left, right, .. } => {
            has_equality(left) || has_equality(right)
        }
        FlowExpressionKind::LogicalNot { value } => has_equality(value),
        _ => false,
    }
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
    /// The last call on the way that passes a wrapper's parameter on; calls found through it
    /// replace it.
    through: Option<SourceSpan>,
}

/// An occurrence's outcome in the walk loop.
enum Found {
    Call(FoundCall),
    /// A lead, the argument mapping it carries, a hop to add to the path, and the conditions
    /// at the occurrence.
    Lead(OwnedLead, Rc<Forward>, Option<String>, Rc<Vec<OwnedGuard>>),
}

struct Walk {
    calls: Vec<FoundCall>,
    escapes: Vec<(SourceSpan, String, Vec<String>, Vec<String>)>,
    /// Whether the result was used anywhere after the factory callsite binds it.
    used: bool,
    /// Files whose importers the walk needs but are not parsed, such as a hook's callers.
    importer_requests: BTreeSet<PathBuf>,
    /// Files the walk needs but are not parsed, such as the module of a component it meets.
    file_requests: BTreeSet<PathBuf>,
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

/// A condition an expression runs under: the test, and whether it holds there.
type Guard<'e> = (&'e FlowExpression, bool);

/// Whether a statement list always leaves the enclosing function or block when it runs.
fn ends_in_jump(statements: &[FlowStatement]) -> bool {
    matches!(
        statements.last(),
        Some(FlowStatement::Return { .. } | FlowStatement::Throw { .. })
    )
}

/// Calls `found` for every expression in statements, with the frames from the statement down
/// and the conditions it runs under.
fn visit_statements<'e>(
    statements: &'e [FlowStatement],
    frames: &mut Vec<Frame<'e>>,
    guards: &mut Vec<Guard<'e>>,
    found: &mut dyn FnMut(&[Frame<'e>], &[Guard<'e>]),
) {
    // Conditions an early return leaves for the statements after it.
    let depth = guards.len();
    for statement in statements {
        match statement {
            FlowStatement::Bind(binding) => {
                visit(
                    &binding.value,
                    Role::Bind(&binding.pattern),
                    frames,
                    guards,
                    found,
                );
            }
            FlowStatement::Return {
                value: Some(value), ..
            } => visit(value, Role::Return, frames, guards, found),
            FlowStatement::Expression { value, .. } | FlowStatement::Throw { value, .. } => {
                visit(value, Role::Other, frames, guards, found);
            }
            FlowStatement::Assign { target, value, .. } => {
                visit(value, Role::Assign(target), frames, guards, found);
                match target {
                    FlowAssignmentTarget::StaticMember { object, .. } => {
                        visit(object, Role::Other, frames, guards, found);
                    }
                    FlowAssignmentTarget::ComputedMember { object, property } => {
                        visit(object, Role::Other, frames, guards, found);
                        visit(property, Role::Other, frames, guards, found);
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
                visit(test, Role::Other, frames, guards, found);
                guards.push((test, true));
                visit_statements(consequent, frames, guards, found);
                guards.pop();
                guards.push((test, false));
                visit_statements(alternate, frames, guards, found);
                guards.pop();
                // `if (kind !== Kind.A) return null;` leaves `kind === Kind.A` for the rest.
                if ends_in_jump(consequent) && !ends_in_jump(alternate) {
                    guards.push((test, false));
                } else if ends_in_jump(alternate) && !ends_in_jump(consequent) {
                    guards.push((test, true));
                }
            }
            FlowStatement::Return { value: None, .. } | FlowStatement::Unsupported(_) => {}
        }
    }
    guards.truncate(depth);
}

fn visit<'e>(
    expression: &'e FlowExpression,
    role: Role<'e>,
    frames: &mut Vec<Frame<'e>>,
    guards: &mut Vec<Guard<'e>>,
    found: &mut dyn FnMut(&[Frame<'e>], &[Guard<'e>]),
) {
    frames.push(Frame { expression, role });
    found(frames, guards);
    match &expression.kind {
        FlowExpressionKind::Record { fields } => {
            for field in fields {
                let role = if field.spread {
                    Role::FieldSpread
                } else {
                    Role::Field(&field.property)
                };
                visit(&field.value, role, frames, guards, found);
            }
        }
        FlowExpressionKind::Array { elements } => {
            for (index, element) in elements.iter().enumerate() {
                visit(element, Role::Element(index), frames, guards, found);
            }
        }
        FlowExpressionKind::Spread { value } => visit(value, Role::Through, frames, guards, found),
        FlowExpressionKind::LogicalNot { value }
        | FlowExpressionKind::LooseNullEquality { value, .. } => {
            visit(value, Role::Other, frames, guards, found);
        }
        FlowExpressionKind::StaticMember { object, property } => {
            visit(
                object,
                Role::Member(Some(property.clone())),
                frames,
                guards,
                found,
            );
        }
        FlowExpressionKind::ComputedMember { object, property } => {
            let key = match &property.kind {
                FlowExpressionKind::Number { value } => Some(format!("[{value}]")),
                FlowExpressionKind::String { value } => Some(value.clone()),
                _ => None,
            };
            visit(object, Role::Member(key), frames, guards, found);
            visit(property, Role::Other, frames, guards, found);
        }
        FlowExpressionKind::Call { callee, arguments } => {
            visit(callee, Role::Callee, frames, guards, found);
            for (index, argument) in arguments.iter().enumerate() {
                visit(argument, Role::Argument(index), frames, guards, found);
            }
        }
        FlowExpressionKind::StrictEquality { left, right, .. } => {
            visit(left, Role::Other, frames, guards, found);
            visit(right, Role::Other, frames, guards, found);
        }
        FlowExpressionKind::Logical {
            left,
            right,
            operator,
        } => {
            visit(left, Role::Through, frames, guards, found);
            // The right side of `&&` runs when the left holds, and of `||` when it does not.
            let guard = match operator {
                FlowLogicalOperator::And => Some((left.as_ref(), true)),
                FlowLogicalOperator::Or => Some((left.as_ref(), false)),
                FlowLogicalOperator::Coalesce => None,
            };
            guards.extend(guard);
            visit(right, Role::Through, frames, guards, found);
            if guard.is_some() {
                guards.pop();
            }
        }
        FlowExpressionKind::Conditional {
            test,
            consequent,
            alternate,
        } => {
            visit(test, Role::Other, frames, guards, found);
            guards.push((test, true));
            visit(consequent, Role::Through, frames, guards, found);
            guards.pop();
            guards.push((test, false));
            visit(alternate, Role::Through, frames, guards, found);
            guards.pop();
        }
        FlowExpressionKind::Arrow { body, .. } => match body {
            FlowArrowBody::Expression { expression } => {
                visit(expression, Role::ArrowBody, frames, guards, found);
            }
            FlowArrowBody::Statements { statements } => {
                visit_statements(statements, frames, guards, found);
            }
        },
        FlowExpressionKind::JsxElement { props, .. } => {
            for prop in props {
                match prop {
                    FlowJsxProp::Property { name, value, .. } => {
                        visit(value, Role::Prop(name), frames, guards, found);
                    }
                    FlowJsxProp::Spread { value, .. } => {
                        visit(value, Role::PropSpread, frames, guards, found);
                    }
                    FlowJsxProp::Unsupported(_) => {}
                }
            }
        }
        _ => {}
    }
    frames.pop();
}

/// Visits a scope's body.
fn visit_body<'e>(body: &ScopeBody<'e>, found: &mut dyn FnMut(&[Frame<'e>], &[Guard<'e>])) {
    let mut frames = Vec::new();
    let mut guards = Vec::new();
    match body {
        ScopeBody::Statements(statements) => {
            visit_statements(statements, &mut frames, &mut guards, found);
        }
        ScopeBody::Expression(expression) => {
            visit(expression, Role::Return, &mut frames, &mut guards, found);
        }
    }
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
            let mut walk = self.walk_capability(candidate, query, &graph);
            self.importer_requests.append(&mut walk.importer_requests);
            self.walk_file_requests.append(&mut walk.file_requests);
            results.push(self.summarize_callsite(candidate, query, walk, &graph));
        }
        results
    }

    /// The functions that, by the static uses in the parsed files, lead to a matching factory
    /// callsite: each callsite's function and every function that uses one of those.
    pub(super) fn factory_ancestors(&self) -> HashSet<FunctionKey> {
        let graph = self.use_graph();
        let mut seen = HashSet::new();
        let mut queue = self
            .factory_candidates()
            .into_iter()
            .filter(|candidate| self.expression_matches_model(candidate.file_id, &candidate.callee))
            .filter_map(|candidate| self.candidate_scope(&candidate))
            .collect::<VecDeque<_>>();
        while let Some(node) = queue.pop_front() {
            if !seen.insert(node.clone()) {
                continue;
            }
            for edge in graph.get(&node).into_iter().flatten() {
                queue.push_back(edge.user.clone());
            }
        }
        seen.into_iter()
            .filter_map(|node| match node {
                UseNode::Function(key) => Some(key),
                UseNode::Global(_) => None,
            })
            .collect()
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
        if let Some(target) = self.destructured_namespace_export(symbol) {
            return self.declaration_scopes(&target);
        }
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
        // `const Panel = require('./Panel').default` binds a module's export.
        if let Some((file_id, binding)) = self.globals_ir.get(index)
            && let FlowExpressionKind::StaticMember { object, property } = &binding.value.kind
            && let FlowExpressionKind::DynamicImport { module } = &object.kind
            && let Some(scopes) = self.module_export_scopes(*file_id, module, property)
        {
            return scopes;
        }
        // `export default Panel` binds the default export to another name, and
        // `export default connect(mapState)(Panel)` to a component a configured wrapper renders.
        if let Some((file_id, binding)) = self.globals_ir.get(index)
            && let FlowExpressionKind::Identifier { name, .. } =
                &self.wrapped_component(*file_id, &binding.value).kind
            && name != &symbol.name
        {
            let scopes = self.resolve_scopes(*file_id, &Imports::new(), name, None);
            if !scopes.is_empty() {
                return scopes;
            }
        }
        self.lazy_scopes(index)
            .unwrap_or_else(|| vec![UseNode::Global(index)])
    }

    /// The scopes of the component a configured opener renders: the component, an import of its
    /// module, or a loader, written at the call or, for a parameter, passed by the site the walk
    /// entered the scope through. `loader()`, as in `open(props.importer(), props)`, opens what
    /// the loader loads.
    fn opened_scopes(
        &self,
        code: &ScopeCode<'_>,
        imports: &Imports,
        scope: &UseNode,
        entered: Option<&Entered>,
        component: &FlowExpression,
        unparsed: &mut BTreeSet<PathBuf>,
        guessed: &mut bool,
    ) -> Vec<UseNode> {
        let (expression, called) = match &component.kind {
            FlowExpressionKind::Call { callee, arguments } if arguments.is_empty() => {
                (callee.as_ref(), true)
            }
            _ => (component, false),
        };
        if let Some((root, path)) = read_path(expression)
            && !imports.contains_key(&root)
            && let Some(binding) =
                code.params.iter().enumerate().find_map(|(index, param)| {
                    pattern_path(param, &root).map(|inner| (index, inner))
                })
        {
            // Without the site, the value is any caller's, which would open every caller's
            // component on this path.
            let Some(entered) = entered else {
                return Vec::new();
            };
            let (index, inner) = binding;
            let steps = inner.into_iter().chain(path).collect();
            return self
                .entered_value(entered, scope, index, steps, 0)
                .map(|(caller_file, value)| {
                    let imports = self
                        .scope_code(&entered.user)
                        .map_or_else(Imports::new, |code| local_imports(&code.body));
                    self.component_scopes(caller_file, &imports, &value, called, unparsed, guessed)
                })
                .unwrap_or_default();
        }
        self.component_scopes(code.file_id, imports, expression, called, unparsed, guessed)
    }

    /// What the site the walk entered `scope` through passes for its parameter at `index` and
    /// `steps`, following a prop the site spreads from its own scope's props, as
    /// `<Sheet {...props} />`, to where that scope's caller wrote it.
    fn entered_value(
        &self,
        entered: &Entered,
        scope: &UseNode,
        index: usize,
        steps: Vec<Step>,
        depth: usize,
    ) -> Option<(FileId, FlowExpression)> {
        if let Some((caller_file, passed)) = self.passed_values(
            &entered.user,
            &entered.site,
            scope,
            &[(String::new(), (index, steps.clone()))],
        ) {
            return passed
                .into_iter()
                .next()
                .map(|(_, value)| (caller_file, value));
        }
        let parent = entered.parent.as_deref().filter(|_| depth < MAX_HOPS)?;
        let user = self.scope_code(&entered.user)?;
        let FlowExpressionKind::JsxElement { props, .. } =
            &self.site_expression(&entered.user, &entered.site)?.kind
        else {
            return None;
        };
        // The last spread of the user's own props is the one that supplies the prop.
        props.iter().rev().find_map(|prop| {
            let FlowJsxProp::Spread { value, .. } = prop else {
                return None;
            };
            let (root, path) = read_path(value)?;
            let (parent_index, inner) =
                user.params
                    .iter()
                    .enumerate()
                    .find_map(|(position, param)| {
                        pattern_path(param, &root).map(|inner| (position, inner))
                    })?;
            let steps = inner
                .into_iter()
                .chain(path)
                .chain(steps.iter().cloned())
                .collect();
            self.entered_value(parent, &entered.user, parent_index, steps, depth + 1)
        })
    }

    /// The expression at a site in a scope's body.
    fn site_expression(&self, scope: &UseNode, site: &SourceSpan) -> Option<FlowExpression> {
        let code = self.scope_code(scope)?;
        let mut at_site = None;
        let mut found = |frames: &[Frame<'_>], _guards: &[Guard<'_>]| {
            let expression = frames.last().expect("visited frame").expression;
            if at_site.is_none() && expression.span == *site {
                at_site = Some(expression.clone());
            }
        };
        visit_body(&code.body, &mut found);
        at_site
    }

    /// The values a file gives a ref by name: what `useRef` starts it with and what is assigned
    /// to its `current`.
    fn ref_values(&self, file_id: FileId, name: &str) -> Vec<FlowExpression> {
        let mut values = Vec::new();
        for (_, function) in self
            .functions
            .iter()
            .filter(|(key, _)| key.file_id == file_id)
        {
            let mut found = |frames: &[Frame<'_>], _guards: &[Guard<'_>]| {
                let frame = frames.last().expect("visited frame");
                match (&frame.role, &frame.expression.kind) {
                    (
                        Role::Bind(FlowPattern {
                            kind: FlowPatternKind::Identifier { name: bound },
                            ..
                        }),
                        FlowExpressionKind::Call { callee, arguments },
                    ) if bound == name
                        && callee_text(callee).rsplit('.').next() == Some("useRef") =>
                    {
                        values.extend(arguments.first().cloned());
                    }
                    (Role::Assign(FlowAssignmentTarget::StaticMember { object, property }), _)
                        if property == "current"
                            && matches!(&object.kind, FlowExpressionKind::Identifier { name: assigned, .. } if assigned == name) =>
                    {
                        values.push(frame.expression.clone());
                    }
                    _ => {}
                }
            };
            visit_body(&ScopeBody::Statements(&function.body), &mut found);
        }
        values
    }

    /// Every value a file writes under a property in a record literal, as the `importer` of each
    /// entry in a table of sheets.
    fn file_property_values(&self, file_id: FileId, property: &str) -> Vec<FlowExpression> {
        let mut values = Vec::new();
        let bodies = self
            .functions
            .iter()
            .filter(|(key, _)| key.file_id == file_id)
            .map(|(_, function)| ScopeBody::Statements(&function.body))
            .chain(
                self.globals_ir
                    .iter()
                    .filter(|(binding_file, _)| *binding_file == file_id)
                    .map(|(_, binding)| ScopeBody::Expression(&binding.value)),
            )
            .collect::<Vec<_>>();
        for body in &bodies {
            let mut found = |frames: &[Frame<'_>], _guards: &[Guard<'_>]| {
                if let FlowExpressionKind::Record { fields } =
                    &frames.last().expect("visited frame").expression.kind
                {
                    values.extend(
                        fields
                            .iter()
                            .filter(|field| !field.spread && field.property == property)
                            .map(|field| field.value.clone()),
                    );
                }
            };
            visit_body(body, &mut found);
        }
        values
    }

    /// The scopes of a component written in a file, or of the component a module import or a
    /// loader loads; with `called`, the expression is a loader the opener's argument calls. A
    /// loaded module that is not parsed goes to `unparsed`.
    fn component_scopes(
        &self,
        file_id: FileId,
        imports: &Imports,
        expression: &FlowExpression,
        called: bool,
        unparsed: &mut BTreeSet<PathBuf>,
        guessed: &mut bool,
    ) -> Vec<UseNode> {
        let mut export_scopes = |file_id: FileId, module: &str, export: &str| {
            self.module_export_scopes(file_id, module, export)
                .unwrap_or_else(|| {
                    unparsed.extend(self.unparsed_module(file_id, module));
                    Vec::new()
                })
        };
        if let Some((module, export)) = imported_module(expression, imports) {
            return export_scopes(file_id, &module, &export);
        }
        let scopes = match &expression.kind {
            FlowExpressionKind::Identifier { name, .. } => {
                self.resolve_scopes(file_id, imports, name, None)
            }
            FlowExpressionKind::StaticMember { object, property } => match &object.kind {
                FlowExpressionKind::Identifier { name, .. } => {
                    self.resolve_scopes(file_id, imports, name, Some(property))
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        // A declared loader, as `function loadPanel() { return import('./Panel'); }`.
        let loader = scopes.iter().find_map(|scope| match scope {
            UseNode::Function(key) => statements_module(&self.functions.get(key)?.body)
                .map(|module| (key.file_id, module)),
            UseNode::Global(index) => {
                let (file_id, binding) = self.globals_ir.get(*index)?;
                loaded_module(&binding.value).map(|module| (*file_id, module))
            }
        });
        if let Some((file_id, (module, export))) = loader {
            return export_scopes(file_id, &module, &export);
        }
        // A property of a local the walk cannot trace, as `selected.importer` for an entry picked
        // from a table, may be any value the file writes under that property.
        if scopes.is_empty()
            && let FlowExpressionKind::StaticMember { object, property } = &expression.kind
            && let FlowExpressionKind::Identifier { name, .. } = &object.kind
            && !imports.contains_key(name)
            && !matches!(
                self.symbol_linker.resolve_binding(file_id, name),
                ValueResolution::Resolved(_)
            )
        {
            let mut found = Vec::new();
            for value in self.file_property_values(file_id, property) {
                if matches!(value.kind, FlowExpressionKind::StaticMember { .. }) {
                    continue;
                }
                for scope in
                    self.component_scopes(file_id, imports, &value, called, unparsed, guessed)
                {
                    if !found.contains(&scope) {
                        found.push(scope);
                    }
                }
            }
            *guessed |= !found.is_empty();
            return found;
        }
        if called { Vec::new() } else { scopes }
    }

    /// The component a value renders through configured wrappers, as `Panel` in
    /// `withTheme(connect(mapState)(Panel))`, or the value itself.
    fn wrapped_component<'e>(
        &self,
        file_id: FileId,
        mut value: &'e FlowExpression,
    ) -> &'e FlowExpression {
        let Some(file) = self.symbol_linker.file(file_id) else {
            return value;
        };
        while let FlowExpressionKind::Call { callee, arguments } = &value.kind
            && let Some(wrapper) = self.project.config.component_wrapper(&file.flow, callee)
            && let Some(component) = arguments.get(wrapper.component_argument)
        {
            value = component;
        }
        value
    }

    /// For a module binding built by a loader, such as `lazy(() => import('./Panel'))`,
    /// `load({ promise: () => import('./Panel') })`, or `load({ promise: importPanel })` with
    /// `importPanel` declared to return an import, the scopes of the component it loads.
    fn lazy_scopes(&self, index: usize) -> Option<Vec<UseNode>> {
        let (file_id, binding) = self.globals_ir.get(index)?;
        let FlowExpressionKind::Call { arguments, .. } = &binding.value.kind else {
            return None;
        };
        let loader = |value: &FlowExpression| match &value.kind {
            FlowExpressionKind::Arrow { .. } => loaded_module(value)
                .and_then(|(module, export)| self.module_export_scopes(*file_id, &module, &export)),
            FlowExpressionKind::Identifier { .. } => {
                let scopes = self.component_scopes(
                    *file_id,
                    &Imports::new(),
                    value,
                    true,
                    &mut BTreeSet::new(),
                    &mut false,
                );
                (!scopes.is_empty()).then_some(scopes)
            }
            _ => None,
        };
        arguments.iter().find_map(|argument| match &argument.kind {
            FlowExpressionKind::Record { fields } => {
                fields.iter().find_map(|field| loader(&field.value))
            }
            _ => loader(argument),
        })
    }

    /// The scopes of a module's export, for a module specifier written in a file.
    fn module_export_scopes(
        &self,
        file_id: FileId,
        module: &str,
        export: &str,
    ) -> Option<Vec<UseNode>> {
        let file = self.symbol_linker.file(file_id)?;
        let path = self
            .symbol_linker
            .import_resolutions(&file.path)
            .filter(|resolution| resolution.specifier == module)
            .find_map(|resolution| resolution.resolved_path.as_ref())?;
        let target = self.symbol_linker.file_at(path)?;
        match self
            .symbol_linker
            .resolve_exported_value(target.file_id, export)
        {
            ValueResolution::Resolved(LinkedValue::Declaration(symbol)) => {
                let scopes = self.declaration_scopes(&symbol);
                (!scopes.is_empty()).then_some(scopes)
            }
            _ => None,
        }
    }

    /// For a module binding such as `export const { usePanel } = web` over `import * as web`,
    /// the declaration it re-exports.
    pub(super) fn destructured_namespace_export(
        &self,
        symbol: &LinkedSymbol,
    ) -> Option<LinkedSymbol> {
        let index = *self.global_bindings.get(symbol)?;
        let (file_id, binding) = self.globals_ir.get(index)?;
        let FlowExpressionKind::Identifier {
            name: namespace, ..
        } = &binding.value.kind
        else {
            return None;
        };
        let steps = pattern_path(&binding.pattern, &symbol.name)?;
        let [export] = steps.as_slice() else {
            return None;
        };
        let ValueResolution::Resolved(LinkedValue::Namespace(module)) =
            self.symbol_linker.resolve_binding(*file_id, namespace)
        else {
            return None;
        };
        match self.symbol_linker.resolve_exported_value(module, export) {
            ValueResolution::Resolved(LinkedValue::Declaration(target)) => Some(target),
            _ => None,
        }
    }

    /// The scopes a name, or a namespace member, refers to from a file. A local bound to
    /// `import('./Panel')` is that module's namespace.
    fn resolve_scopes(
        &self,
        file_id: FileId,
        imports: &Imports,
        name: &str,
        member: Option<&str>,
    ) -> Vec<UseNode> {
        // A local bound to a choice of names is any of them.
        if member.is_none()
            && let Some(names) = imports.choices.get(name)
        {
            let others = Imports {
                modules: imports.modules.clone(),
                ..Imports::default()
            };
            return names
                .iter()
                .flat_map(|chosen| self.resolve_scopes(file_id, &others, chosen, None))
                .collect();
        }
        if let Some((module, export)) = imports.get(name) {
            let export = match (export, member) {
                (None, Some(member)) => member,
                (Some(export), None) => export,
                _ => return Vec::new(),
            };
            return self
                .module_export_scopes(file_id, module, export)
                .unwrap_or_default();
        }
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
        imports: &Imports,
        frames: &[Frame<'e>],
        index: usize,
        steps: Vec<Step>,
    ) -> Lead<'e> {
        let frame = &frames[index];
        let parent = index.checked_sub(1).map(|parent| frames[parent].expression);
        match &frame.role {
            Role::Member(key) => match (steps.first(), key) {
                (Some(first), Some(key)) if first == key => {
                    self.follow(file_id, imports, frames, index - 1, steps[1..].to_vec())
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
                if let Some(lead) = self.state_write(file_id, imports, callee, *position, &steps) {
                    return lead;
                }
                // `useState(value)` holds its argument as the pair's first element, as
                // `useReducer(reducer, value)` holds its second.
                let hook = text.rsplit('.').next();
                if (hook == Some("useState") && *position == 0)
                    || (hook == Some("useReducer") && *position == 1)
                {
                    return self.follow(
                        file_id,
                        imports,
                        frames,
                        index - 1,
                        std::iter::once("[0]".to_owned()).chain(steps).collect(),
                    );
                }
                if *position > 0 && takes_dependencies(&text) {
                    return Lead::Used;
                }
                if *position == 0 && returns_first_argument(&text) {
                    return self.follow(file_id, imports, frames, index - 1, steps);
                }
                if *position == 0 && text.rsplit('.').next() == Some("useRef") {
                    return self.follow(
                        file_id,
                        imports,
                        frames,
                        index - 1,
                        std::iter::once("current".to_owned()).chain(steps).collect(),
                    );
                }
                // Props a configured opener renders its component with.
                if let Some(opener) = self
                    .symbol_linker
                    .file(file_id)
                    .and_then(|file| self.project.config.component_opener(&file.flow, callee))
                    && opener.props_argument == Some(*position)
                    && let Some(FlowExpressionKind::Call { arguments, .. }) =
                        parent.map(|parent| &parent.kind)
                    && let Some(component) = opener
                        .component_argument
                        .and_then(|index| arguments.get(index))
                    && steps.starts_with(&opener.props_path)
                {
                    return Lead::Open {
                        component,
                        steps: steps[opener.props_path.len()..].to_vec(),
                        span: span.clone(),
                        text,
                        context: context_of(frames, index),
                    };
                }
                let scopes = match &callee.kind {
                    FlowExpressionKind::Identifier { name, .. } => {
                        self.resolve_scopes(file_id, imports, name, None)
                    }
                    FlowExpressionKind::StaticMember { object, property } => match &object.kind {
                        FlowExpressionKind::Identifier { name, .. } => {
                            self.resolve_scopes(file_id, imports, name, Some(property))
                        }
                        _ => Vec::new(),
                    },
                    _ => Vec::new(),
                };
                // Props passed next to a module import, as in `open(import('./Sheet'), { onClose })`,
                // are taken as props of the component the module exports.
                if scopes.is_empty()
                    && matches!(frame.expression.kind, FlowExpressionKind::Record { .. })
                    && let Some(FlowExpressionKind::Call { arguments, .. }) =
                        parent.map(|parent| &parent.kind)
                    && let Some((module, export)) = arguments
                        .iter()
                        .find_map(|argument| imported_module(argument, imports))
                    && let Some(scopes) = self.module_export_scopes(file_id, &module, &export)
                {
                    return Lead::Enter {
                        scopes,
                        parameter: 0,
                        steps,
                        hop: format!("props passed with the component {text} loads from {module}"),
                        site: span.clone(),
                    };
                }
                if scopes.is_empty() {
                    // A call of a parameter, such as a function child, is followed to what the
                    // callers pass for it.
                    match read_path(callee) {
                        Some((root, path)) => Lead::ParameterCall {
                            root,
                            path,
                            position: *position,
                            steps,
                            span: span.clone(),
                            text,
                            context: context_of(frames, index),
                        },
                        None => Lead::Escape {
                            span: span.clone(),
                            detail: format!("passed to {text}, which the walk does not follow"),
                            context: context_of(frames, index),
                            unparsed: BTreeSet::new(),
                        },
                    }
                } else {
                    Lead::Enter {
                        scopes,
                        parameter: *position,
                        steps,
                        hop: format!("argument {} of {text}", position + 1),
                        site: span.clone(),
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
                // `<Ctx.Provider value={...}>` provides the value to every `useContext(Ctx)`.
                if prop == Some("value")
                    && let FlowJsxTag::Member {
                        object, property, ..
                    } = tag
                    && property == "Provider"
                    && let ValueResolution::Resolved(LinkedValue::Declaration(context)) =
                        self.symbol_linker.resolve_binding(file_id, object)
                {
                    return Lead::Context { context, steps };
                }
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
                        (self.resolve_scopes(file_id, imports, name, None), None)
                    }
                    FlowJsxTag::Member {
                        object, property, ..
                    } => (
                        self.resolve_scopes(file_id, imports, object, Some(property)),
                        None,
                    ),
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
                    let unparsed = match tag {
                        FlowJsxTag::Identifier { name, .. } => {
                            self.unparsed_targets(file_id, imports, name)
                        }
                        FlowJsxTag::Member { object, .. } => {
                            self.unparsed_targets(file_id, imports, object)
                        }
                        FlowJsxTag::Unsupported { .. } => BTreeSet::new(),
                    };
                    Lead::Escape {
                        span: span.clone(),
                        detail: format!("{shown}, whose component the walk does not follow"),
                        context: context_of(frames, index),
                        unparsed,
                    }
                } else {
                    Lead::Enter {
                        scopes,
                        parameter: 0,
                        steps,
                        hop: shown,
                        site: span.clone(),
                    }
                }
            }
            Role::Field(name) => self.follow(
                file_id,
                imports,
                frames,
                index - 1,
                std::iter::once((*name).to_owned()).chain(steps).collect(),
            ),
            Role::Element(position) => self.follow(
                file_id,
                imports,
                frames,
                index - 1,
                std::iter::once(format!("[{position}]"))
                    .chain(steps)
                    .collect(),
            ),
            Role::FieldSpread | Role::Through => {
                self.follow(file_id, imports, frames, index - 1, steps)
            }
            Role::ArrowBody => self.follow_arrow_result(file_id, imports, frames, index - 1, steps),
            Role::Return => {
                let arrow = (0..index).rev().find(|&frame| {
                    matches!(
                        frames[frame].expression.kind,
                        FlowExpressionKind::Arrow { .. }
                    )
                });
                match arrow {
                    Some(arrow) => self.follow_arrow_result(file_id, imports, frames, arrow, steps),
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
                // `ref.current = value` and other writes to a local's properties.
                FlowAssignmentTarget::StaticMember { object, property } => {
                    match read_path(object) {
                        Some((root, mut path)) => {
                            path.push(property.clone());
                            path.extend(steps);
                            Lead::Same(Target::Name(root, path))
                        }
                        None => Lead::Escape {
                            span: frame.expression.span.clone(),
                            detail: "stored in a property, which the walk does not follow"
                                .to_owned(),
                            context: context_of(frames, index),
                            unparsed: BTreeSet::new(),
                        },
                    }
                }
                _ => Lead::Escape {
                    span: frame.expression.span.clone(),
                    detail: "stored in a property, which the walk does not follow".to_owned(),
                    context: context_of(frames, index),
                    unparsed: BTreeSet::new(),
                },
            },
            Role::Other => Lead::Used,
        }
    }

    /// What passing a value as a call's argument stores, when the callee sets state: a
    /// `useState` setter, a class's `this.setState`, or a configured store's `setState`.
    fn state_write<'e>(
        &self,
        file_id: FileId,
        imports: &Imports,
        callee: &FlowExpression,
        position: usize,
        steps: &[Step],
    ) -> Option<Lead<'e>> {
        if position != 0 {
            return None;
        }
        match &callee.kind {
            FlowExpressionKind::Identifier { name, .. } => {
                if let Some(state) = imports.setters.get(name) {
                    return Some(Lead::Same(Target::Name(state.clone(), steps.to_vec())));
                }
                // `this.setState(...)` lowers to a call of `Class.setState`.
                let class = name.strip_suffix(".setState")?;
                self.class_scopes(file_id, class)
                    .next()
                    .map(|_| Lead::ClassState {
                        class: class.to_owned(),
                        steps: steps.to_vec(),
                    })
            }
            FlowExpressionKind::StaticMember { object, property } if property == "setState" => {
                let FlowExpressionKind::Identifier { name, .. } = &object.kind else {
                    return None;
                };
                self.store_symbol(file_id, name).map(|store| Lead::Store {
                    store,
                    steps: steps.to_vec(),
                })
            }
            _ => None,
        }
    }

    /// A class component's scopes in a file: the class and its methods.
    fn class_scopes<'s>(
        &'s self,
        file_id: FileId,
        class: &'s str,
    ) -> impl Iterator<Item = &'s FunctionKey> + 's {
        self.functions.keys().filter(move |key| {
            key.file_id == file_id
                && (key.name == class
                    || key
                        .name
                        .strip_prefix(class)
                        .is_some_and(|rest| rest.starts_with('.')))
        })
    }

    /// The store a name in a file refers to, when its binding is created by a configured state
    /// library.
    fn store_symbol(&self, file_id: FileId, name: &str) -> Option<LinkedSymbol> {
        let ValueResolution::Resolved(LinkedValue::Declaration(symbol)) =
            self.symbol_linker.resolve_binding(file_id, name)
        else {
            return None;
        };
        let index = *self.global_bindings.get(&symbol)?;
        let (binding_file, binding) = self.globals_ir.get(index)?;
        let FlowExpressionKind::Call { callee, .. } = &binding.value.kind else {
            return None;
        };
        let file = self.symbol_linker.file(*binding_file)?;
        self.project
            .config
            .state_store(&file.flow, callee)
            .map(|_| symbol)
    }

    /// Every scope the walk can visit, with the calls in it.
    fn scope_calls(&self) -> Vec<(UseNode, FileId, Vec<&FlowExpression>)> {
        self.functions
            .iter()
            .map(|(key, function)| {
                (
                    UseNode::Function(key.clone()),
                    key.file_id,
                    calls_in(&function.body, None),
                )
            })
            .chain(
                self.globals_ir
                    .iter()
                    .enumerate()
                    .map(|(index, (file_id, binding))| {
                        (
                            UseNode::Global(index),
                            *file_id,
                            calls_in(&[], Some(&binding.value)),
                        )
                    }),
            )
            .collect()
    }

    /// Where a store's state is read, with the steps each read takes into the state: `S(s =>
    /// s.key)` reads `key`, and `S()`, `S.getState()`, or a selector that is not a property path
    /// reads the whole state.
    fn store_reads(&self, store: &LinkedSymbol) -> Vec<(UseNode, SourceSpan, Vec<Step>)> {
        let mut reads = Vec::new();
        for (scope, file_id, calls) in self.scope_calls() {
            for call in calls {
                let FlowExpressionKind::Call { callee, arguments } = &call.kind else {
                    continue;
                };
                let (name, selector) = match &callee.kind {
                    FlowExpressionKind::Identifier { name, .. } if name == "useStore" => {
                        match arguments.first().map(|argument| &argument.kind) {
                            Some(FlowExpressionKind::Identifier { name, .. }) => {
                                (name, arguments.get(1))
                            }
                            _ => continue,
                        }
                    }
                    FlowExpressionKind::Identifier { name, .. } => (name, arguments.first()),
                    FlowExpressionKind::StaticMember { object, property }
                        if property == "getState" =>
                    {
                        match &object.kind {
                            FlowExpressionKind::Identifier { name, .. } => (name, None),
                            _ => continue,
                        }
                    }
                    _ => continue,
                };
                if self.store_symbol(file_id, name).as_ref() != Some(store) {
                    continue;
                }
                let path = selector.and_then(selector_path).unwrap_or_default();
                reads.push((scope.clone(), call.span.clone(), path));
            }
        }
        reads
    }

    /// Where a context's value is read: `useContext(Ctx)` and `use(Ctx)`.
    fn context_reads(&self, context: &LinkedSymbol) -> Vec<(UseNode, SourceSpan)> {
        let mut reads = Vec::new();
        for (scope, file_id, calls) in self.scope_calls() {
            for call in calls {
                let FlowExpressionKind::Call { callee, arguments } = &call.kind else {
                    continue;
                };
                if !matches!(
                    callee_text(callee).rsplit('.').next(),
                    Some("useContext" | "use")
                ) {
                    continue;
                }
                let Some(FlowExpressionKind::Identifier { name, .. }) =
                    arguments.first().map(|argument| &argument.kind)
                else {
                    continue;
                };
                if matches!(
                    self.symbol_linker.resolve_binding(file_id, name),
                    ValueResolution::Resolved(LinkedValue::Declaration(symbol)) if &symbol == context
                ) {
                    reads.push((scope.clone(), call.span.clone()));
                }
            }
        }
        reads
    }

    /// Follows the value an arrow returns: only `useMemo` hands it on.
    fn follow_arrow_result<'e>(
        &self,
        file_id: FileId,
        imports: &Imports,
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
                self.follow(file_id, imports, frames, arrow - 1, steps)
            }
            // An updater, as `setItems((items) => [...items, item])`, writes what it returns.
            (Role::Argument(0), Some(FlowExpressionKind::Call { callee, .. })) => self
                .state_write(file_id, imports, callee, 0, &steps)
                .unwrap_or(Lead::Used),
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
            importer_requests: BTreeSet::new(),
            file_requests: BTreeSet::new(),
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
            through: None,
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
            Rc::new(Vec::<OwnedGuard>::new()),
            None::<SourceSpan>,
            None::<Rc<Entered>>,
        )]);
        let mut seen = HashSet::new();
        let mut locals_cache = HashMap::<UseNode, Rc<BTreeSet<String>>>::new();
        let mut store_reads =
            BTreeMap::<LinkedSymbol, Vec<(UseNode, SourceSpan, Vec<Step>)>>::new();
        let mut context_reads = BTreeMap::<LinkedSymbol, Vec<(UseNode, SourceSpan)>>::new();
        let mut recorded = HashSet::new();
        // Each entry is a scope and what to follow in it, with the path so far, how calls map to
        // the result's arguments, the conditions carried in, the caller site that supplied the
        // callsite's arguments, and the scope and site the walk entered this scope through.
        while let Some((scope, target, via, forward, carried, instance, entered)) =
            queue.pop_front()
        {
            let inner = forward.inner.as_ref().map(span_key);
            if seen.len() >= WALK_LIMIT
                || !seen.insert((
                    scope.clone(),
                    target.clone(),
                    inner,
                    guard_key(&carried),
                    entered.as_ref().map(|entered| span_key(&entered.site)),
                ))
            {
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
            let imports = &local_imports(&code.body);
            let open = |component: &FlowExpression,
                        steps: Vec<Step>,
                        span: SourceSpan,
                        text: String,
                        context: Vec<String>| {
                let mut unparsed = BTreeSet::new();
                let mut guessed = false;
                let scopes = self.opened_scopes(
                    &code,
                    imports,
                    &scope,
                    entered.as_deref(),
                    component,
                    &mut unparsed,
                    &mut guessed,
                );
                if scopes.is_empty() {
                    OwnedLead::Escape {
                        span,
                        detail: format!(
                            "props of the component {text} opens, which the walk does not resolve"
                        ),
                        context,
                        unparsed,
                    }
                } else {
                    OwnedLead::Enter {
                        scopes,
                        parameter: 0,
                        steps,
                        // A component taken from the values a property may hold is inferred.
                        hop: if guessed {
                            format!(
                                "props passed with the component {text} opens, which may be any the caller's file names for it"
                            )
                        } else {
                            format!("props of the component {text} opens")
                        },
                        site: span,
                    }
                }
            };
            let mut found = |frames: &[Frame<'_>], local: &[Guard<'_>]| {
                let frame = frames.last().expect("visited frame");
                let expression = frame.expression;
                // The carried conditions and the ones at this occurrence.
                let here = || {
                    let mut tests = local
                        .iter()
                        .filter(|(test, _)| has_equality(test))
                        .map(|(test, holds)| OwnedGuard {
                            file_id,
                            test: (*test).clone(),
                            holds: *holds,
                            locals: locals.clone(),
                        })
                        .peekable();
                    if tests.peek().is_none() {
                        return carried.clone();
                    }
                    Rc::new(carried.iter().cloned().chain(tests).collect::<Vec<_>>())
                };
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
                                    if self.calls_scope(file_id, called, callee))
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
                match self.follow(file_id, imports, frames, frames.len() - 1, steps) {
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
                            through: forward.through.clone(),
                            forwards: forwarded,
                            guards: here(),
                            instance: instance.clone(),
                        }));
                        if forwarded && let Some(arrow) = arrow {
                            let lead = self
                                .follow(file_id, imports, frames, arrow, Vec::new())
                                .into_owned(&open);
                            leads.push(Found::Lead(
                                lead,
                                Rc::new(Forward {
                                    sources,
                                    inner: Some(
                                        forward.inner.clone().unwrap_or_else(|| call.span.clone()),
                                    ),
                                    through: Some(call.span.clone()),
                                }),
                                Some(format!("through {}", describe_arrow(frames, arrow))),
                                here(),
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
                            through: forward.through.clone(),
                            forwards: false,
                            guards: here(),
                            instance: instance.clone(),
                        }));
                    }
                    other => leads.push(Found::Lead(
                        other.into_owned(&open),
                        forward.clone(),
                        None,
                        here(),
                    )),
                }
            };
            visit_body(&code.body, &mut found);
            let is_start = via.is_empty() && matches!(target, Target::Site(_, _, None, _));
            for found in leads {
                let (lead, forward, via, here) = match found {
                    Found::Call(call) => {
                        walk.used = true;
                        if recorded.insert((
                            span_key(&call.span),
                            call.inner.as_ref().map(span_key),
                            guard_key(&call.guards),
                        )) {
                            walk.calls.push(call);
                        }
                        continue;
                    }
                    Found::Lead(lead, forward, hop, here) => {
                        let mut via = via.clone();
                        via.extend(hop);
                        (lead, forward, via, here)
                    }
                };
                // A path into a caller takes the caller's site as the instance that supplied
                // the callsite's arguments.
                let from_caller =
                    |site: &SourceSpan| instance.clone().or_else(|| Some(site.clone()));
                match lead {
                    OwnedLead::Same(next) => {
                        walk.used |= !is_start;
                        queue.push_back((
                            scope.clone(),
                            next,
                            via,
                            forward,
                            carried.clone(),
                            instance.clone(),
                            entered.clone(),
                        ));
                    }
                    OwnedLead::Enter {
                        scopes,
                        parameter,
                        steps,
                        hop,
                        site,
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
                            let entered = Rc::new(Entered {
                                user: scope.clone(),
                                site: site.clone(),
                                parent: entered.clone(),
                            });
                            for target in targets {
                                queue.push_back((
                                    next.clone(),
                                    target,
                                    next_via.clone(),
                                    forward.clone(),
                                    here.clone(),
                                    instance.clone(),
                                    Some(entered.clone()),
                                ));
                            }
                        }
                    }
                    OwnedLead::ClassState { class, steps } => {
                        walk.used = true;
                        if via.len() >= MAX_HOPS {
                            continue;
                        }
                        let mut next_via = via.clone();
                        next_via.push(format!("stored in the state of {class}"));
                        let state = format!("{class}.state");
                        for key in self.class_scopes(file_id, &class) {
                            queue.push_back((
                                UseNode::Function(key.clone()),
                                Target::Name(state.clone(), steps.clone()),
                                next_via.clone(),
                                forward.clone(),
                                here.clone(),
                                instance.clone(),
                                None,
                            ));
                        }
                    }
                    OwnedLead::Store { store, steps } => {
                        walk.used = true;
                        // Readers in files that are not parsed come in the next round.
                        if let Some(file) = self.symbol_linker.file(store.file_id) {
                            walk.importer_requests.insert(file.path.clone());
                        }
                        if via.len() >= MAX_HOPS {
                            continue;
                        }
                        let reads = store_reads
                            .entry(store.clone())
                            .or_insert_with(|| self.store_reads(&store))
                            .clone();
                        for (reader, span, path) in reads {
                            // A read takes the steps its selector names into the state.
                            let Some(rest) = steps.strip_prefix(path.as_slice()) else {
                                continue;
                            };
                            let mut next_via = via.clone();
                            next_via.push(format!(
                                "stored in {} and read by {}",
                                store.name,
                                self.scope_code(&reader)
                                    .map_or_else(String::new, |code| code.name)
                            ));
                            queue.push_back((
                                reader,
                                Target::Site(span.start, span.end, None, rest.to_vec()),
                                next_via,
                                forward.clone(),
                                here.clone(),
                                instance.clone(),
                                None,
                            ));
                        }
                    }
                    OwnedLead::Context { context, steps } => {
                        walk.used = true;
                        if let Some(file) = self.symbol_linker.file(context.file_id) {
                            walk.importer_requests.insert(file.path.clone());
                        }
                        if via.len() >= MAX_HOPS {
                            continue;
                        }
                        let reads = context_reads
                            .entry(context.clone())
                            .or_insert_with(|| self.context_reads(&context))
                            .clone();
                        for (reader, span) in reads {
                            let mut next_via = via.clone();
                            next_via.push(format!(
                                "provided as {} and read by {}",
                                context.name,
                                self.scope_code(&reader)
                                    .map_or_else(String::new, |code| code.name)
                            ));
                            queue.push_back((
                                reader,
                                Target::Site(span.start, span.end, None, steps.clone()),
                                next_via,
                                forward.clone(),
                                here.clone(),
                                instance.clone(),
                                None,
                            ));
                        }
                    }
                    OwnedLead::Callers(steps) => {
                        walk.used = true;
                        if via.len() >= MAX_HOPS {
                            continue;
                        }
                        let callee = Some(scope.clone());
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
                            self.request_importers(&scope, &mut walk);
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
                                here.clone(),
                                from_caller(&edge.site),
                                None,
                            ));
                        }
                    }
                    OwnedLead::ParameterCall {
                        root,
                        path,
                        position,
                        steps,
                        span,
                        text,
                        context,
                    } => {
                        walk.used = true;
                        let binding = code.params.iter().enumerate().find_map(|(index, param)| {
                            pattern_path(param, &root).map(|inner| (index, inner))
                        });
                        let Some((parameter, inner)) = binding else {
                            walk.escapes.push((
                                span,
                                format!("passed to {text}, which the walk does not follow"),
                                context,
                                via,
                            ));
                            walk.file_requests
                                .append(&mut self.unparsed_targets(file_id, imports, &root));
                            continue;
                        };
                        if via.len() >= MAX_HOPS {
                            continue;
                        }
                        let parameter_steps = inner.into_iter().chain(path).collect::<Vec<_>>();
                        let edges = graph.get(&scope).map_or(&[][..], Vec::as_slice);
                        if edges.is_empty() {
                            walk.escapes.push((
                                span.clone(),
                                format!(
                                    "passed to {text}, a parameter of {}, whose callers are not in the parsed files",
                                    code.name
                                ),
                                context.clone(),
                                via.clone(),
                            ));
                            self.request_importers(&scope, &mut walk);
                        }
                        for edge in edges {
                            let Some((caller_file, passed)) = self.passed_values(
                                &edge.user,
                                &edge.site,
                                &scope,
                                &[(root.clone(), (parameter, parameter_steps.clone()))],
                            ) else {
                                continue;
                            };
                            let Some((_, function)) = passed.into_iter().next() else {
                                continue;
                            };
                            let caller = self
                                .scope_code(&edge.user)
                                .map_or_else(String::new, |user| user.name);
                            let mut next_via = via.clone();
                            next_via.push(format!("{text} given by {caller}"));
                            let mut targets = Vec::new();
                            match &function.kind {
                                // A function written at the call, such as a function child.
                                FlowExpressionKind::Arrow { params, .. } => {
                                    if let Some(param) = params.get(position) {
                                        bind_targets(param, &steps, &mut targets);
                                    }
                                    for target in targets {
                                        queue.push_back((
                                            edge.user.clone(),
                                            target,
                                            next_via.clone(),
                                            forward.clone(),
                                            here.clone(),
                                            from_caller(&edge.site),
                                            None,
                                        ));
                                    }
                                }
                                FlowExpressionKind::Identifier { name, .. } => {
                                    let scopes =
                                        self.resolve_scopes(caller_file, &Imports::new(), name, None);
                                    let local = self.local_function_params(&edge.user, name);
                                    for next in scopes {
                                        let Some(code) = self.scope_code(&next) else {
                                            continue;
                                        };
                                        let mut targets = Vec::new();
                                        if let Some(param) = code.params.get(position) {
                                            bind_targets(param, &steps, &mut targets);
                                        }
                                        for target in targets {
                                            queue.push_back((
                                                next.clone(),
                                                target,
                                                next_via.clone(),
                                                forward.clone(),
                                                here.clone(),
                                                from_caller(&edge.site),
                                                None,
                                            ));
                                        }
                                    }
                                    if let Some(params) = local
                                        && let Some(param) = params.get(position)
                                    {
                                        bind_targets(param, &steps, &mut targets);
                                        for target in targets {
                                            queue.push_back((
                                                edge.user.clone(),
                                                target,
                                                next_via.clone(),
                                                forward.clone(),
                                                here.clone(),
                                                from_caller(&edge.site),
                                                None,
                                            ));
                                        }
                                    }
                                }
                                _ => walk.escapes.push((
                                    function.span.clone(),
                                    format!(
                                        "passed to {text}, which {caller} gives a value the walk does not follow"
                                    ),
                                    Vec::new(),
                                    via.clone(),
                                )),
                            }
                        }
                    }
                    OwnedLead::Escape {
                        span,
                        detail,
                        context,
                        mut unparsed,
                    } => {
                        walk.used = true;
                        walk.escapes.push((span, detail, context, via));
                        walk.file_requests.append(&mut unparsed);
                    }
                    OwnedLead::Used => walk.used |= !is_start,
                    OwnedLead::Unrelated => {}
                }
            }
        }
        walk
    }

    /// The parameters of a function bound to a local name in a scope, such as a handler
    /// declared in a component.
    fn local_function_params(&self, scope: &UseNode, name: &str) -> Option<Vec<FlowPattern>> {
        let code = self.scope_code(scope)?;
        let mut params = None;
        let mut found = |frames: &[Frame<'_>], _guards: &[Guard<'_>]| {
            let frame = frames.last().expect("visited frame");
            if params.is_none()
                && let Role::Bind(pattern) = &frame.role
                && matches!(&pattern.kind, FlowPatternKind::Identifier { name: bound } if bound == name)
            {
                // A function bound directly or through `useCallback`.
                let function = match &frame.expression.kind {
                    FlowExpressionKind::Call { callee, arguments }
                        if returns_first_argument(&callee_text(callee)) =>
                    {
                        arguments.first()
                    }
                    _ => Some(frame.expression),
                };
                if let Some(FlowExpression {
                    kind: FlowExpressionKind::Arrow { params: arrow, .. },
                    ..
                }) = function
                {
                    params = Some(arrow.clone());
                }
            }
        };
        visit_body(&code.body, &mut found);
        params
    }

    /// Whether a call's callee is a scope, through any import alias, as for a default import
    /// named differently from the function.
    fn calls_scope(&self, file_id: FileId, callee: &FlowExpression, scope: &UseNode) -> bool {
        let scopes = match &callee.kind {
            FlowExpressionKind::Identifier { name, .. } => {
                self.resolve_scopes(file_id, &Imports::new(), name, None)
            }
            FlowExpressionKind::StaticMember { object, property } => match &object.kind {
                FlowExpressionKind::Identifier { name, .. } => {
                    self.resolve_scopes(file_id, &Imports::new(), name, Some(property))
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        scopes.contains(scope)
    }

    /// Asks the next pass to parse the files that import a scope's file.
    fn request_importers(&self, scope: &UseNode, walk: &mut Walk) {
        let file_id = match scope {
            UseNode::Function(key) => key.file_id,
            UseNode::Global(index) => self.globals_ir[*index].0,
        };
        if let Some(file) = self.symbol_linker.file(file_id) {
            walk.importer_requests.insert(file.path.clone());
        }
    }

    /// The files outside the snapshot that resolving a name from a file needs: the module a local
    /// bound to `import()` loads, or the modules that linking the name reached.
    fn unparsed_targets(
        &self,
        file_id: FileId,
        imports: &Imports,
        name: &str,
    ) -> BTreeSet<PathBuf> {
        let Some((module, _)) = imports.get(name) else {
            return self.symbol_linker.unparsed_link_targets(file_id, name);
        };
        self.unparsed_module(file_id, module).into_iter().collect()
    }

    /// The file a module specifier written in a file resolves to, when it is not parsed.
    fn unparsed_module(&self, file_id: FileId, module: &str) -> Option<PathBuf> {
        let file = self.symbol_linker.file(file_id)?;
        self.symbol_linker
            .import_resolutions(&file.path)
            .filter(|resolution| resolution.specifier == module)
            .find_map(|resolution| resolution.resolved_path.clone())
            .filter(|path| self.symbol_linker.file_at(path).is_none())
    }

    fn scope_span(&self, scope: &UseNode) -> SourceSpan {
        match scope {
            UseNode::Function(key) => self.functions[key].span.clone(),
            UseNode::Global(index) => self.globals_ir[*index].1.span.clone(),
        }
    }

    /// What a condition says a compared value is: in one of the returned sets, as for
    /// `kind === Kind.A || kind === Kind.B`, and none of the excluded values, as after
    /// `if (kind !== Kind.C) return;`. Only enum members count, since those are what a
    /// condition on a selected item compares with.
    fn guard_values(&mut self, guard: &OwnedGuard) -> (Vec<Vec<QueryValue>>, Vec<QueryValue>) {
        let mut sets = Vec::new();
        let mut excluded = Vec::new();
        for comparison in comparison_sets(&guard.test, guard.holds) {
            let (pairs, one_of) = match comparison {
                Comparison::OneOf(pairs) => (pairs, true),
                Comparison::NoneOf(pairs) => (pairs, false),
            };
            let mut values = Vec::new();
            for (left, right) in pairs {
                match (
                    self.guard_constant(guard, left),
                    self.guard_constant(guard, right),
                ) {
                    (Some(value), None) | (None, Some(value)) => {
                        if !values.contains(&value) {
                            values.push(value);
                        }
                    }
                    _ => {
                        values.clear();
                        break;
                    }
                }
            }
            if values.is_empty() {
                continue;
            }
            if one_of {
                sets.push(values);
            } else {
                excluded.extend(values);
            }
        }
        (sets, excluded)
    }

    /// The enum member a side of a comparison names, as in `Kind.A`.
    fn guard_constant(&mut self, guard: &OwnedGuard, side: &FlowExpression) -> Option<QueryValue> {
        if !matches!(
            side.kind,
            FlowExpressionKind::StaticMember { .. } | FlowExpressionKind::NumericEnumMember { .. }
        ) || !is_context_free(side, &guard.locals)
        {
            return None;
        }
        let value = self.evaluate_context_free(guard.file_id, side);
        matches!(value, QueryValue::EnumMember { .. }).then_some(value)
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

    /// The elements an instance's caller writes for a factory argument that is one of the
    /// callsite scope's parameters, as `contentTypes={[selected.id]}`. A property of a local the
    /// walk cannot trace, as `selected.id` for an entry picked from a table, may be any value the
    /// caller's file writes under that property.
    fn written_instance_elements(
        &mut self,
        candidate: &FactoryCallCandidate,
        graph: &HashMap<UseNode, Vec<UseEdge>>,
        instance: &SourceSpan,
        position: usize,
    ) -> Option<Vec<QueryValue>> {
        let scope = self.candidate_scope(candidate)?;
        let FlowExpressionKind::Identifier { name, .. } = &candidate.arguments.get(position)?.kind
        else {
            return None;
        };
        let binding = self
            .scope_code(&scope)?
            .params
            .iter()
            .enumerate()
            .find_map(|(index, param)| pattern_path(param, name).map(|steps| (index, steps)))?;
        let edge = graph
            .get(&scope)?
            .iter()
            .find(|edge| edge.site == *instance)?;
        let (caller_file, passed) =
            self.passed_values(&edge.user, &edge.site, &scope, &[(name.clone(), binding)])?;
        let (_, written) = passed.into_iter().next()?;
        let caller_locals = self.scope_locals(&edge.user)?;
        // The locals the written value reads properties of, and which properties.
        let mut reads = BTreeMap::<String, BTreeSet<String>>::new();
        let mut found = |frames: &[Frame<'_>], _guards: &[Guard<'_>]| {
            if let FlowExpressionKind::StaticMember { object, property } =
                &frames.last().expect("visited frame").expression.kind
                && let FlowExpressionKind::Identifier { name, .. } = &object.kind
                && caller_locals.contains(name)
            {
                reads
                    .entry(name.clone())
                    .or_default()
                    .insert(property.clone());
            }
        };
        visit_body(&ScopeBody::Expression(&written), &mut found);
        let mut locals = Vec::new();
        for (local, properties) in reads {
            // One record per value the file writes under each property the local is read for.
            let mut choices = Vec::new();
            for property in properties {
                for value in self.file_property_values(caller_file, &property) {
                    let value = self.eval_with(caller_file, &value, Vec::new());
                    choices.push(TrackedValue::plain(AbstractValue::open_record(
                        BTreeMap::from([(property.clone(), value)]),
                        "property_choice",
                    )));
                }
            }
            if choices.is_empty() {
                return None;
            }
            locals.push((
                local,
                TrackedValue::plain(AbstractValue::Union(Rc::new(choices))),
            ));
        }
        if locals.is_empty() {
            return None;
        }
        let value = query_value(&self.eval_with(caller_file, &written, locals));
        let mut elements = Vec::new();
        collect_array_elements(&value, &mut elements);
        // An element that may be any of several values is each of them.
        let elements = elements
            .into_iter()
            .flat_map(|element| match element {
                QueryValue::Alternatives { values } => values,
                element => vec![element],
            })
            .filter(value_is_known)
            .fold(Vec::new(), |mut elements, element| {
                if !elements.contains(&element) {
                    elements.push(element);
                }
                elements
            });
        (!elements.is_empty()).then_some(elements)
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
            let mut found = |frames: &[Frame<'_>], _guards: &[Guard<'_>]| {
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
            visit_body(&code.body, &mut found);
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
            (code.file_id, scope.clone(), parameters)
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
        callee: &UseNode,
        parameters: &[(String, (usize, Vec<Step>))],
    ) -> Option<(FileId, Vec<(String, FlowExpression)>)> {
        let code = self.scope_code(user)?;
        let mut at_site = None;
        let mut found = |frames: &[Frame<'_>], _guards: &[Guard<'_>]| {
            let expression = frames.last().expect("visited frame").expression;
            if at_site.is_none() && expression.span == *site {
                at_site = Some(expression.clone());
            }
        };
        visit_body(&code.body, &mut found);
        let at_site = at_site?;
        let mut passed = Vec::new();
        for (name, (index, steps)) in parameters {
            let (mut value, rest) = match &at_site.kind {
                FlowExpressionKind::Call {
                    callee: target,
                    arguments,
                } if self.calls_scope(code.file_id, target, callee) => {
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
        // The steps of the path each explored context was created on, to match an instance.
        let mut explored_traces = Vec::new();
        let mut explored =
            BTreeMap::<(u32, u32), (SourceSpan, Vec<BTreeMap<String, QueryValue>>)>::new();
        for capability in self
            .capabilities
            .iter()
            .filter(|capability| capability.callsite == candidate.span)
        {
            contexts += 1;
            explored_traces.push(
                capability
                    .origin_trace
                    .iter()
                    .map(|step| (step.kind, step.span.clone()))
                    .collect::<Vec<_>>(),
            );
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
        let factory_arguments_resolved = factory_arguments.values().flatten().all(value_is_known);
        // Where an argument is unresolved, what the source still says about it.
        let mut possible_elements = BTreeMap::new();
        let mut values_from_callers = BTreeMap::new();
        // An argument with several possible arrays, such as a list filtered by unknown
        // predicates, is summarized by the elements they hold.
        for (label, values) in &factory_arguments {
            if values.iter().map(array_count).sum::<usize>() > 1 {
                let mut elements = Vec::new();
                for value in values {
                    collect_array_elements(value, &mut elements);
                }
                possible_elements.insert(label.clone(), elements);
            }
        }
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
                if let Some(pushed) = self.pushed_elements(&scope, argument) {
                    let elements = possible_elements
                        .entry(projection.label.clone())
                        .or_insert_with(Vec::new);
                    for element in pushed {
                        if !elements.contains(&element) {
                            elements.push(element);
                        }
                    }
                }
                let callers = self.values_from_callers(&scope, argument, graph);
                if !callers.is_empty() {
                    values_from_callers.insert(projection.label.clone(), callers);
                }
            }
        }
        // The elements of each array argument a call can apply to, before its conditions narrow
        // them: the instance caller's value when the path went through one, else the callsite's.
        let callsite_elements = factory_arguments
            .iter()
            .map(|(label, values)| {
                let elements = possible_elements.get(label).cloned().unwrap_or_else(|| {
                    let mut elements = Vec::new();
                    for value in values {
                        collect_array_elements(value, &mut elements);
                    }
                    elements
                });
                let complete = elements.iter().all(value_is_known)
                    && (values.iter().all(value_is_known) || possible_elements.contains_key(label));
                // Literals the callers pass are among the elements, though other callers' values
                // may not be known.
                let mut elements = elements;
                for caller in values_from_callers.get(label).into_iter().flatten() {
                    let mut passed = Vec::new();
                    collect_array_elements(&caller.value, &mut passed);
                    for element in passed {
                        if !elements.contains(&element) {
                            elements.push(element);
                        }
                    }
                }
                (label.clone(), (elements, complete))
            })
            .collect::<BTreeMap<_, _>>();
        let caller_elements = values_from_callers
            .iter()
            .map(|(label, callers)| {
                let by_caller = callers
                    .iter()
                    .map(|caller| {
                        let mut elements = Vec::new();
                        collect_array_elements(&caller.value, &mut elements);
                        (caller.caller.clone(), elements)
                    })
                    .collect::<Vec<_>>();
                (label.clone(), by_caller)
            })
            .collect::<BTreeMap<_, _>>();
        let mut excluded_calls = Vec::new();
        // The elements an instance requests, from the contexts explored through it: those whose
        // path renders the instance's element, or else passes through the function holding its
        // call.
        let instance_elements = |span: &SourceSpan, position: usize| {
            let exact = explored_traces
                .iter()
                .enumerate()
                .filter(|(_, trace)| trace.iter().any(|(_, step)| step == span))
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            let matched = if exact.is_empty() {
                explored_traces
                    .iter()
                    .enumerate()
                    .filter(|(_, trace)| {
                        trace.iter().any(|(kind, step)| {
                            *kind == QueryCallPathKind::Call
                                && step.file_id == span.file_id
                                && step.start <= span.start
                                && span.end <= step.end
                        })
                    })
                    .map(|(index, _)| index)
                    .collect()
            } else {
                exact
            };
            if matched.is_empty() {
                return None;
            }
            let mut elements = Vec::new();
            for index in matched {
                collect_array_elements(&explored_factory_arguments[index][position], &mut elements);
            }
            Some(elements)
        };
        let label_positions = query
            .factory_arguments
            .iter()
            .enumerate()
            .map(|(position, projection)| (projection.label.clone(), position))
            .collect::<BTreeMap<_, _>>();
        // An argument that reads the callsite scope's parameters, as a wrapper's `items` prop,
        // differs by instance; any other is the same for every caller.
        let parameters = self
            .candidate_scope(candidate)
            .and_then(|scope| self.scope_code(&scope))
            .map(|code| {
                code.params
                    .iter()
                    .flat_map(pattern_names)
                    .map(str::to_owned)
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        let per_instance = query
            .factory_arguments
            .iter()
            .map(|projection| {
                let mut read = BTreeSet::new();
                if let Some(argument) = candidate.arguments.get(projection.index) {
                    collect_read_names(argument, &mut read);
                }
                // When the callsite's elements are all known, they bound every instance's.
                let known = callsite_elements
                    .get(&projection.label)
                    .is_some_and(|(_, complete)| *complete);
                (
                    projection.label.clone(),
                    !known && !read.is_disjoint(&parameters),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut calls = Vec::new();
        let mut covered = BTreeSet::new();
        // A call that passes a wrapper's parameter on is replaced by the calls of the wrapper,
        // at each level of wrappers.
        let wrapped = walk
            .calls
            .iter()
            .flat_map(|call| [call.inner.as_ref(), call.through.as_ref()])
            .flatten()
            .map(span_key)
            .collect::<BTreeSet<_>>();
        covered.extend(wrapped.iter().copied());
        // What each instance's caller writes for the factory arguments, for an instance whose
        // explored elements are not known.
        let mut written_elements = BTreeMap::new();
        for instance in walk.calls.iter().filter_map(|call| call.instance.clone()) {
            for (label, &position) in &label_positions {
                let key = (span_key(&instance), label.clone());
                if written_elements.contains_key(&key) {
                    continue;
                }
                let elements =
                    self.written_instance_elements(candidate, graph, &instance, position);
                written_elements.insert(key, elements);
            }
        }
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
                // A ref's `current`, as `pending.current`, is its initial value or any value the
                // file assigns to it, when nothing else gave a known value.
                if !values.iter().any(value_is_known)
                    && let Some(WrittenArgument {
                        file_id,
                        expression: Some(expression),
                        ..
                    }) = argument
                    && let FlowExpressionKind::StaticMember { object, property } = &expression.kind
                    && property == "current"
                    && let FlowExpressionKind::Identifier { name, .. } = &object.kind
                {
                    let mut assigned = Vec::new();
                    for written in self.ref_values(*file_id, name) {
                        let value = self.evaluate_context_free(*file_id, &written);
                        if value_is_known(&value) && !assigned.contains(&value) {
                            assigned.push(value);
                        }
                    }
                    if !assigned.is_empty() {
                        values = assigned;
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
            let instance = call
                .instance
                .as_ref()
                .and_then(|span| self.query_location(span));
            let mut guards = Vec::new();
            let mut guards_not = Vec::new();
            for guard in call.guards.iter() {
                let (sets, excluded) = self.guard_values(guard);
                for set in sets {
                    if !guards.contains(&set) {
                        guards.push(set);
                    }
                }
                for value in excluded {
                    if !guards_not.contains(&value) {
                        guards_not.push(value);
                    }
                }
            }
            let mut elements = BTreeMap::new();
            let mut elements_complete = true;
            let mut ruled_out = false;
            for (label, (callsite, callsite_complete)) in &callsite_elements {
                // An instance requests its own elements; the callsite's are every instance's. An
                // argument that does not read the callsite's parameters is the same everywhere.
                let from_instance = call.instance.as_ref().and_then(|span| {
                    let written = || {
                        written_elements
                            .get(&(span_key(span), label.clone()))
                            .cloned()
                            .flatten()
                    };
                    match instance_elements(span, label_positions[label]) {
                        // Explored contexts that know nothing defer to what the caller writes.
                        Some(explored) if !explored.iter().any(value_is_known) => {
                            written().or(Some(explored))
                        }
                        Some(explored) => Some(explored),
                        None => {
                            let location = self.query_location(span);
                            caller_elements
                                .get(label)
                                .and_then(|callers| {
                                    callers
                                        .iter()
                                        .find(|(caller, _)| *caller == location)
                                        .map(|(_, elements)| elements.clone())
                                })
                                .or_else(written)
                        }
                    }
                });
                let (source, complete) = match from_instance {
                    Some(source) => {
                        let complete = source.iter().all(value_is_known);
                        (source, complete)
                    }
                    // The instance's own elements are not known, so neither are the call's.
                    None if call.instance.is_some() && per_instance[label] => (Vec::new(), false),
                    None => (callsite.clone(), *callsite_complete),
                };
                // Conditions on members of the elements' enum say which elements reach the call.
                let mut domain = source.clone();
                domain.extend(callsite.iter().cloned());
                let applicable = guards
                    .iter()
                    .filter(|set| guard_applies(set, &domain))
                    .collect::<Vec<_>>();
                let ruled = guards_not
                    .iter()
                    .filter(|value| guard_applies(std::slice::from_ref(value), &domain))
                    .collect::<Vec<_>>();
                let allowed = |element: &&QueryValue| {
                    applicable.iter().all(|set| set.contains(element)) && !ruled.contains(element)
                };
                let narrowed = if complete || applicable.is_empty() {
                    source.iter().filter(allowed).cloned().collect::<Vec<_>>()
                } else {
                    // Unknown elements: the conditions still bound which can reach the call.
                    applicable[0].iter().filter(allowed).cloned().collect()
                };
                // A condition no requested element meets means this callsite's result cannot
                // reach the call, as when a shared descriptor carries several hooks' callbacks.
                let conditioned = !applicable.is_empty() || !ruled.is_empty();
                ruled_out |= complete && conditioned && narrowed.is_empty();
                elements_complete &= complete || !applicable.is_empty();
                elements.insert(label.clone(), narrowed);
            }
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
            let found = QueryCapabilityCall {
                location: self.query_location(&call.span),
                context: call.context,
                via: call.via,
                arguments,
                arguments_resolved,
                explored,
                instance,
                guards,
                guards_not,
                elements,
                elements_complete,
            };
            if ruled_out {
                excluded_calls.push(found);
            } else {
                calls.push(found);
            }
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
                instance: None,
                guards: Vec::new(),
                guards_not: Vec::new(),
                elements: callsite_elements
                    .iter()
                    .map(|(label, (elements, _))| (label.clone(), elements.clone()))
                    .collect(),
                elements_complete: callsite_elements.values().all(|(_, complete)| *complete),
            });
        }
        let mut calls = merge_calls(calls);
        // A call another condition context allows is not excluded.
        let excluded_calls = merge_calls(excluded_calls)
            .into_iter()
            .filter(|excluded| !calls.iter().any(|call| call.location == excluded.location))
            .collect::<Vec<_>>();
        calls.sort_by(|left, right| {
            location_key(left.location.as_ref()).cmp(&location_key(right.location.as_ref()))
        });
        // A scope entered through several sites meets its escapes once for each; the first path
        // stands for them.
        let mut met = HashSet::new();
        let escapes = walk
            .escapes
            .into_iter()
            .filter(|(span, detail, context, _)| {
                met.insert((span_key(span), detail.clone(), context.clone()))
            })
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
                excluded_calls,
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
        site: SourceSpan,
    },
    Callers(Vec<Step>),
    ClassState {
        class: String,
        steps: Vec<Step>,
    },
    Store {
        store: LinkedSymbol,
        steps: Vec<Step>,
    },
    Context {
        context: LinkedSymbol,
        steps: Vec<Step>,
    },
    ParameterCall {
        root: String,
        path: Vec<Step>,
        position: usize,
        steps: Vec<Step>,
        span: SourceSpan,
        text: String,
        context: Vec<String>,
    },
    Escape {
        span: SourceSpan,
        detail: String,
        context: Vec<String>,
        unparsed: BTreeSet<PathBuf>,
    },
    Used,
    Unrelated,
}

impl<'e> Lead<'e> {
    /// The lead without borrowed frames; `open` resolves the component a configured opener
    /// renders, given the lead's component, steps, span, callee text, and context.
    fn into_owned(
        self,
        open: &dyn Fn(&'e FlowExpression, Vec<Step>, SourceSpan, String, Vec<String>) -> OwnedLead,
    ) -> OwnedLead {
        match self {
            Lead::Same(target) => OwnedLead::Same(target),
            Lead::Enter {
                scopes,
                parameter,
                steps,
                hop,
                site,
            } => OwnedLead::Enter {
                scopes,
                parameter,
                steps,
                hop,
                site,
            },
            Lead::Open {
                component,
                steps,
                span,
                text,
                context,
            } => open(component, steps, span, text, context),
            Lead::Callers(steps) => OwnedLead::Callers(steps),
            Lead::ClassState { class, steps } => OwnedLead::ClassState { class, steps },
            Lead::Store { store, steps } => OwnedLead::Store { store, steps },
            Lead::Context { context, steps } => OwnedLead::Context { context, steps },
            Lead::ParameterCall {
                root,
                path,
                position,
                steps,
                span,
                text,
                context,
            } => OwnedLead::ParameterCall {
                root,
                path,
                position,
                steps,
                span,
                text,
                context,
            },
            Lead::Escape {
                span,
                detail,
                context,
                unparsed,
            } => OwnedLead::Escape {
                span,
                detail,
                context,
                unparsed,
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
    match body {
        FlowArrowBody::Expression { expression } => returned_module(expression),
        FlowArrowBody::Statements { statements } => statements_module(statements),
    }
}

/// The calls in a function body or expression, outermost first.
pub(super) fn calls_in<'e>(
    statements: &'e [FlowStatement],
    expression: Option<&'e FlowExpression>,
) -> Vec<&'e FlowExpression> {
    let mut calls = Vec::new();
    let mut found = |frames: &[Frame<'e>], _guards: &[Guard<'e>]| {
        let expression = frames.last().expect("visited frame").expression;
        if matches!(expression.kind, FlowExpressionKind::Call { .. }) {
            calls.push(expression);
        }
    };
    match expression {
        Some(expression) => visit_body(&ScopeBody::Expression(expression), &mut found),
        None => visit_body(&ScopeBody::Statements(statements), &mut found),
    }
    calls
}

/// The names a function body reads, calls, or renders as a JSX tag.
pub(super) fn body_names(body: &FlowArrowBody) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut found = |frames: &[Frame<'_>], _guards: &[Guard<'_>]| match &frames
        .last()
        .expect("visited frame")
        .expression
        .kind
    {
        FlowExpressionKind::Identifier { name, .. }
        | FlowExpressionKind::JsxElement {
            tag:
                FlowJsxTag::Identifier {
                    name,
                    intrinsic: false,
                    ..
                }
                | FlowJsxTag::Member { object: name, .. },
            ..
        } => {
            names.insert(name.clone());
        }
        _ => {}
    };
    let body = match body {
        FlowArrowBody::Statements { statements } => ScopeBody::Statements(statements),
        FlowArrowBody::Expression { expression } => ScopeBody::Expression(expression),
    };
    visit_body(&body, &mut found);
    names
}

/// The module and export a function body's first `return` loads.
pub(super) fn statements_module(statements: &[FlowStatement]) -> Option<(String, String)> {
    statements
        .iter()
        .find_map(|statement| match statement {
            FlowStatement::Return {
                value: Some(value), ..
            } => Some(value),
            _ => None,
        })
        .and_then(returned_module)
}

/// The module and export a returned value loads, as for `loaded_module`.
pub(super) fn returned_module(returned: &FlowExpression) -> Option<(String, String)> {
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
        FlowPatternKind::Object { fields, rest } => fields
            .iter()
            .find_map(|field| {
                pattern_path(&field.target, name).map(|inner| {
                    std::iter::once(field.source_property.clone())
                        .chain(inner)
                        .collect()
                })
            })
            // The rest holds the other properties at the same steps.
            .or_else(|| rest.as_ref().and_then(|rest| pattern_path(rest, name))),
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

/// The number of arrays a value may be.
fn array_count(value: &QueryValue) -> usize {
    match value {
        QueryValue::Array { .. } => 1,
        QueryValue::Alternatives { values } => values.iter().map(array_count).sum(),
        _ => 0,
    }
}

/// The distinct elements of every array a value may be, in first-seen order. An unknown
/// alternative is kept, since the arrays it stands for may hold anything.
fn collect_array_elements(value: &QueryValue, elements: &mut Vec<QueryValue>) {
    let mut add = |element: &QueryValue| {
        if !elements.contains(element) {
            elements.push(element.clone());
        }
    };
    match value {
        QueryValue::Array { elements: items } => items.iter().for_each(&mut add),
        QueryValue::Alternatives { values } => {
            for value in values {
                collect_array_elements(value, elements);
            }
        }
        QueryValue::Unknown { .. } => add(value),
        _ => {}
    }
}

/// What a scope's locals name, for resolving them as components or modules.
#[derive(Default)]
struct Imports {
    /// Locals bound to a module loaded with `import()`: the module, and the export the name
    /// holds, or `None` for the namespace, as in `const module = await import('./Panel')`.
    modules: HashMap<String, (String, Option<String>)>,
    /// Locals bound to one of several names, as `const List = compact ? Grid : Rows`.
    choices: HashMap<String, Vec<String>>,
    /// `useState` setters and the state each sets, as `setOpen` for `const [open, setOpen]`.
    setters: HashMap<String, String>,
}

impl Imports {
    fn new() -> Self {
        Self::default()
    }

    fn get(&self, name: &str) -> Option<&(String, Option<String>)> {
        self.modules.get(name)
    }

    fn contains_key(&self, name: &str) -> bool {
        self.modules.contains_key(name)
    }

    fn insert(&mut self, name: String, module: (String, Option<String>)) {
        self.modules.insert(name, module);
    }
}

/// The names an expression is one of, as `Grid` and `Rows` for `compact ? Grid : Rows`, when
/// every alternative is a name.
fn chosen_names(expression: &FlowExpression, names: &mut Vec<String>) -> bool {
    match &expression.kind {
        FlowExpressionKind::Identifier { name, .. } => {
            names.push(name.clone());
            true
        }
        FlowExpressionKind::Conditional {
            consequent,
            alternate,
            ..
        } => chosen_names(consequent, names) && chosen_names(alternate, names),
        FlowExpressionKind::Logical {
            left,
            right,
            operator: FlowLogicalOperator::Coalesce | FlowLogicalOperator::Or,
        } => chosen_names(left, names) && chosen_names(right, names),
        _ => false,
    }
}

fn local_imports(body: &ScopeBody<'_>) -> Imports {
    let mut imports = Imports::new();
    let mut found = |frames: &[Frame<'_>], _guards: &[Guard<'_>]| {
        let frame = frames.last().expect("visited frame");
        // `const [open, setOpen] = useState(...)`.
        if let (
            Role::Bind(FlowPattern {
                kind: FlowPatternKind::Array { elements },
                ..
            }),
            FlowExpressionKind::Call { callee, .. },
        ) = (&frame.role, &frame.expression.kind)
            && callee_text(callee).rsplit('.').next() == Some("useState")
            && let [Some(state), Some(setter), ..] = elements.as_slice()
            && let (
                FlowPatternKind::Identifier { name: state },
                FlowPatternKind::Identifier { name: setter },
            ) = (&state.kind, &setter.kind)
        {
            imports.setters.insert(setter.clone(), state.clone());
            return;
        }
        if let Role::Bind(FlowPattern {
            kind: FlowPatternKind::Identifier { name },
            ..
        }) = &frame.role
            && matches!(
                frame.expression.kind,
                FlowExpressionKind::Conditional { .. } | FlowExpressionKind::Logical { .. }
            )
        {
            let mut names = Vec::new();
            if chosen_names(frame.expression, &mut names) {
                names.retain(|chosen| chosen != name);
                imports.choices.insert(name.clone(), names);
            }
            return;
        }
        // `const Panel = require('./Panel').default` binds the export.
        if let (
            Role::Bind(FlowPattern {
                kind: FlowPatternKind::Identifier { name },
                ..
            }),
            FlowExpressionKind::StaticMember { object, property },
        ) = (&frame.role, &frame.expression.kind)
            && let FlowExpressionKind::DynamicImport { module } = &object.kind
        {
            imports.insert(name.clone(), (module.clone(), Some(property.clone())));
            return;
        }
        let (Role::Bind(pattern), FlowExpressionKind::DynamicImport { module }) =
            (&frame.role, &frame.expression.kind)
        else {
            return;
        };
        match &pattern.kind {
            FlowPatternKind::Identifier { name } => {
                imports.insert(name.clone(), (module.clone(), None));
            }
            // `const { default: Panel } = await import('./Panel')` binds the export itself.
            FlowPatternKind::Object { fields, .. } => {
                for field in fields {
                    let target = match &field.target.kind {
                        FlowPatternKind::Default { target, .. } => target,
                        _ => &field.target,
                    };
                    if let FlowPatternKind::Identifier { name } = &target.kind {
                        imports.insert(
                            name.clone(),
                            (module.clone(), Some(field.source_property.clone())),
                        );
                    }
                }
            }
            _ => {}
        }
    };
    visit_body(body, &mut found);
    imports
}

/// The module and export an argument loads: `import('./Panel')`, a local bound to one or to one
/// of its exports, or a loader such as `() => import('./Panel')`.
fn imported_module(argument: &FlowExpression, imports: &Imports) -> Option<(String, String)> {
    match &argument.kind {
        FlowExpressionKind::DynamicImport { module } => {
            Some((module.clone(), "default".to_owned()))
        }
        FlowExpressionKind::Identifier { name, .. } => imports.get(name).map(|(module, export)| {
            (
                module.clone(),
                export.clone().unwrap_or_else(|| "default".to_owned()),
            )
        }),
        FlowExpressionKind::Arrow { .. } => loaded_module(argument),
        _ => None,
    }
}

/// The properties a selector reads from the state it is given, as `[key]` for
/// `(state) => state.key`.
fn selector_path(selector: &FlowExpression) -> Option<Vec<Step>> {
    let FlowExpressionKind::Arrow { params, body } = &selector.kind else {
        return None;
    };
    let [
        FlowPattern {
            kind: FlowPatternKind::Identifier { name: state },
            ..
        },
    ] = params.as_slice()
    else {
        return None;
    };
    let returned = match body {
        FlowArrowBody::Expression { expression } => expression.as_ref(),
        FlowArrowBody::Statements { statements } => match statements.as_slice() {
            [
                FlowStatement::Return {
                    value: Some(value), ..
                },
            ] => value,
            _ => return None,
        },
    };
    let (root, path) = read_path(returned)?;
    (&root == state).then_some(path)
}

/// A name and the properties read from it, as in `props.children` or `ref.current`.
fn read_path(expression: &FlowExpression) -> Option<(String, Vec<Step>)> {
    match &expression.kind {
        FlowExpressionKind::Identifier { name, .. } => Some((name.clone(), Vec::new())),
        FlowExpressionKind::StaticMember { object, property } => {
            let (root, mut path) = read_path(object)?;
            path.push(property.clone());
            Some((root, path))
        }
        _ => None,
    }
}

/// What a condition says about compared values: one comparison of a set holds, or none does.
enum Comparison<'e> {
    OneOf(Vec<(&'e FlowExpression, &'e FlowExpression)>),
    NoneOf(Vec<(&'e FlowExpression, &'e FlowExpression)>),
}

/// The comparisons a condition implies, all of which hold together.
fn comparison_sets(test: &FlowExpression, holds: bool) -> Vec<Comparison<'_>> {
    match &test.kind {
        FlowExpressionKind::StrictEquality {
            left,
            right,
            negated,
        } => {
            let pair = vec![(left.as_ref(), right.as_ref())];
            if holds == *negated {
                vec![Comparison::NoneOf(pair)]
            } else {
                vec![Comparison::OneOf(pair)]
            }
        }
        FlowExpressionKind::LogicalNot { value } => comparison_sets(value, !holds),
        FlowExpressionKind::Logical {
            left,
            right,
            operator,
        } => match (operator, holds) {
            // Both sides hold, or both fail.
            (FlowLogicalOperator::And, true) | (FlowLogicalOperator::Or, false) => {
                let mut left = comparison_sets(left, holds);
                left.extend(comparison_sets(right, holds));
                left
            }
            // One side holds, or one fails: only equalities on both sides combine.
            (FlowLogicalOperator::Or, true) | (FlowLogicalOperator::And, false) => {
                match (
                    comparison_sets(left, holds).as_slice(),
                    comparison_sets(right, holds).as_slice(),
                ) {
                    ([Comparison::OneOf(left)], [Comparison::OneOf(right)]) => {
                        vec![Comparison::OneOf(
                            left.iter().chain(right).copied().collect(),
                        )]
                    }
                    _ => Vec::new(),
                }
            }
            (FlowLogicalOperator::Coalesce, _) => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Whether a condition's values are of the enum the elements are, so it says which of them can
/// reach the code.
fn guard_applies(set: &[QueryValue], elements: &[QueryValue]) -> bool {
    let enum_of = |value: &QueryValue| match value {
        QueryValue::EnumMember { enum_name, .. } => Some(enum_name.clone()),
        _ => None,
    };
    let kinds = elements.iter().filter_map(enum_of).collect::<BTreeSet<_>>();
    !set.is_empty()
        && set
            .iter()
            .all(|value| enum_of(value).is_some_and(|name| kinds.contains(&name)))
}

/// Calls found at one location through several paths, as one call with the union of their
/// values.
fn merge_calls(calls: Vec<QueryCapabilityCall>) -> Vec<QueryCapabilityCall> {
    let mut merged: Vec<QueryCapabilityCall> = Vec::new();
    for call in calls {
        let Some(existing) = merged
            .iter_mut()
            .find(|existing| existing.location == call.location)
        else {
            merged.push(call);
            continue;
        };
        let union = |into: &mut BTreeMap<String, Vec<QueryValue>>,
                     from: BTreeMap<String, Vec<QueryValue>>| {
            for (label, values) in from {
                let into = into.entry(label).or_default();
                for value in values {
                    if !into.contains(&value) {
                        into.push(value);
                    }
                }
            }
        };
        union(&mut existing.arguments, call.arguments);
        union(&mut existing.elements, call.elements);
        // Only conditions that hold on every path to the call describe it.
        existing.guards.retain(|set| call.guards.contains(set));
        // A value one path rules out may reach the call through another.
        existing
            .guards_not
            .retain(|value| call.guards_not.contains(value));
        existing.arguments_resolved &= call.arguments_resolved;
        existing.elements_complete &= call.elements_complete;
        existing.explored |= call.explored;
        if existing.instance != call.instance {
            existing.instance = None;
        }
    }
    merged
}

/// Orders reachability from strongest to weakest.
fn rank(reachability: Reachability) -> u8 {
    match reachability {
        Reachability::Reachable => 0,
        Reachability::Possible => 1,
        Reachability::Declared => 2,
        Reachability::Unknown => 3,
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
