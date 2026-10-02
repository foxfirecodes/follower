use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::Path,
};

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
#[allow(clippy::struct_excessive_bools)]
pub struct QueryReportOptions {
    #[serde(default = "default_true")]
    pub include_non_invoked: bool,
    #[serde(default = "default_true")]
    pub include_registrations: bool,
    #[serde(default = "default_true")]
    pub include_unresolved_escapes: bool,
    #[serde(default)]
    pub html: bool,
}

impl Default for QueryReportOptions {
    fn default() -> Self {
        Self {
            include_non_invoked: true,
            include_registrations: true,
            include_unresolved_escapes: true,
            html: false,
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
    /// Evaluation steps allowed per entry input below components the model cannot follow. More
    /// steps explore more possible paths; exact paths do not use this budget.
    #[serde(default)]
    pub assumed_render_steps: Option<usize>,
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reachability {
    Reachable,
    /// Reached from a configured root only if unmodeled components render the path.
    Possible,
    Unknown,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
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
    #[serde(default)]
    pub call_path: Vec<QueryCallPathStep>,
    pub arguments: BTreeMap<String, QueryValue>,
    #[serde(default)]
    pub argument_evidence: BTreeMap<String, Option<String>>,
    /// Other explored paths that reach the same invocation with the same argument values;
    /// `call_path` shows the first.
    #[serde(default)]
    pub other_paths: usize,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryCallPathKind {
    Entry,
    Call,
    Render,
    ModeledRender,
    /// A render through a component whose behavior is not modeled.
    AssumedRender,
    /// A callback that carries the factory result and that nothing in the model called, such as
    /// an event handler, run because it may be.
    UncalledCallback,
    Factory,
    Invocation,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueryCallPathStep {
    pub kind: QueryCallPathKind,
    pub location: QueryLocation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryReverseImporterEvaluation {
    Function,
    DirectImportUse,
    ModuleBinding,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueryReverseImporter {
    pub symbol: String,
    pub matched_imports: Vec<String>,
    pub evaluation: QueryReverseImporterEvaluation,
    pub span: SourceSpan,
    pub location: Option<QueryLocation>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueryCreation {
    pub creation_id: String,
    pub factory_callsite: SourceSpan,
    pub factory_location: Option<QueryLocation>,
    pub reachability: Reachability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverse_importer: Option<QueryReverseImporter>,
    pub choice: String,
    pub factory_arguments: BTreeMap<String, QueryValue>,
    #[serde(default)]
    pub factory_argument_evidence: BTreeMap<String, Option<String>>,
    pub capability_path: Vec<String>,
    pub registrations: Vec<FindingRef>,
    pub invocations: Vec<QueryInvocation>,
    pub unresolved: Vec<FindingRef>,
    #[serde(default)]
    pub unresolved_count: usize,
    #[serde(skip)]
    pub(crate) all_unresolved_evidence_ids: Vec<String>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryGapAssessment {
    Direct,
    MayAffect,
    UnknownRelevance,
    Unlinked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryGapTarget {
    Creation,
    FactoryArgument,
    InvocationArgument,
    Callsite,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueryGapLink {
    pub target: QueryGapTarget,
    pub assessment: QueryGapAssessment,
    pub creation_id: Option<String>,
    pub label: Option<String>,
    pub invocation_evidence_id: Option<String>,
    pub callsite_index: Option<usize>,
    pub evidence_path: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueryGap {
    pub gap_id: String,
    pub kind: String,
    pub summary: String,
    pub span: Option<SourceSpan>,
    pub location: Option<QueryLocation>,
    pub choice: Option<String>,
    pub assessment: QueryGapAssessment,
    pub links: Vec<QueryGapLink>,
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
    /// One entry per matching factory callsite: the values it was called with and the calls
    /// made with its result, whether or not a path from a configured root reaches it.
    #[serde(default)]
    pub callsites: Vec<QueryCallsiteValues>,
    pub creations: Vec<QueryCreation>,
    pub callsite_inventory: QueryCallsiteInventory,
    pub evidence: Vec<Evidence>,
    #[serde(default)]
    pub gaps: Vec<QueryGap>,
    /// Components on paths from configured roots that the model could not follow, most
    /// consequential first. Each is a place where a contract, a larger parse, or engine support
    /// can turn possible creations into reachable ones.
    #[serde(default)]
    pub component_boundaries: Vec<QueryComponentBoundary>,
    /// Factory callsites no exact or possible path from a configured root reached, with where
    /// the nearest explored code stopped short of them.
    #[serde(default)]
    pub unreached_callsites: Vec<QueryUnreachedCallsite>,
    pub coverage: Coverage,
    pub diagnostics: Vec<String>,
}

/// A factory callsite with the values it was called with and what its result was called with.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueryCallsiteValues {
    pub location: Option<QueryLocation>,
    /// The function or module binding that contains the callsite.
    pub enclosing: Option<String>,
    /// The strongest reachability among the callsite's creations; a callsite no path from a
    /// configured root reached is `unknown`. Values are reported either way.
    pub reachability: Reachability,
    /// Why no path reached the callsite, when none did.
    #[serde(default)]
    pub unreached_reason: Option<QueryUnreachedReason>,
    /// Explored creation contexts at the callsite.
    pub contexts: usize,
    /// The distinct values of each projected factory argument across contexts.
    pub factory_arguments: BTreeMap<String, Vec<QueryValue>>,
    pub factory_arguments_resolved: bool,
    pub capability: QueryCapabilityUse,
}

/// What became of the selected factory result: the calls made with it and where it went that
/// the walk could not follow.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueryCapabilityUse {
    pub status: QueryCapabilityStatus,
    pub calls: Vec<QueryCapabilityCall>,
    pub escapes: Vec<QueryCapabilityEscape>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryCapabilityStatus {
    /// Calls were found, and every projected argument at them is known.
    Called,
    /// Calls were found, but some projected arguments are unknown.
    CalledWithUnknownArguments,
    /// No call was found, and the result leaves code the walk follows; see `escapes`.
    Escapes,
    /// The result is used, for example compared or checked, but never called.
    NotCalled,
    /// The callsite does not bind the result, or binds it and never uses it.
    Unused,
}

/// A call made with the factory result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueryCapabilityCall {
    pub location: Option<QueryLocation>,
    /// The functions the call sits in, innermost first, such as `the onClose prop of <Panel>`.
    /// Empty when the call is directly in the code the result reached.
    pub context: Vec<String>,
    /// How the result reached the call from the factory callsite, such as `prop onClose of
    /// <Panel>` or `returned by useNotice to Banner`.
    pub via: Vec<String>,
    /// The distinct values of each projected invocation argument.
    pub arguments: BTreeMap<String, Vec<QueryValue>>,
    pub arguments_resolved: bool,
    /// Whether an explored path executed this call. A call no path executed may be behind a
    /// condition, a handler, or a component no path rendered; its arguments are still reported.
    pub explored: bool,
}

/// Somewhere the factory result went that the walk could not follow.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueryCapabilityEscape {
    pub location: Option<QueryLocation>,
    pub detail: String,
    pub context: Vec<String>,
    pub via: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryUnreachedReason {
    /// The use that leads to the callsite is inside a callback passed to a call, such as a modal
    /// opener or an effect, which exact exploration did not invoke.
    Callback,
    /// The use is inside a function passed as a JSX prop, such as an event handler or render
    /// prop, that exact exploration did not invoke.
    PropCallback,
    /// The use is inside a local function that exact exploration did not call.
    LocalCallback,
    /// The use is a dynamic `import()` the model does not follow.
    LazyImport,
    /// The render visit budget stopped exact exploration at the use.
    RenderBudget,
    /// The use was rendered, but the component's body was not explored, for example because a
    /// contract models it or its value is unknown.
    ComponentNotFollowed,
    /// The JSX at the use was created but never rendered.
    CreatedNotRendered,
    /// The explored function did not evaluate the use: a decided condition, an early return,
    /// or a path cut short.
    BranchNotTaken,
    /// No user of the callsite's code was explored exactly within the parsed files.
    NoExploredAncestor,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueryUnreachedCallsite {
    pub location: Option<QueryLocation>,
    /// The function or module binding that contains the callsite.
    pub enclosing: Option<String>,
    pub reason: QueryUnreachedReason,
    pub detail: String,
    /// The nearest user explored exactly, and the use inside it that was not followed.
    pub explored_ancestor: Option<String>,
    pub blocking_site: Option<QueryLocation>,
    /// Code between the callsite and the explored ancestor, innermost first.
    pub chain: Vec<String>,
    /// A project contract that would let exploration follow the use, when one applies.
    #[serde(default)]
    pub suggested_contract: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryBoundaryKind {
    /// Linkage ends in a package that is not parsed, such as one under `node_modules`.
    ExternalPackage,
    /// Linkage ends in a project source file that was not parsed, for example because of the
    /// text filter or an expansion budget.
    UnparsedSource,
    /// A relative or aliased import did not resolve.
    UnresolvedImport,
    /// The component is linked but its value is not modeled, such as the result of an unknown
    /// higher-order component call.
    DynamicValue,
    /// A parsed component did not render JSX it received after reaching an unmodeled operation.
    PartiallyModeled,
    /// JSX was handed to an unmodeled call or unsupported expression.
    EscapedJsx,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct QueryComponentBoundary {
    pub boundary_id: String,
    pub kind: QueryBoundaryKind,
    /// The JSX tag or call as written at the first site.
    pub component: String,
    /// Module specifier and export where linkage stops, as written there; `.member` follows a
    /// member tag.
    pub module: Option<String>,
    pub export: Option<String>,
    pub reason: String,
    /// Whether an exact path from a configured root reaches the boundary, so modeling it can make
    /// creations below it reachable.
    pub entered_from_reachable: bool,
    pub site_count: usize,
    pub sites: Vec<QueryLocation>,
    /// Reported creations that are possible only through this boundary and possibly others.
    pub affected_creations: usize,
    /// Reported creations whose only assumption is this boundary.
    pub sole_blocker_creations: usize,
    /// A project contract that would model the boundary, when one applies. Contracts are
    /// assumptions about library behavior; check the component before adding one.
    pub suggested_contract: Option<String>,
}

impl QueryGap {
    /// Create a gap with an ID stable for its source and context.
    ///
    /// # Panics
    ///
    /// Panics only if serializing these owned scalar fields unexpectedly fails.
    pub fn new(
        kind: String,
        summary: String,
        span: Option<SourceSpan>,
        location: Option<QueryLocation>,
        choice: Option<String>,
    ) -> Self {
        let identity = serde_json::to_vec(&(&kind, &summary, &span, &location, &choice))
            .expect("gap identity is serializable");
        let hash = hex::encode(Sha256::digest(identity));
        Self {
            gap_id: format!("G{}", &hash[..16]),
            kind,
            summary,
            span,
            location,
            choice,
            assessment: QueryGapAssessment::UnknownRelevance,
            links: Vec::new(),
        }
    }
}

impl QueryReport {
    /// Keeps the evidence the report refers to and its ancestors up to the evidence path limit,
    /// within an overall bound. The rest of the solver's evaluation steps are dropped so the
    /// report stays a usable size.
    pub fn prune_evidence(&mut self) {
        let mut referenced = Vec::<u32>::new();
        let mut add = |id: &str| referenced.extend(evidence_number(id));
        for creation in &self.creations {
            creation
                .factory_argument_evidence
                .values()
                .flatten()
                .for_each(|id| add(id));
            creation
                .registrations
                .iter()
                .for_each(|finding| add(&finding.finding_id));
            creation
                .unresolved
                .iter()
                .for_each(|finding| add(&finding.finding_id));
            creation
                .all_unresolved_evidence_ids
                .iter()
                .for_each(|id| add(id));
            for invocation in &creation.invocations {
                add(&invocation.evidence_id);
                invocation
                    .argument_evidence
                    .values()
                    .flatten()
                    .for_each(|id| add(id));
            }
        }
        for gap in &self.gaps {
            for link in &gap.links {
                link.evidence_path.iter().for_each(|id| add(id));
                link.invocation_evidence_id.iter().for_each(|id| add(id));
            }
        }
        // Mutations explain aliasing that provenance links do not always reach, and are rare.
        referenced.extend(
            self.evidence
                .iter()
                .filter(|node| node.relation == crate::evidence::RelationKind::Mutation)
                .map(|node| node.id.0),
        );
        // Ancestors within the evidence path limit, nearest first, up to an overall bound.
        let mut keep = BTreeSet::new();
        let mut frontier = referenced
            .into_iter()
            .map(|number| (number, 0))
            .collect::<VecDeque<_>>();
        while let Some((number, depth)) = frontier.pop_front() {
            if keep.len() >= KEPT_EVIDENCE_LIMIT || !keep.insert(number) {
                continue;
            }
            if depth < EVIDENCE_PATH_LIMIT
                && let Some(node) = evidence_by_number(&self.evidence, number)
            {
                frontier.extend(node.parents.iter().map(|parent| (parent.0, depth + 1)));
            }
        }
        self.evidence.retain(|node| keep.contains(&node.id.0));
    }

    pub fn finish_gaps(&mut self) {
        let mut seen_summaries = self
            .gaps
            .iter()
            .map(|gap| gap.summary.clone())
            .collect::<BTreeSet<_>>();
        for summary in &self.coverage.gaps {
            if seen_summaries.insert(summary.clone()) {
                self.gaps.push(QueryGap::new(
                    "analysis_gap".to_owned(),
                    summary.clone(),
                    None,
                    None,
                    None,
                ));
            }
        }
        for (index, callsite) in self.callsite_inventory.callsites.iter().enumerate() {
            let kind = match callsite.status {
                QueryCallsiteStatus::Analyzed | QueryCallsiteStatus::Unresolved => continue,
                QueryCallsiteStatus::Filtered => "filtered_callsite",
                QueryCallsiteStatus::Skipped => "skipped_callsite",
            };
            let mut gap = QueryGap::new(
                kind.to_owned(),
                callsite.reason.clone(),
                None,
                Some(callsite.location.clone()),
                None,
            );
            let assessment = match callsite.status {
                QueryCallsiteStatus::Filtered | QueryCallsiteStatus::Skipped => {
                    QueryGapAssessment::MayAffect
                }
                QueryCallsiteStatus::Unresolved | QueryCallsiteStatus::Analyzed => unreachable!(),
            };
            gap.assessment = assessment;
            gap.links.push(QueryGapLink {
                target: QueryGapTarget::Callsite,
                assessment,
                creation_id: None,
                label: None,
                invocation_evidence_id: None,
                callsite_index: Some(index),
                evidence_path: Vec::new(),
            });
            self.gaps.push(gap);
        }
        let mut creations_by_span: BTreeMap<(u32, u32, u32), BTreeSet<usize>> = BTreeMap::new();
        for (index, creation) in self.creations.iter().enumerate() {
            creations_by_span
                .entry(span_key(&creation.factory_callsite))
                .or_default()
                .insert(index);
            for id in &creation.all_unresolved_evidence_ids {
                if let Some(evidence) = evidence_for_id(&self.evidence, id) {
                    creations_by_span
                        .entry(span_key(&evidence.span))
                        .or_default()
                        .insert(index);
                }
            }
            for invocation in &creation.invocations {
                creations_by_span
                    .entry(span_key(&invocation.callsite))
                    .or_default()
                    .insert(index);
            }
        }
        for gap in &mut self.gaps {
            if !gap.links.is_empty() {
                continue;
            }
            let candidates = gap
                .span
                .as_ref()
                .and_then(|span| creations_by_span.get(&span_key(span)));
            for creation in candidates
                .into_iter()
                .flatten()
                .map(|&index| &self.creations[index])
            {
                let matching = creation.all_unresolved_evidence_ids.iter().find(|id| {
                    evidence_for_id(&self.evidence, id)
                        .is_some_and(|evidence| gap.span.as_ref() == Some(&evidence.span))
                });
                if let Some(id) = matching {
                    gap.links.push(QueryGapLink {
                        target: QueryGapTarget::Creation,
                        assessment: QueryGapAssessment::Direct,
                        creation_id: Some(creation.creation_id.clone()),
                        label: None,
                        invocation_evidence_id: None,
                        callsite_index: None,
                        evidence_path: evidence_path(&self.evidence, id),
                    });
                } else if gap.kind == "factory_call_not_reached"
                    && gap.span.as_ref() == Some(&creation.factory_callsite)
                {
                    gap.links.push(QueryGapLink {
                        target: QueryGapTarget::Creation,
                        assessment: QueryGapAssessment::Direct,
                        creation_id: Some(creation.creation_id.clone()),
                        label: None,
                        invocation_evidence_id: None,
                        callsite_index: None,
                        evidence_path: Vec::new(),
                    });
                }
                if gap.kind == "uncertain_callback_factory_capture"
                    && gap.span.as_ref() == Some(&creation.factory_callsite)
                {
                    for (label, value) in &creation.factory_arguments {
                        if query_value_uncertain(value) {
                            gap.links.push(QueryGapLink {
                                target: QueryGapTarget::FactoryArgument,
                                assessment: QueryGapAssessment::Direct,
                                creation_id: Some(creation.creation_id.clone()),
                                label: Some(label.clone()),
                                invocation_evidence_id: None,
                                callsite_index: None,
                                evidence_path: creation
                                    .factory_argument_evidence
                                    .get(label)
                                    .and_then(|id| id.as_deref())
                                    .map_or_else(Vec::new, |id| evidence_path(&self.evidence, id)),
                            });
                        }
                    }
                }
                for invocation in &creation.invocations {
                    if gap.span.as_ref() == Some(&invocation.callsite) && matching.is_none() {
                        for (label, value) in &invocation.arguments {
                            if query_value_uncertain(value) {
                                gap.links.push(QueryGapLink {
                                    target: QueryGapTarget::InvocationArgument,
                                    assessment: QueryGapAssessment::MayAffect,
                                    creation_id: Some(creation.creation_id.clone()),
                                    label: Some(label.clone()),
                                    invocation_evidence_id: Some(invocation.evidence_id.clone()),
                                    callsite_index: None,
                                    evidence_path: invocation
                                        .argument_evidence
                                        .get(label)
                                        .and_then(|id| id.as_deref())
                                        .map_or_else(Vec::new, |id| {
                                            evidence_path(&self.evidence, id)
                                        }),
                                });
                            }
                        }
                    }
                }
            }
            gap.assessment = if gap
                .links
                .iter()
                .any(|link| link.assessment == QueryGapAssessment::Direct)
            {
                QueryGapAssessment::Direct
            } else if !gap.links.is_empty() {
                QueryGapAssessment::MayAffect
            } else if gap.location.is_some() {
                QueryGapAssessment::UnknownRelevance
            } else {
                QueryGapAssessment::Unlinked
            };
        }
        self.gaps
            .sort_by(|left, right| left.gap_id.cmp(&right.gap_id));
        self.gaps
            .dedup_by(|left, right| left.gap_id == right.gap_id);
    }
}

fn query_value_uncertain(value: &QueryValue) -> bool {
    match value {
        QueryValue::Unknown { .. } => true,
        QueryValue::Alternatives { values } => values.iter().any(query_value_uncertain),
        QueryValue::Array { elements } => elements.iter().any(query_value_uncertain),
        _ => false,
    }
}

fn span_key(span: &SourceSpan) -> (u32, u32, u32) {
    (span.file_id.0, span.start, span.end)
}

fn evidence_path(evidence: &[Evidence], id: &str) -> Vec<String> {
    let mut path = Vec::new();
    let mut current = evidence_number(id);
    while let Some(number) = current {
        let Some(node) = evidence_by_number(evidence, number) else {
            break;
        };
        path.push(format!("E{}", node.id.0));
        if path.len() >= EVIDENCE_PATH_LIMIT {
            break;
        }
        current = node.parents.first().map(|parent| parent.0);
    }
    path
}

const EVIDENCE_PATH_LIMIT: usize = 64;
const KEPT_EVIDENCE_LIMIT: usize = 250_000;

fn evidence_number(id: &str) -> Option<u32> {
    id.strip_prefix('E')
        .and_then(|digits| digits.parse::<u32>().ok())
}

/// Evidence is kept in ID order, including after pruning.
fn evidence_by_number(evidence: &[Evidence], number: u32) -> Option<&Evidence> {
    evidence
        .binary_search_by_key(&number, |node| node.id.0)
        .ok()
        .map(|index| &evidence[index])
}

fn evidence_for_id<'a>(evidence: &'a [Evidence], id: &str) -> Option<&'a Evidence> {
    evidence_number(id).and_then(|number| evidence_by_number(evidence, number))
}
