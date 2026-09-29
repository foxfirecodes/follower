use serde::{Deserialize, Serialize};

use crate::{evidence::Evidence, ir::SourceSpan};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Conclusion {
    CandidateInvocation,
    AbsentWithinModel,
    Unresolved,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FindingRef {
    pub finding_id: String,
    pub summary: String,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuditFinding {
    pub finding_id: String,
    pub key: String,
    pub factory_callsite: SourceSpan,
    pub choice: String,
    pub origins: Vec<FindingRef>,
    pub registrations: Vec<FindingRef>,
    pub invocations: Vec<FindingRef>,
    pub unresolved: Vec<FindingRef>,
    pub assumptions: Vec<String>,
    pub conclusion: Conclusion,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Coverage {
    pub scope: String,
    pub roots: Vec<String>,
    pub processed_files: usize,
    pub complete: bool,
    pub gaps: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuditReport {
    pub schema_version: u32,
    pub snapshot_id: String,
    pub config_hash: String,
    pub model_hash: String,
    pub model_id: String,
    pub findings: Vec<AuditFinding>,
    pub evidence: Vec<Evidence>,
    pub coverage: Coverage,
    pub diagnostics: Vec<String>,
}
