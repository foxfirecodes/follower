use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    evidence::Evidence,
    ir::SourceSpan,
    models::SymbolMatcher,
    queries::{Conclusion, Coverage, FindingRef},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryKind {
    FactoryReturnInvocations,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryScope {
    Reachable,
    AllCreations,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArgumentProjection {
    pub index: usize,
    pub label: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityQuery {
    #[serde(default)]
    pub returned_property: Vec<String>,
    #[serde(default)]
    pub returned_index: Option<usize>,
    #[serde(default)]
    pub invocation_arguments: Vec<ArgumentProjection>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryReportOptions {
    #[serde(default = "default_true")]
    pub include_non_invoked: bool,
    #[serde(default = "default_true")]
    pub include_registrations: bool,
    #[serde(default = "default_true")]
    pub include_unresolved_escapes: bool,
}

impl Default for QueryReportOptions {
    fn default() -> Self {
        Self {
            include_non_invoked: true,
            include_registrations: true,
            include_unresolved_escapes: true,
        }
    }
}

const fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuerySpec {
    pub schema_version: u32,
    pub id: String,
    pub kind: QueryKind,
    pub scope: QueryScope,
    #[serde(default)]
    pub scan_callback_bodies: bool,
    pub factory: SymbolMatcher,
    #[serde(default)]
    pub factory_arguments: Vec<ArgumentProjection>,
    pub capability: CapabilityQuery,
    #[serde(default)]
    pub report: QueryReportOptions,
}

impl QuerySpec {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            bail!(
                "unsupported query schema version {}; expected 1",
                self.schema_version
            );
        }
        if self.id.trim().is_empty() {
            bail!("query id cannot be empty");
        }
        if self.capability.returned_property.is_empty() == self.capability.returned_index.is_none()
        {
            bail!("capability must select exactly one of returned_property or returned_index");
        }
        validate_projections("factory_arguments", &self.factory_arguments)?;
        validate_projections(
            "capability.invocation_arguments",
            &self.capability.invocation_arguments,
        )
    }
}

fn validate_projections(name: &str, projections: &[ArgumentProjection]) -> Result<()> {
    let mut labels = std::collections::BTreeSet::new();
    let mut indices = std::collections::BTreeSet::new();
    for projection in projections {
        if projection.label.trim().is_empty() {
            bail!("{name} contains an empty label");
        }
        if !labels.insert(&projection.label) {
            bail!("{name} contains duplicate label {}", projection.label);
        }
        if !indices.insert(projection.index) {
            bail!(
                "{name} contains duplicate argument index {}",
                projection.index
            );
        }
    }
    Ok(())
}

pub fn load_query(path: &Path) -> Result<(QuerySpec, String)> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("failed to read query file {}", path.display()))?;
    let query: QuerySpec = toml::from_str(&source)
        .with_context(|| format!("failed to parse query file {}", path.display()))?;
    query.validate()?;
    let hash = hex::encode(Sha256::digest(source.as_bytes()));
    Ok((query, hash))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reachability {
    Reachable,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QueryValue {
    Null,
    String {
        value: String,
    },
    Number {
        value: i64,
    },
    EnumMember {
        enum_name: String,
        member_name: String,
        value: i64,
    },
    Boolean {
        value: bool,
    },
    Alternatives {
        values: Vec<QueryValue>,
    },
    Array {
        elements: Vec<QueryValue>,
    },
    Undefined,
    Unknown {
        reason: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueryLocation {
    pub path: String,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueryInvocation {
    pub evidence_id: String,
    pub callsite: SourceSpan,
    pub location: Option<QueryLocation>,
    pub arguments: BTreeMap<String, QueryValue>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueryCreation {
    pub creation_id: String,
    pub factory_callsite: SourceSpan,
    pub factory_location: Option<QueryLocation>,
    pub reachability: Reachability,
    pub choice: String,
    pub factory_arguments: BTreeMap<String, QueryValue>,
    pub capability_path: Vec<String>,
    pub registrations: Vec<FindingRef>,
    pub invocations: Vec<QueryInvocation>,
    pub unresolved: Vec<FindingRef>,
    pub conclusion: Conclusion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryCallsiteStatus {
    Analyzed,
    Filtered,
    Unresolved,
    Skipped,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueryCallsite {
    pub location: QueryLocation,
    pub status: QueryCallsiteStatus,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueryCallsiteInventory {
    pub configured_files: usize,
    pub candidate_files: usize,
    pub skipped_candidate_files: usize,
    pub round_limit_hit: bool,
    pub callsites: Vec<QueryCallsite>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueryReport {
    pub schema_version: u32,
    pub snapshot_id: String,
    pub config_hash: String,
    pub query_hash: String,
    pub query_id: String,
    pub kind: QueryKind,
    pub scope: QueryScope,
    pub creations: Vec<QueryCreation>,
    pub callsite_inventory: QueryCallsiteInventory,
    pub evidence: Vec<Evidence>,
    pub coverage: Coverage,
    pub diagnostics: Vec<String>,
}
