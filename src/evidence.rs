use serde::{Deserialize, Serialize};

use crate::{ids::EvidenceId, ir::SourceSpan};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    ImportReference,
    ValueTransfer,
    Derivation,
    KeySelection,
    Capture,
    Call,
    Return,
    RenderPropBinding,
    EventRegistration,
    Invocation,
    Effect,
    UnresolvedEscape,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub id: EvidenceId,
    pub relation: RelationKind,
    pub rule: String,
    pub span: SourceSpan,
    #[serde(default)]
    pub parents: Vec<EvidenceId>,
    #[serde(default)]
    pub choice: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
    pub summary: String,
}
