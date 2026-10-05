use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ids::{BlockId, FileId, FunctionId, SymbolId};

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct SourceSpan {
    pub file_id: FileId,
    pub start: u32,
    pub end: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OwnedSymbol {
    pub id: SymbolId,
    pub name: String,
    pub declaration: SourceSpan,
    pub reference_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OwnedFunction {
    pub id: FunctionId,
    pub name: Option<String>,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OwnedBlock {
    pub id: BlockId,
    pub instruction_count: usize,
    pub successor_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImportIr {
    pub specifier: String,
    pub span: SourceSpan,
    pub type_only: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EnumMemberIr {
    pub enum_name: String,
    pub member_name: String,
    pub string_value: Option<String>,
    pub span: SourceSpan,
}

/// All parser-owned data is copied into this structure before the Oxc arena is dropped.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileIr {
    pub file_id: FileId,
    pub path: PathBuf,
    pub content_hash: String,
    pub source_len: usize,
    pub symbols: Vec<OwnedSymbol>,
    pub functions: Vec<OwnedFunction>,
    pub blocks: Vec<OwnedBlock>,
    pub imports: Vec<ImportIr>,
    pub enum_members: Vec<EnumMemberIr>,
    pub flow: FlowFileIr,
    pub diagnostics: Vec<String>,
    pub cfg_enabled: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct FlowFileIr {
    pub imports: Vec<FlowImport>,
    #[serde(default)]
    pub exports: Vec<FlowExport>,
    pub globals: Vec<FlowBinding>,
    pub functions: Vec<FlowFunction>,
    pub unsupported: Vec<UnsupportedIr>,
    /// `defaultProps` declared in this module, from `static defaultProps` or
    /// `Component.defaultProps = ...`.
    #[serde(default)]
    pub default_props: Vec<FlowDefaultProps>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FlowDefaultProps {
    pub component: String,
    pub value: FlowExpression,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FlowImport {
    pub local: String,
    pub imported: String,
    pub module: String,
    pub type_only: bool,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowExport {
    Local {
        local: String,
        exported: String,
        type_only: bool,
        span: SourceSpan,
    },
    ReExport {
        imported: String,
        exported: String,
        module: String,
        type_only: bool,
        span: SourceSpan,
    },
    Star {
        module: String,
        type_only: bool,
        span: SourceSpan,
    },
    Namespace {
        exported: String,
        module: String,
        type_only: bool,
        span: SourceSpan,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FlowFunction {
    pub name: String,
    pub params: Vec<FlowPattern>,
    pub body: Vec<FlowStatement>,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FlowBinding {
    pub pattern: FlowPattern,
    pub value: FlowExpression,
    pub span: SourceSpan,
    #[serde(default)]
    pub lexical: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowStatement {
    Bind(FlowBinding),
    Return {
        value: Option<FlowExpression>,
        span: SourceSpan,
    },
    Expression {
        value: FlowExpression,
        span: SourceSpan,
    },
    Assign {
        target: FlowAssignmentTarget,
        value: FlowExpression,
        span: SourceSpan,
    },
    If {
        test: FlowExpression,
        consequent: Vec<FlowStatement>,
        alternate: Vec<FlowStatement>,
        span: SourceSpan,
    },
    /// Ends the path with an exception after evaluating the value.
    Throw {
        value: FlowExpression,
        span: SourceSpan,
    },
    Unsupported(UnsupportedIr),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowAssignmentTarget {
    Identifier {
        name: String,
    },
    StaticMember {
        object: FlowExpression,
        property: String,
    },
    ComputedMember {
        object: FlowExpression,
        property: FlowExpression,
    },
    Unsupported {
        syntax: String,
        span: SourceSpan,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FlowPattern {
    pub kind: FlowPatternKind,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowPatternKind {
    Identifier {
        name: String,
    },
    Object {
        fields: Vec<FlowPatternField>,
        #[serde(default)]
        rest: Option<Box<FlowPattern>>,
    },
    Array {
        elements: Vec<Option<FlowPattern>>,
    },
    /// A target with a default used when the value is `undefined`, as in `{ size = 'md' }` or a
    /// parameter default.
    Default {
        target: Box<FlowPattern>,
        default: Box<FlowExpression>,
    },
    Unsupported {
        syntax: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FlowPatternField {
    pub source_property: String,
    pub target: FlowPattern,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FlowExpression {
    pub kind: FlowExpressionKind,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowExpressionKind {
    Null,
    String {
        value: String,
    },
    Number {
        value: i64,
    },
    NumericEnumMember {
        enum_name: String,
        member_name: String,
        value: i64,
    },
    Boolean {
        value: bool,
    },
    Identifier {
        name: String,
        /// Whether Oxc bound this reference in the module scope.
        #[serde(default)]
        module_binding: bool,
    },
    Record {
        fields: Vec<FlowRecordField>,
    },
    Array {
        elements: Vec<FlowExpression>,
    },
    Spread {
        value: Box<FlowExpression>,
    },
    StaticMember {
        object: Box<FlowExpression>,
        property: String,
    },
    ComputedMember {
        object: Box<FlowExpression>,
        property: Box<FlowExpression>,
    },
    Call {
        callee: Box<FlowExpression>,
        arguments: Vec<FlowExpression>,
    },
    DynamicImport {
        module: String,
    },
    StrictEquality {
        left: Box<FlowExpression>,
        right: Box<FlowExpression>,
        negated: bool,
    },
    LooseNullEquality {
        value: Box<FlowExpression>,
        negated: bool,
    },
    Logical {
        left: Box<FlowExpression>,
        right: Box<FlowExpression>,
        operator: FlowLogicalOperator,
    },
    LogicalNot {
        value: Box<FlowExpression>,
    },
    Conditional {
        test: Box<FlowExpression>,
        consequent: Box<FlowExpression>,
        alternate: Box<FlowExpression>,
    },
    Arrow {
        params: Vec<FlowPattern>,
        body: FlowArrowBody,
    },
    JsxElement {
        tag: FlowJsxTag,
        props: Vec<FlowJsxProp>,
    },
    Unsupported {
        syntax: String,
        #[serde(default)]
        references: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowLogicalOperator {
    And,
    Or,
    Coalesce,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FlowRecordField {
    pub property: String,
    pub value: FlowExpression,
    pub span: SourceSpan,
    /// `...value` copies the value's properties at this point; `property` is unused.
    #[serde(default)]
    pub spread: bool,
    /// The key of `[key]: value`, evaluated to name the property; `property` is unused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computed: Option<FlowExpression>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowArrowBody {
    Expression { expression: Box<FlowExpression> },
    Statements { statements: Vec<FlowStatement> },
}

/// Recognize a configured lazy factory only when one record property holds a loader returning
/// a literal dynamic import: an arrow, or a function or arrow the file declares under that name,
/// as `load: importPanel`. The caller checks the factory symbol before using this
/// result.
pub fn lazy_component_import<'a>(
    file: &'a FlowFileIr,
    expression: &'a FlowExpression,
    promise_property: &str,
) -> Option<&'a str> {
    let FlowExpressionKind::Call { arguments, .. } = &expression.kind else {
        return None;
    };
    let FlowExpressionKind::Record { fields } = &arguments.first()?.kind else {
        return None;
    };
    let callback = &fields
        .iter()
        .find(|field| !field.spread && field.property == promise_property)?
        .value;
    let statements = match &callback.kind {
        FlowExpressionKind::Arrow { body, .. } => match body {
            FlowArrowBody::Expression { expression } => return imported_module(expression),
            FlowArrowBody::Statements { statements } => statements,
        },
        FlowExpressionKind::Identifier { name, .. } => {
            if let Some(function) = file
                .functions
                .iter()
                .find(|function| &function.name == name)
            {
                &function.body
            } else {
                let binding = file.globals.iter().find(|binding| {
                    matches!(&binding.pattern.kind, FlowPatternKind::Identifier { name: bound } if bound == name)
                })?;
                match &binding.value.kind {
                    FlowExpressionKind::Arrow {
                        body: FlowArrowBody::Expression { expression },
                        ..
                    } => return imported_module(expression),
                    FlowExpressionKind::Arrow {
                        body: FlowArrowBody::Statements { statements },
                        ..
                    } => statements,
                    _ => return None,
                }
            }
        }
        _ => return None,
    };
    let [
        FlowStatement::Return {
            value: Some(expression),
            ..
        },
    ] = statements.as_slice()
    else {
        return None;
    };
    imported_module(expression)
}

fn imported_module(expression: &FlowExpression) -> Option<&str> {
    match &expression.kind {
        FlowExpressionKind::DynamicImport { module } => Some(module),
        _ => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowJsxTag {
    Identifier {
        name: String,
        intrinsic: bool,
        #[serde(default)]
        module_binding: bool,
    },
    Member {
        object: String,
        property: String,
        module_binding: bool,
    },
    Unsupported {
        syntax: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowJsxProp {
    Property {
        name: String,
        value: FlowExpression,
        span: SourceSpan,
    },
    Spread {
        value: FlowExpression,
        span: SourceSpan,
    },
    Unsupported(UnsupportedIr),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UnsupportedIr {
    pub syntax: String,
    pub span: SourceSpan,
}
