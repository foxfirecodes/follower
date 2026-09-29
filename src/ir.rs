use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ids::{BlockId, FileId, FunctionId, SymbolId};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
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
    pub globals: Vec<FlowBinding>,
    pub functions: Vec<FlowFunction>,
    pub unsupported: Vec<UnsupportedIr>,
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
    Unsupported(UnsupportedIr),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FlowPattern {
    pub kind: FlowPatternKind,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowPatternKind {
    Identifier { name: String },
    Object { fields: Vec<FlowPatternField> },
    Unsupported { syntax: String },
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
    String {
        value: String,
    },
    Identifier {
        name: String,
    },
    Record {
        fields: Vec<FlowRecordField>,
    },
    Array {
        elements: Vec<FlowExpression>,
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
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FlowRecordField {
    pub property: String,
    pub value: FlowExpression,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowArrowBody {
    Expression { expression: Box<FlowExpression> },
    Statements { statements: Vec<FlowStatement> },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowJsxTag {
    Identifier { name: String, intrinsic: bool },
    Unsupported { syntax: String },
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
